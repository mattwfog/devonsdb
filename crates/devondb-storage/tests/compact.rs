//! Offline compaction: content, size, feature derivation, WAL, and locks.

use std::fs;
use std::path::{Path, PathBuf};

use devondb_storage::catalog::{Catalog, IndexEntry, IndexKind, RelStorage, TableStorage};
use devondb_storage::compact::compact;
use devondb_storage::csr_group::CsrGroup;
use devondb_storage::hnsw::format::HnswRoot;
use devondb_storage::hnsw::index::load_persisted_index;
use devondb_storage::hnsw::types::{HnswConfig, HnswMetric, NavigationEncoding};
use devondb_storage::lock::{LockPaths, PublicationGate, WriterLease};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::node_table::NodeTable;
use devondb_storage::pager::Pager;
use devondb_storage::rel_table::RelTable;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, HNSW_INDEX_FLAG};
use devondb_types::DevonError;
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema, RelTableSchema};
use devondb_types::value::Value;
use tempfile::TempDir;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"compact-test-db!";
const BLOAT_PAGES: usize = 256;

#[derive(Debug, Clone, PartialEq)]
struct Contents {
    nodes: Vec<Vec<Value>>,
    relationships: Vec<(u64, u64, Vec<Value>)>,
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn node_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Person".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
            column("score", LogicalType::Int64, false),
        ],
    )
    .unwrap()
}

fn rel_schema() -> RelTableSchema {
    RelTableSchema::new(
        "Knows".to_owned(),
        "Person".to_owned(),
        "Person".to_owned(),
        vec![column("since", LogicalType::Int64, false)],
    )
    .unwrap()
}

fn rows() -> Vec<Vec<Value>> {
    [(30, "Cora"), (10, "Ada"), (20, "Bea")]
        .into_iter()
        .map(|(id, name)| {
            vec![
                Value::Int64(id),
                Value::String(name.to_owned()),
                Value::Int64(7),
            ]
        })
        .collect()
}

fn build_fixture(path: &Path, bloat_pages: usize, force_encoding: bool) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
    let nodes = node_schema();
    let relationships = rel_schema();
    let mut catalog = Catalog::default();
    catalog.add_node_table(nodes.clone()).unwrap();
    catalog.add_rel_table(relationships.clone()).unwrap();

    let types = nodes.columns().iter().map(|column| column.ty).collect();
    let mut group = NodeGroup::new(types).unwrap();
    for row in rows() {
        group.push_row(row).unwrap();
    }
    let node_root = if force_encoding {
        group.write_forcing_encodings(&pager, &[(2, 1)]).unwrap()
    } else {
        group.write_forcing_encodings(&pager, &[]).unwrap()
    };
    catalog
        .set_table_storage(
            nodes.name(),
            TableStorage {
                groups: vec![node_root],
            },
        )
        .unwrap();

    let rel_types = vec![LogicalType::Int64];
    let mut forward = CsrGroup::new(3, rel_types.clone()).unwrap();
    forward.push_edge(0, 2, vec![Value::Int64(2001)]).unwrap();
    forward.push_edge(2, 1, vec![Value::Int64(2002)]).unwrap();
    let mut backward = CsrGroup::new(3, rel_types).unwrap();
    backward.push_edge(2, 0, vec![Value::Int64(2001)]).unwrap();
    backward.push_edge(1, 2, vec![Value::Int64(2002)]).unwrap();
    catalog
        .set_rel_storage(
            relationships.name(),
            RelStorage {
                fwd: vec![forward.write(&pager).unwrap()],
                bwd: vec![backward.write(&pager).unwrap()],
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();

    for page_number in 0..bloat_pages {
        let page = pager.allocate_page().unwrap();
        let mut bytes = vec![0_u8; PAGE_SIZE as usize];
        bytes[..8].copy_from_slice(&(page_number as u64).to_le_bytes());
        pager.write_page(page, &bytes).unwrap();
    }
    pager.sync().unwrap();
    drop(pager);
    fs::metadata(path).unwrap().len()
}

fn contents(path: &Path) -> Contents {
    let pager = Pager::open(path).unwrap();
    let catalog = Catalog::load(&pager).unwrap();
    let nodes = NodeTable::new(node_schema())
        .scan(&pager, &catalog)
        .unwrap();
    let relationships = RelTable::new(rel_schema()).scan(&pager, &catalog).unwrap();
    Contents {
        nodes,
        relationships,
    }
}

fn wal_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push("-wal");
    value.into()
}

#[test]
fn compact_preserves_scan_order_relationships_and_encoded_values() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("content.devondb");
    build_fixture(&path, 64, true);
    let expected = contents(&path);
    let before_flags = Pager::open(&path).unwrap().superblock().feature_flags;
    assert_ne!(before_flags & COLUMN_ENCODINGS_FLAG, 0);

    let stats = compact(&path).unwrap();

    assert_eq!(contents(&path), expected);
    assert!(stats.after_bytes < stats.before_bytes);
    let after_flags = Pager::open(&path).unwrap().superblock().feature_flags;
    assert_ne!(after_flags & COLUMN_ENCODINGS_FLAG, 0);
}

