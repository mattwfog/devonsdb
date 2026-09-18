//! Derived full-text cache lifecycle, visibility boundary and budget pin laws.
use std::sync::{Arc, Mutex, PoisonError};

use devondb::{Database, Options, Statement};
use devondb_storage::{
    budget::MemoryBudget,
    catalog::{Catalog, TableStorage},
    fulltext::{FullTextIndex, FullTextIndexBuilder, FullTextQuery, query},
    node_group::NodeGroup,
    overlay::{ChargedFullTextIndex, FullTextResult, PublishedState, shed_fulltext_caches},
    pager::Pager,
};
use devondb_types::{
    DevonError,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
    value::Value,
};
use tempfile::tempdir;

static SERIAL: Mutex<()> = Mutex::new(());

fn schema(ty: LogicalType) -> NodeTableSchema {
    NodeTableSchema::new(
        "Document".into(),
        vec![
            Column {
                name: "id".into(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "body".into(),
                ty,
                primary_key: false,
            },
        ],
    )
    .unwrap()
}

fn fixture(path: &std::path::Path, documents: &[String], limit: usize) -> Database {
    let pager = Pager::create(path, 4096, *b"fulltext-fixture").unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Int64, LogicalType::String]).unwrap();
    for (id, text) in documents.iter().enumerate() {
        group
            .push_row(vec![Value::Int64(id as i64), Value::String(text.clone())])
            .unwrap();
    }
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema(LogicalType::String)).unwrap();
    catalog
        .set_table_storage(
            "Document",
            TableStorage {
                groups: vec![group.write(&pager).unwrap()],
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();
    drop(pager);
    Database::open_with(
        path,
        Options {
            page_size: 4096,
            memory_limit: limit,
        },
    )
    .unwrap()
}

fn indexed(database: &Database) -> Arc<ChargedFullTextIndex> {
    match database.fulltext_index("document", "BODY").unwrap() {
        FullTextResult::Indexed(index) => index,
        FullTextResult::Unavailable => panic!("fixture index refused"),
    }
}

fn corpus() -> Vec<String> {
    (0..50)
        .map(|row| {
            format!(
                "row{row} river stream stream {}",
                if row % 2 == 0 { "blue" } else { "green" }
            )
        })
        .collect()
}

#[test]
fn lazy_build_oracle_parity_and_exact_arc_pin_charge() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    let texts = corpus();
    let database = fixture(&directory.path().join("lazy.devondb"), &texts, 1024 * 1024);
    assert!(!database.fulltext_index_present("Document", "body"));
    let first = indexed(&database);
    assert!(database.fulltext_index_present("Document", "body"));
    let second = indexed(&database);
    assert!(Arc::ptr_eq(&first, &second));
    let oracle =
        FullTextIndex::build(texts.iter().enumerate().map(|(i, text)| (i as u64, text))).unwrap();
    assert_eq!(&**first, &oracle);
    drop(second);
    drop(first);
    shed_fulltext_caches();
    let budget = database.memory_budget();
    let baseline = budget.charged(); // Pruned pages are warm; only the index grows now.
    let index = indexed(&database);
    let bytes = index.heap_bytes();
    assert_eq!(budget.charged(), baseline + bytes);
    assert!(shed_fulltext_caches());
    assert!(!database.fulltext_index_present("Document", "body"));
    assert_eq!(budget.charged(), baseline + bytes);
    drop(index);
    assert_eq!(budget.charged(), baseline);
    let rebuilt = indexed(&database);
    assert_eq!(&**rebuilt, &oracle);
    shed_fulltext_caches();
    drop(rebuilt);
    assert_eq!(budget.charged(), baseline);
}

#[test]
fn ordinary_commit_adopts_but_checkpoint_rebuilds_and_overlay_is_excluded() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    let mut database = fixture(
        &directory.path().join("adopt.devondb"),
        &corpus(),
        1024 * 1024,
    );
    let before = indexed(&database);
    database
        .execute(&Statement::InsertNode {
            table: "Document".into(),
            rows: vec![vec![
                Value::Int64(90),
                Value::String("overlay novel".into()),
            ]],
        })
        .unwrap();
    let after = indexed(&database);
    assert!(Arc::ptr_eq(&before, &after));
    assert_eq!(after.stats().document_count(), 50);
    assert!(query(&after, "novel", 10).is_empty());
    database.checkpoint().unwrap();
    let rebuilt = indexed(&database);
    assert!(!Arc::ptr_eq(&after, &rebuilt));
    assert_eq!(rebuilt.stats().document_count(), 51);
    assert_eq!(query(&rebuilt, "novel", 10).len(), 1);
}

fn state(catalog: Catalog) -> Arc<PublishedState> {
    Arc::new(PublishedState {
        catalog: Arc::new(catalog),
        chain: None,
        last_commit_lsn: 1,
        catalog_generation: 1,
        recent_summaries: Vec::new(),
    })
}

#[test]
fn adoption_validates_hand_built_catalogs_for_removed_or_retyped_columns() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    let pager = Pager::create(
        directory.path().join("catalog.devondb"),
        4096,
        *b"fulltext-catalog",
    )
    .unwrap();
    let budget = Arc::new(MemoryBudget::unlimited());
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema(LogicalType::String)).unwrap();
    let prev = state(catalog.clone());
    PublishedState::fulltext_index(&prev, &pager, &budget, "Document", "body").unwrap();
    let unchanged = state(catalog);
    PublishedState::adopt_fulltext_caches(&prev, &unchanged);
    assert!(PublishedState::fulltext_index_present(
        &unchanged, "Document", "body"
    ));
    let mut retyped = Catalog::default();
    retyped.add_node_table(schema(LogicalType::Int64)).unwrap();
    let mut removed = Catalog::default();
    removed
        .add_node_table(
            NodeTableSchema::new(
                "Document".into(),
                vec![schema(LogicalType::String).columns()[0].clone()],
            )
            .unwrap(),
        )
        .unwrap();
    for invalid in [Catalog::default(), retyped, removed] {
        let next = state(invalid);
        PublishedState::adopt_fulltext_caches(&prev, &next);
        assert!(!PublishedState::fulltext_index_present(
            &next, "Document", "body"
        ));
    }
}

