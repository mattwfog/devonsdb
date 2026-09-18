//! Detach-delete overlay tests.

use std::sync::Arc;

use devondb_storage::{
    budget::MemoryBudget,
    catalog::Catalog,
    node_table::NodeTable,
    overlay::{
        COMMIT_LINK_OVERHEAD_BYTES, CommitDelta, CommitLink, MAP_ENTRY_OVERHEAD_BYTES, OverlayEdge,
        PublishedState, REL_ENDPOINT_TOMBSTONES_OVERHEAD_BYTES,
        REL_TOMBSTONE_OFFSET_OVERHEAD_BYTES, RelEndpointTombstones,
    },
    pager::Pager,
    rel_table::{Direction, RelTable},
};
use devondb_types::{
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema, RelTableSchema},
    value::Value,
};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"detach-overlay!!";

fn person_row(id: i64, name: &str) -> Vec<Value> {
    vec![Value::Int64(id), Value::String(name.to_owned())]
}

fn person_delta(inserts: &[(i64, &str)], deletes: &[i64]) -> CommitDelta {
    let mut delta = CommitDelta::default();
    if !inserts.is_empty() {
        delta.nodes.insert(
            "Person".to_owned(),
            inserts
                .iter()
                .map(|(id, name)| person_row(*id, name))
                .collect(),
        );
    }
    if !deletes.is_empty() {
        delta.node_deletes.insert(
            "Person".to_owned(),
            deletes.iter().copied().map(Value::Int64).collect(),
        );
    }
    delta
}