#[test]
fn compact_clears_a_stale_encoding_bit_when_every_live_group_is_plain() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("plain.devondb");
    build_fixture(&path, 0, false);
    let pager = Pager::open(&path).unwrap();
    pager
        .commit_feature_flags(pager.superblock().feature_flags | COLUMN_ENCODINGS_FLAG)
        .unwrap();
    drop(pager);
    assert_ne!(
        Pager::open(&path).unwrap().superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        0
    );

    compact(&path).unwrap();

    assert_eq!(
        Pager::open(&path).unwrap().superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        0
    );
}

#[test]
fn bloated_fixture_shrinks_to_within_twenty_five_percent_of_fresh() {
    let directory = TempDir::new().unwrap();
    let fresh = directory.path().join("fresh.devondb");
    let bloated = directory.path().join("bloated.devondb");
    let fresh_bytes = build_fixture(&fresh, 0, false);
    let bloated_bytes = build_fixture(&bloated, BLOAT_PAGES, false);

    let stats = compact(&bloated).unwrap();
    eprintln!(
        "compact size gate: bloated_before={bloated_bytes} compacted={} fresh={fresh_bytes}",
        stats.after_bytes
    );

    assert!(bloated_bytes > fresh_bytes * 2);
    assert!(
        stats.after_bytes.saturating_mul(4) <= fresh_bytes.saturating_mul(5),
        "compacted={} exceeds 125% of fresh={fresh_bytes}",
        stats.after_bytes
    );
    assert_eq!(contents(&bloated), contents(&fresh));
}

#[test]
fn nonempty_wal_is_refused_without_changing_the_main_file() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("wal.devondb");
    build_fixture(&path, 8, false);
    fs::write(wal_path(&path), b"pending").unwrap();
    let before = fs::read(&path).unwrap();

    let error = compact(&path).unwrap_err();

    assert!(
        matches!(error, DevonError::Busy { context } if context.contains("WAL") && context.contains("checkpoint"))
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(wal_path(&path)).unwrap(), b"pending");
}

#[test]
fn held_writer_or_publication_lock_is_refused() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("locked.devondb");
    build_fixture(&path, 8, false);
    let paths = LockPaths::for_main(&path).unwrap();

    let writer = WriterLease::try_acquire(&paths).unwrap();
    assert!(matches!(compact(&path), Err(DevonError::Busy { .. })));
    drop(writer);

    let gate = PublicationGate::open(&paths).unwrap();
    let publication = gate.try_exclusive().unwrap();
    assert!(matches!(compact(&path), Err(DevonError::Busy { .. })));
    drop(publication);

    compact(&path).unwrap();
    assert_eq!(contents(&path).nodes, rows());
}

#[test]
fn hnsw_root_and_catalog_entry_remain_live() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("hnsw.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut catalog = Catalog::default();
    catalog
        .add_node_table(
            NodeTableSchema::new(
                "Corpus".to_owned(),
                vec![
                    column("id", LogicalType::Int64, true),
                    column("embedding", LogicalType::Vector { dim: 2 }, false),
                ],
            )
            .unwrap(),
        )
        .unwrap();
    let root = HnswRoot {
        config: HnswConfig::with_defaults(7, HnswMetric::L2, NavigationEncoding::F32),
        covered_rows: 0,
        entry_node: None,
        entry_level: 0,
        layer_count: 0,
        group_count: 0,
        layer_dir_first_page: 0,
        layer_dir_byte_len: 0,
        layer_dir_crc32c: crc32c::crc32c(&[]),
    };
    let root_page = pager.allocate_page().unwrap();
    let mut bytes = vec![0_u8; PAGE_SIZE as usize];
    root.encode(&mut bytes).unwrap();
    pager.write_page(root_page, &bytes).unwrap();
    catalog
        .add_index(IndexEntry {
            name: "corpus_l2".to_owned(),
            kind: IndexKind::Hnsw,
            table: "Corpus".to_owned(),
            column: "embedding".to_owned(),
            root: root_page,
        })
        .unwrap();
    catalog.save(&pager, 1).unwrap();
    drop(pager);

    compact(&path).unwrap();

    let pager = Pager::open(&path).unwrap();
    let catalog = Catalog::load(&pager).unwrap();
    assert_ne!(pager.superblock().feature_flags & HNSW_INDEX_FLAG, 0);
    assert_eq!(catalog.indexes().len(), 1);
    assert_eq!(
        load_persisted_index(&pager, catalog.indexes()[0].root)
            .unwrap()
            .root,
        root
    );
}
