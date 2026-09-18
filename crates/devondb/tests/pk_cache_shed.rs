//! The primary-key resolution and decoded-group caches yield under budget
//! pressure instead of filling the budget until an unrelated charge fails.
//!
//! These caches are proportional to table size. Pager eviction cannot reach
//! them, and a checkpoint cannot free the charging transaction's caches because
//! its own snapshot pins the owning state.
//!
//! The caches are global (`PUBLISHED_PK_CACHES` is a process-wide registry)
//! and shedding is global, so the two tests serialize.

use std::sync::{Mutex, PoisonError};

use devondb::{Database, Options, Statement};
use devondb_plan::{
    statement::SetItem,
    text::parser::{Parsed, parse},
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const MEMORY_LIMIT: usize = 1024 * 1024;
const ROW_COUNT: i64 = 75;
const PAYLOAD_BYTES: usize = 12 * 1024;
const FAT_UPDATE_BYTES: usize = 400 * 1024;

static SERIAL: Mutex<()> = Mutex::new(());

fn seeded_database(path: &std::path::Path) -> Database {
    let mut database = Database::create_with(
        path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: MEMORY_LIMIT,
        },
    )
    .unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Blob".to_owned(),
            columns: vec![
                Column {
                    name: "id".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                },
                Column {
                    name: "payload".to_owned(),
                    ty: LogicalType::String,
                    primary_key: false,
                },
                Column {
                    name: "flag".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: false,
                },
            ],
        })
        .unwrap();
    for id in 0..ROW_COUNT {
        database
            .execute(&Statement::InsertNode {
                table: "Blob".to_owned(),
                rows: vec![vec![
                    Value::Int64(id),
                    Value::String(incompressible_payload(id)),
                    Value::Int64(0),
                ]],
            })
            .unwrap();
    }
    database.checkpoint().unwrap();
    database
}

/// A high-entropy per-row payload, so the §8.4 encoding selection keeps
/// plain (a constant `"x"` payload constant-encodes to ~one page, and the
/// decoded groups' page-frame pressure this fixture exists to build never
/// materializes on disk).
fn incompressible_payload(seed: i64) -> String {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64 ^ (seed as u64).wrapping_mul(0xA24B_AED4_963E_E407);
    let mut text = String::with_capacity(PAYLOAD_BYTES);
    for _ in 0..PAYLOAD_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        text.push(char::from((state >> 33) as u8 % 94 + 33));
    }
    text
}

fn update_flag(database: &mut Database, id: i64, flag: i64) {
    database
        .execute(&Statement::UpdateNode {
            table: "Blob".to_owned(),
            set: vec![SetItem {
                column: "flag".to_owned(),
                value: Value::Int64(flag),
            }],
            key_column: "id".to_owned(),
            key: Value::Int64(id),
        })
        .unwrap();
}

/// Builds the caches through a keyed update and asserts they actually
/// dominate the budget — the fixture is honest only if the decoded fat
/// group's charge is present before the pressure moment.
fn build_dominating_caches(database: &mut Database) -> usize {
    update_flag(database, 5, 1);
    let charged = database.memory_budget().charged();
    assert!(
        charged > (ROW_COUNT as usize) * PAYLOAD_BYTES * 8 / 10,
        "fixture defect: caches hold only {charged} bytes — the decoded \
         fat group's charge is missing, so the test would not exercise the \
         cache-pressure path"
    );
    charged
}

#[test]
fn write_set_charge_sheds_caches_instead_of_failing() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    let path = directory.path().join("ladder.devondb");
    let mut database = seeded_database(&path);
    build_dominating_caches(&mut database);

    // A write-set charge arrives while the caches hold the budget. Without the
    // cache-shedding rung, this statement fails with BudgetExceeded.
    database
        .execute(&Statement::UpdateNode {
            table: "Blob".to_owned(),
            set: vec![SetItem {
                column: "payload".to_owned(),
                value: Value::String("y".repeat(FAT_UPDATE_BYTES)),
            }],
            key_column: "id".to_owned(),
            key: Value::Int64(9),
        })
        .unwrap();

    // Both writes are visible and keyed resolution still works after the
    // shed (the caches rebuild or degrade to the scan fallback — either
    // way the answer is right).
    let Parsed::Query(plan) = parse("nodes(Blob) as b | project b.id, b.flag").unwrap() else {
        panic!("expected query");
    };
    let rows = database.run(&plan).unwrap().rows;
    assert_eq!(rows.len(), usize::try_from(ROW_COUNT).unwrap());
    assert!(rows.contains(&vec![Value::Int64(5), Value::Int64(1)]));
    update_flag(&mut database, 9, 7);
    let rows = database.run(&plan).unwrap().rows;
    assert!(rows.contains(&vec![Value::Int64(9), Value::Int64(7)]));
}

#[test]
fn scan_result_working_set_sheds_caches_instead_of_refusing() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    let path = directory.path().join("working-set.devondb");
    let mut database = seeded_database(&path);
    build_dominating_caches(&mut database);

    // A large query arriving while the caches dominate must shed them
    // rather than refuse: the ~900 KiB payload projection cannot fit
    // beside ~900 KiB of caches in a 1 MiB budget, but fits alone. The
    // rescue is TRANSITIVE — chunk production charges page frames through
    // charge_or_reclaim, whose second rung sheds the caches
    // before the scan-result working-set ladder (which has no cache rung
    // of its own, deliberately) ever sees pressure. Without the reclaimer
    // rung, the scan result working set is refused even though it fits after
    // the caches are released.
    let Parsed::Query(plan) = parse("nodes(Blob) as b | project b.payload").unwrap() else {
        panic!("expected query");
    };
    let rows = database.run(&plan).unwrap().rows;
    assert_eq!(rows.len(), usize::try_from(ROW_COUNT).unwrap());
    assert!(
        rows.iter()
            .all(|row| matches!(&row[0], Value::String(payload) if payload.len() == PAYLOAD_BYTES))
    );
}

#[test]
fn charge_or_reclaim_second_rung_frees_the_cache_class() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempdir().unwrap();
    let path = directory.path().join("reclaimer.devondb");
    let mut database = seeded_database(&path);
    let charged_with_caches = build_dominating_caches(&mut database);
    let budget = database.memory_budget();

    // A plain try_charge for most of the budget must fail — the caches
    // hold it. charge_or_reclaim must succeed by shedding them; the pager rung
    // alone cannot free cache-class bytes.
    let request = MEMORY_LIMIT * 7 / 10;
    assert!(
        !budget.try_charge(request),
        "fixture defect: {request} bytes charged without pressure \
         (only {charged_with_caches} held)"
    );
    assert!(
        budget.charge_or_reclaim(request),
        "the reclaimer's cache rung failed to free the cache class \
         ({} bytes still charged)",
        budget.charged()
    );
    budget.release(request);
}