fn person_catalog() -> Catalog {
    let mut catalog = Catalog::default();
    catalog
        .add_node_table(
            NodeTableSchema::new(
                "Person".to_owned(),
                vec![
                    Column {
                        name: "id".to_owned(),
                        ty: LogicalType::Int64,
                        primary_key: true,
                    },
                    Column {
                        name: "name".to_owned(),
                        ty: LogicalType::String,
                        primary_key: false,
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
    catalog.add_rel_table(rel_schema("Knows")).unwrap();
    catalog
}

fn rel_schema(name: &str) -> RelTableSchema {
    RelTableSchema::new(
        name.to_owned(),
        "Person".to_owned(),
        "Person".to_owned(),
        vec![Column {
            name: "since".to_owned(),
            ty: LogicalType::Int64,
            primary_key: false,
        }],
    )
    .unwrap()
}

fn link(
    previous: Option<Arc<CommitLink>>,
    lsn: u64,
    delta: CommitDelta,
    budget: &Arc<MemoryBudget>,
) -> Arc<CommitLink> {
    CommitLink::new_arc(previous, lsn, delta, Arc::clone(budget)).unwrap()
}

fn state(chain: Arc<CommitLink>) -> PublishedState {
    PublishedState {
        catalog: Arc::new(person_catalog()),
        last_commit_lsn: chain.commit_lsn,
        chain: Some(chain),
        catalog_generation: 0,
        recent_summaries: Vec::new(),
    }
}

/// A compacting reader assigns Grace offset 1
/// after Ada's successor is deleted. The physical-slot view must retain the
/// deleted slot as a hole and append a revived key after the complete span.
#[test]
fn severed_proof_overlay_rows_carry_physical_offsets_across_holes() {
    let budget = Arc::new(MemoryBudget::unlimited());
    let inserted = link(
        None,
        1,
        person_delta(&[(1, "Ada"), (2, "Lin"), (3, "Grace")], &[]),
        &budget,
    );
    let deleted = link(Some(inserted), 2, person_delta(&[], &[2]), &budget);
    let revived = link(
        Some(deleted),
        3,
        person_delta(&[(2, "Lin revived")], &[]),
        &budget,
    );
    let state = state(revived);

    let slots = state
        .node_slots("Person")
        .map(|slot| {
            (
                slot.position,
                slot.row.map(|row| match &row[0] {
                    Value::Int64(id) => *id,
                    other => panic!("unexpected primary key {other}"),
                }),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        slots,
        vec![(0, Some(1)), (1, None), (2, Some(3)), (3, Some(2))]
    );
    assert_eq!(state.node_slots("Person").count(), 4);
}

#[test]
fn endpoint_tombstones_union_oldest_first_without_offset_revival() {
    let budget = Arc::new(MemoryBudget::unlimited());
    let mut initial = person_delta(&[(1, "Ada"), (2, "Grace")], &[]);
    initial.edges.insert(
        "Knows".to_owned(),
        vec![OverlayEdge {
            from: 0,
            to: 1,
            values: vec![Value::Int64(1)],
        }],
    );
    let base = link(None, 1, initial, &budget);
    let old_snapshot = state(Arc::clone(&base));

    let mut detached = person_delta(&[], &[1]);
    detached.rel_tombstones.insert(
        "Knows".to_owned(),
        RelEndpointTombstones {
            from_offsets: [0].into_iter().collect(),
            to_offsets: Default::default(),
        },
    );
    let detached = link(Some(base), 2, detached, &budget);
    let detached_snapshot = state(Arc::clone(&detached));

    let mut revived = person_delta(&[(1, "Ada revived")], &[]);
    revived.rel_tombstones.insert(
        "Knows".to_owned(),
        RelEndpointTombstones {
            from_offsets: Default::default(),
            to_offsets: [1].into_iter().collect(),
        },
    );
    revived.edges.insert(
        "Knows".to_owned(),
        vec![OverlayEdge {
            from: 2,
            to: 1,
            values: vec![Value::Int64(2)],
        }],
    );
    let revived_snapshot = state(link(Some(detached), 3, revived, &budget));

    assert!(old_snapshot.rel_tombstones("Knows").is_empty());
    assert_eq!(
        old_snapshot
            .node_slots("Person")
            .map(|slot| slot.row.is_some())
            .collect::<Vec<_>>(),
        vec![true, true]
    );
    assert_eq!(old_snapshot.rel_edges("Knows").count(), 1);

    assert_eq!(
        detached_snapshot
            .node_slots("Person")
            .map(|slot| slot.row.is_some())
            .collect::<Vec<_>>(),
        vec![false, true]
    );
    let effective = revived_snapshot.rel_tombstones("Knows");
    assert_eq!(effective.from_offsets, [0].into_iter().collect());
    assert_eq!(effective.to_offsets, [1].into_iter().collect());
    assert_eq!(
        revived_snapshot
            .node_slots("Person")
            .map(|slot| (slot.position, slot.row.map(|row| row[0].clone())))
            .collect::<Vec<_>>(),
        vec![
            (0, None),
            (1, Some(Value::Int64(2))),
            (2, Some(Value::Int64(1))),
        ]
    );
    assert_eq!(revived_snapshot.rel_edges("Knows").count(), 2);
}

fn seeded_relationships() -> (tempfile::TempDir, Pager, Catalog, RelTable) {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("graph.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut catalog = person_catalog();
    let mut nodes = NodeTable::new(catalog.node_table("Person").unwrap().clone());
    for id in 0..6 {
        nodes
            .recover_row(person_row(id, &format!("person-{id}")))
            .unwrap();
    }
    nodes.checkpoint(&pager, &mut catalog).unwrap();

    let mut relationships = RelTable::new(rel_schema("Knows"));
    for (from, to, since) in [(0, 2, 1), (2, 1, 2), (3, 3, 3)] {
        relationships
            .recover_edge(from, to, vec![Value::Int64(since)])
            .unwrap();
    }
    relationships.checkpoint(&pager, &mut catalog).unwrap();
    for (from, to, since) in [(4, 5, 4), (4, 5, 5), (2, 5, 6), (0, 1, 7)] {
        relationships
            .recover_edge(from, to, vec![Value::Int64(since)])
            .unwrap();
    }
    (directory, pager, catalog, relationships)
}

#[test]
fn merged_reads_filter_out_in_both_duplicates_self_loops_and_scans() {
    let (_directory, pager, catalog, mut relationships) = seeded_relationships();
    relationships.set_endpoint_tombstones(RelEndpointTombstones {
        from_offsets: [0, 3, 4].into_iter().collect(),
        to_offsets: [1, 3].into_iter().collect(),
    });

    assert!(
        relationships
            .neighbors(&pager, &catalog, Direction::Out, 0)
            .unwrap()
            .is_empty()
    );
    assert!(
        relationships
            .neighbors(&pager, &catalog, Direction::In, 1)
            .unwrap()
            .is_empty()
    );
    assert!(
        relationships
            .neighbors(&pager, &catalog, Direction::Both, 3)
            .unwrap()
            .is_empty()
    );
    assert!(
        relationships
            .neighbors(&pager, &catalog, Direction::Out, 4)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        relationships
            .neighbors(&pager, &catalog, Direction::Both, 2)
            .unwrap(),
        vec![5]
    );
    assert_eq!(
        relationships.scan(&pager, &catalog).unwrap(),
        vec![(2, 5, vec![Value::Int64(6)])]
    );

    assert_eq!(
        relationships
            .neighbors_before_filtering(&pager, &catalog, Direction::Both, 3)
            .unwrap(),
        vec![3, 3],
        "budget sizing must count both resident self-loop copies before filtering and dedup"
    );
}

#[test]
fn relationship_tables_keep_independent_endpoint_predicates() {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("multi.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut catalog = person_catalog();
    catalog.add_rel_table(rel_schema("Likes")).unwrap();
    let mut nodes = NodeTable::new(catalog.node_table("Person").unwrap().clone());
    for id in 0..3 {
        nodes.recover_row(person_row(id, "person")).unwrap();
    }
    nodes.checkpoint(&pager, &mut catalog).unwrap();

    let mut knows = RelTable::new(rel_schema("Knows"));
    knows.recover_edge(0, 1, vec![Value::Int64(1)]).unwrap();
    knows.set_endpoint_tombstones(RelEndpointTombstones {
        from_offsets: [0].into_iter().collect(),
        to_offsets: Default::default(),
    });
    let mut likes = RelTable::new(rel_schema("Likes"));
    likes.recover_edge(0, 1, vec![Value::Int64(2)]).unwrap();
    likes.set_endpoint_tombstones(RelEndpointTombstones {
        from_offsets: Default::default(),
        to_offsets: [2].into_iter().collect(),
    });

    assert!(knows.scan(&pager, &catalog).unwrap().is_empty());
    assert_eq!(
        likes.scan(&pager, &catalog).unwrap(),
        vec![(0, 1, vec![Value::Int64(2)])]
    );
}

#[test]
fn endpoint_tombstone_entries_are_fully_charged() {
    let base = CommitDelta::default().estimated_bytes().unwrap();
    let mut delta = CommitDelta::default();
    delta.rel_tombstones.insert(
        "Knows".to_owned(),
        RelEndpointTombstones {
            from_offsets: [1, 2, 3].into_iter().collect(),
            to_offsets: [4, 5].into_iter().collect(),
        },
    );
    let expected = base
        + MAP_ENTRY_OVERHEAD_BYTES
        + "Knows".len()
        + REL_ENDPOINT_TOMBSTONES_OVERHEAD_BYTES
        + 5 * REL_TOMBSTONE_OFFSET_OVERHEAD_BYTES;
    assert_eq!(delta.estimated_bytes().unwrap(), expected);

    let budget = Arc::new(MemoryBudget::new(COMMIT_LINK_OVERHEAD_BYTES + expected));
    let link = CommitLink::new_arc(None, 1, delta, Arc::clone(&budget)).unwrap();
    assert_eq!(budget.charged(), COMMIT_LINK_OVERHEAD_BYTES + expected);
    drop(link);
    assert_eq!(budget.charged(), 0);
}
