use std::{
    fs::File,
    io::{BufWriter, Write},
    mem,
    path::Path,
    sync::{Mutex, PoisonError},
};

use devondb::{Database, Options, Statement, text};
use devondb_exec::operators::spill_runs_created;
use devondb_plan::text::parser::Parsed;
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const MEMORY_LIMIT: usize = 1024 * 1024;
const ROW_COUNT: usize = (MEMORY_LIMIT / mem::size_of::<i64>()) * 2;

/// The spill-run counter is process-global, so the two tests
/// serialize to keep their deltas attributable.
static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn spilled_aggregate_reclaims_cache_and_releases_run_charges() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("spill-reclaim.devondb");
    seed_rows(&path);

    let mut database = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: MEMORY_LIMIT,
        },
    )
    .unwrap();
    let budget = database.memory_budget();
    shed_to_cache_floor(&budget);
    let baseline = budget.charged();

    // A unique group key keeps the aggregate on the buffered path. The
    // checkpointed Int64 payload alone spans two memory limits, so it must
    // spill and exercise both merge buffering and spilled-run charge release.
    let runs_before = spill_runs_created();
    let result = run_text(
        &mut database,
        "nodes(T) as r | aggregate count(r.pk) as per_pk by r.pk | aggregate count(r.pk) as n",
    );
    assert_eq!(result.rows, vec![vec![Value::Int64(ROW_COUNT as i64)]]);
    assert!(
        spill_runs_created() > runs_before,
        "aggregate did not spill"
    );

    shed_to_cache_floor(&budget);
    // An impossible reservation asks the installed reclaimer to evict every
    // eligible frame before each measurement. Both sides therefore retain the
    // same eight-frame cache floor, making equality tighter than an epsilon.
    assert_eq!(budget.charged(), baseline);
}

#[test]
fn ungrouped_count_streams_without_spill_artifacts() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("streaming-count.devondb");
    seed_rows(&path);

    let mut database = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: MEMORY_LIMIT,
        },
    )
    .unwrap();
    let runs_before = spill_runs_created();
    let result = run_text(&mut database, "nodes(T) as r | aggregate count(r.pk) as n");

    assert_eq!(result.rows, vec![vec![Value::Int64(ROW_COUNT as i64)]]);
    // `<db>.tmp` always holds this handle's locked directory, so directory
    // absence can no longer mark no-spill; the
    // process-global run counter does, exactly.
    assert_eq!(
        spill_runs_created(),
        runs_before,
        "streaming aggregate created spill artifacts"
    );
}

fn seed_rows(path: &Path) {
    let csv_path = path.with_extension("csv");
    let mut csv = BufWriter::new(File::create(&csv_path).unwrap());
    writeln!(csv, "pk").unwrap();
    for value in 0..ROW_COUNT {
        writeln!(csv, "{value}").unwrap();
    }
    csv.flush().unwrap();

    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "T".to_owned(),
            columns: vec![Column {
                name: "pk".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            }],
        })
        .unwrap();
    database
        .execute(&Statement::CopyNode {
            table: "T".to_owned(),
            path: csv_path.to_string_lossy().into_owned(),
            sort_by: None,
        })
        .unwrap();
    database.checkpoint().unwrap();
}

fn run_text(database: &mut Database, query: &str) -> devondb::QueryResult {
    let Parsed::Query(plan) = text::parser::parse(query).unwrap() else {
        panic!("expected query text");
    };
    database.run(&plan).unwrap()
}

fn shed_to_cache_floor(budget: &devondb_storage::budget::MemoryBudget) {
    assert!(!budget.charge_or_reclaim(budget.limit() + 1));
}