#[test]
fn non_string_column_reports_actual_type() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    let database = fixture(
        &directory.path().join("type.devondb"),
        &corpus(),
        1024 * 1024,
    );
    let error = database.fulltext_index("Document", "id").unwrap_err();
    assert!(matches!(error, DevonError::InvalidArgument { .. }));
    assert!(error.to_string().contains("Int64"));
}

fn pressure_corpus() -> Vec<String> {
    let text = (0..500).map(|n| format!("term{n:04} ")).collect::<String>();
    vec![text; 50]
}

#[test]
fn pressure_reclaimer_sheds_fulltext_before_refusing_a_charge() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    let database = fixture(
        &directory.path().join("pressure.devondb"),
        &pressure_corpus(),
        1024 * 1024,
    );
    let index = indexed(&database);
    assert!(index.heap_bytes() > 400 * 1024);
    drop(index);
    let budget = database.memory_budget();
    let request = 800 * 1024;
    assert!(!budget.try_charge(request));
    assert!(budget.charge_or_reclaim(request));
    assert!(!database.fulltext_index_present("Document", "body"));
    budget.release(request);
}

#[test]
fn write_set_pressure_sheds_fulltext_and_preserves_the_write() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    let mut database = fixture(
        &directory.path().join("write.devondb"),
        &pressure_corpus(),
        1024 * 1024,
    );
    drop(indexed(&database));
    database
        .execute(&Statement::InsertNode {
            table: "Document".into(),
            rows: vec![vec![
                Value::Int64(100),
                Value::String("x".repeat(700 * 1024)),
            ]],
        })
        .unwrap();
    assert!(!database.fulltext_index_present("Document", "body"));
    let devondb_plan::text::parser::Parsed::Query(plan) =
        devondb_plan::text::parser::parse("nodes(Document) as d | project d.id").unwrap()
    else {
        panic!("expected query");
    };
    assert_eq!(database.run(&plan).unwrap().rows.len(), 51);
}

#[test]
fn incremental_builder_and_bounded_ranking_match_full_score_sort() {
    let texts = corpus();
    let mut builder = FullTextIndexBuilder::default();
    for (ordinal, text) in texts.iter().enumerate().rev() {
        builder.push(ordinal as u64, text).unwrap();
    }
    assert!(builder.resident_size_bytes() > 0);
    assert!(builder.push(0, "duplicate").is_err());
    let index = builder.finish();
    let prepared = FullTextQuery::new(&index, "stream blue blue");
    let mut oracle: Vec<_> = texts
        .iter()
        .enumerate()
        .map(|(row, text)| {
            assert_eq!(
                prepared.score_row(&index, row as u64),
                prepared.score_text(text).unwrap()
            );
            (row as u64, prepared.score_row(&index, row as u64))
        })
        .collect();
    oracle.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    for k in [0, 1, 7, 50, 60] {
        assert_eq!(
            query(&index, "stream blue blue", k),
            oracle[..k.min(50)].to_vec()
        );
    }
    assert!(
        FullTextQuery::new(&index, "novel")
            .score_text("novel")
            .unwrap()
            > 0
    );
}

#[test]
fn compressed_string_expansion_is_refused_before_payload_decode() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    // Constant encoding stores this 1.6 MiB column in approximately 32 KiB.
    // A low-cardinality index would fit, but materializing its input cannot.
    let texts = vec!["word ".repeat(6554); 50];
    let database = fixture(
        &directory.path().join("compressed.devondb"),
        &texts,
        1024 * 1024,
    );
    database.reset_page_read_count();
    assert!(matches!(
        database.fulltext_index("Document", "body").unwrap(),
        FullTextResult::Unavailable
    ));
    assert!(
        database.page_read_count() <= 1,
        "payload pages were read before refusing decoded expansion"
    );
    assert!(!database.fulltext_index_present("Document", "body"));
    let baseline = database.memory_budget().charged();
    shed_fulltext_caches();
    assert!(matches!(
        database.fulltext_index("Document", "body").unwrap(),
        FullTextResult::Unavailable
    ));
    assert_eq!(database.memory_budget().charged(), baseline);
}

#[test]
fn builder_refuses_before_allocation_and_reports_exact_resident_capacity() {
    let mut builder = FullTextIndexBuilder::default();
    let mut requests = Vec::new();
    assert!(
        builder
            .push_with_reservation(0, "new term", |bytes| {
                requests.push(bytes);
                false
            })
            .is_err()
    );
    assert!(!requests.is_empty());
    assert_eq!(builder.resident_size_bytes(), 0);
    let mut last = 0;
    builder
        .push_with_reservation(0, "new term", |bytes| {
            last = bytes;
            true
        })
        .unwrap();
    assert_eq!(last, builder.resident_size_bytes());
    for row in 1..100 {
        builder
            .push_with_reservation(row, "new term", |bytes| {
                last = bytes;
                true
            })
            .unwrap();
        assert_eq!(last, builder.resident_size_bytes());
    }
    assert!(builder.finish_peak_bytes() >= builder.resident_size_bytes());
}
