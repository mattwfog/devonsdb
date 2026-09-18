//! Regression coverage for bounded checkpoint space across reopen cycles.

use std::{collections::BTreeMap, fs, path::Path};

use devondb_storage::{
    catalog::Catalog,
    node_table::{DeleteCompaction, NodeTable},
    overlay::{NodeDmlEffects, PkKey},
    pager::Pager,
};
use devondb_types::{
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
    value::Value,
};
use tempfile::TempDir;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"checkpoint-space";
const TABLE: &str = "Document";
const SEED_ROWS: usize = 2500;
const CYCLES: usize = 30;
// Two baseline generations cover live data plus the one-publication reuse
// delay. Sixteen pages cover the catalog and churn-bounded ledger metadata.
const METADATA_SLACK: u64 = 16 * 4096;

fn schema() -> NodeTableSchema {
    NodeTableSchema::new(
        TABLE.to_owned(),
        vec![
            Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "body".to_owned(),
                ty: LogicalType::String,
                primary_key: false,
            },
        ],
    )
    .unwrap()
}

fn row(id: i64, body: String) -> Vec<Value> {
    vec![Value::Int64(id), Value::String(body)]
}

fn incompressible_hex(seed: u64) -> String {
    let mut state = seed ^ 0xA409_3822_299F_31D0;
    let mut text = String::with_capacity(800);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for _ in 0..800 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        text.push(char::from(HEX[(state >> 60) as usize]));
    }
    text
}

fn create_seeded(path: &Path) -> (u64, BTreeMap<i64, Vec<Value>>) {
    let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
    let table_schema = schema();
    let mut catalog = Catalog::default();
    catalog.add_node_table(table_schema.clone()).unwrap();
    catalog.save(&pager, 1).unwrap();

    let mut expected = BTreeMap::new();
    let mut table = NodeTable::new(table_schema);
    for id in 0..SEED_ROWS {
        let value = row(id as i64, incompressible_hex(id as u64));
        table.recover_row(value.clone()).unwrap();
        expected.insert(id as i64, value);
    }
    table.checkpoint(&pager, &mut catalog).unwrap();
    catalog.save(&pager, 2).unwrap();
    drop(pager);
    (fs::metadata(path).unwrap().len(), expected)
}

fn checkpoint_cycle(
    path: &Path,
    label: &str,
    cycle: usize,
    expected: &mut BTreeMap<i64, Vec<Value>>,
    delete: Option<i64>,
) {
    let pager = Pager::open(path).unwrap();
    let mut catalog = Catalog::load(&pager).unwrap();
    let generation = pager.superblock().checkpoint_lsn;
    pager.raise_min_pin(generation);
    let before_bytes = fs::metadata(path).unwrap().len();
    let retired_before = pager
        .free_pages_extension()
        .map_or(0, |extension| extension.retired_total);

    let upsert_id = if delete.is_some() {
        SEED_ROWS as i64 + cycle as i64
    } else {
        SEED_ROWS as i64 - 1 - cycle as i64
    };
    let replacement = row(upsert_id, format!("cycle-{cycle:02}"));
    let mut table = NodeTable::new(schema());
    let mut effects = NodeDmlEffects::default();
    if delete.is_some() {
        table.recover_row(replacement.clone()).unwrap();
    } else {
        effects
            .updates
            .insert(PkKey::Int64(upsert_id), replacement.as_slice());
    }
    if let Some(delete_id) = delete {
        effects.tombstones.insert(PkKey::Int64(delete_id));
        expected.remove(&delete_id);
    }
    table
        .checkpoint_with_dml(&pager, &mut catalog, effects, DeleteCompaction::Permitted)
        .unwrap();
    expected.insert(upsert_id, replacement);
    catalog.save(&pager, generation + 1).unwrap();

    let file_bytes = fs::metadata(path).unwrap().len();
    let pages_reused = pager.free_pages_session_reused();
    let pages_appended = (file_bytes - before_bytes) / u64::from(PAGE_SIZE);
    let pages_allocated = pages_appended + pages_reused;
    let pages_freed = pager
        .free_pages_extension()
        .map_or(0, |extension| extension.retired_total)
        .saturating_sub(retired_before);
    eprintln!(
        "[{label}] cycle={cycle:02} file_bytes={file_bytes} \
         pages_allocated={pages_allocated} pages_freed={pages_freed} \
         pages_reused={pages_reused} degraded={}",
        pager.free_pages_degraded()
    );
}

fn assert_bounded_and_exact(path: &Path, initial_bytes: u64, expected: &BTreeMap<i64, Vec<Value>>) {
    let final_bytes = fs::metadata(path).unwrap().len();
    assert!(
        final_bytes <= 2 * initial_bytes + METADATA_SLACK,
        "checkpoint churn grew the file from {initial_bytes} to {final_bytes} bytes"
    );

    let pager = Pager::open(path).unwrap();
    let catalog = Catalog::load(&pager).unwrap();
    let actual = NodeTable::new(schema()).scan(&pager, &catalog).unwrap();
    let expected: Vec<Vec<Value>> = expected.values().cloned().collect();
    assert_eq!(actual, expected);
}

#[test]
fn repeated_upsert_checkpoint_reopen_is_bounded() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("upsert.devondb");
    let (initial_bytes, mut expected) = create_seeded(&path);

    for cycle in 0..CYCLES {
        checkpoint_cycle(&path, "upsert", cycle, &mut expected, None);
    }

    assert_bounded_and_exact(&path, initial_bytes, &expected);
}

#[test]
fn repeated_upsert_detach_delete_checkpoint_reopen_is_bounded() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("detach-delete.devondb");
    let (initial_bytes, mut expected) = create_seeded(&path);

    for cycle in 0..CYCLES {
        checkpoint_cycle(
            &path,
            "detach-delete",
            cycle,
            &mut expected,
            Some(SEED_ROWS as i64 - 1 - cycle as i64),
        );
    }

    assert_bounded_and_exact(&path, initial_bytes, &expected);
}
