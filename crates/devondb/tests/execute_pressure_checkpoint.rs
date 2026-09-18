//! Sequential per-statement executes whose cumulative committed deltas
//! exceed the memory budget must keep succeeding. Committed deltas hold
//! budget as published chain links until a checkpoint folds them, and the
//! emergency checkpoint inside the write-set charge can never free them
//! for the charging transaction — its own snapshot pins the chain. The
//! facade therefore folds BEFORE beginning a statement's transaction once
//! pending commits hold half the budget. Severed-proof: without that
//! pre-transaction fold, this workload was refused live on 2026-08-12
//! (`write set requested 7452 bytes with 67104960 bytes charged`).

use devondb::{Database, Options, Plan, Statement};
use devondb_plan::text::parser::{Parsed, parse};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
/// The minimum accepted memory budget (1 MiB) keeps the test fast.
const MEMORY_LIMIT: usize = 1024 * 1024;
/// Per-row payload: comfortably under the budget alone, far over it
/// cumulatively.
const PAYLOAD_BYTES: usize = 200 * 1024;
const ROW_COUNT: i64 = 10;

fn parsed_plan(text: &str) -> Plan {
    match parse(text).unwrap() {
        Parsed::Query(query) => query,
        Parsed::Statement(_) => panic!("expected query: {text}"),
    }
}

#[test]
fn sequential_fat_statements_outlive_the_memory_budget() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("pressure.devondb");
    let mut database = Database::create_with(
        &path,
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
                    name: "body".to_owned(),
                    ty: LogicalType::String,
                    primary_key: false,
                },
            ],
        })
        .unwrap();

    // 10 × 200 KiB ≈ 2 MiB of committed deltas against a 1 MiB budget:
    // every statement must succeed, each pressure fold freeing the chain
    // the previous commits left charged.
    for id in 0..ROW_COUNT {
        database
            .execute(&Statement::InsertNode {
                table: "Blob".to_owned(),
                rows: vec![vec![
                    Value::Int64(id),
                    Value::String("x".repeat(PAYLOAD_BYTES)),
                ]],
            })
            .unwrap_or_else(|error| panic!("insert {id} refused: {error}"));
    }

    // Verify after a reopen with a budget the materialized result fits:
    // a full scan of ~2 MiB of rows legitimately exceeds the 1 MiB write
    // budget (fail-fast law, docs/MVCC.md §7.2 step 4), and the reopen
    // also proves the pressure-folded commits are durable.
    drop(database);
    let mut database = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: 8 * MEMORY_LIMIT,
        },
    )
    .unwrap();
    let result = database.run(&parsed_plan("nodes(Blob) as b")).unwrap();
    assert_eq!(result.rows.len(), usize::try_from(ROW_COUNT).unwrap());
}
