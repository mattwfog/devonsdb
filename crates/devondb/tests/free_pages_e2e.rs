//! FREE_PAGES end-to-end coverage: bounded file growth under checkpoint churn,
//! snapshot safety under a deliberately incorrect pin-horizon key, and reopen
//! correctness over a reuse-churned file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use devondb::{Database, Statement};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "devondb-free-pages-e2e-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn create_keyed(path: &Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Keyed".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("body", LogicalType::String, false),
            ],
        })
        .unwrap();
    database.checkpoint().unwrap();
    database
}

fn churn_cycle(database: &mut Database, cycle: u64, rows: u64) {
    let rows = (0..rows)
        .map(|row| {
            vec![
                Value::Int64((cycle * 10_000 + row) as i64),
                Value::String(format!("cycle-{cycle}-row-{row}-{}", "x".repeat(96))),
            ]
        })
        .collect();
    database
        .execute(&Statement::InsertNode {
            table: "Keyed".to_owned(),
            rows,
        })
        .unwrap();
    database.checkpoint().unwrap();
}

fn keyed_scan_plan() -> devondb::Plan {
    let parsed = devondb::text::parser::parse("nodes(Keyed) as k | project k.id").unwrap();
    let devondb::text::parser::Parsed::Query(plan) = parsed else {
        panic!("expected a plan");
    };
    plan
}

fn sorted_ids(result: &devondb::QueryResult) -> Vec<i64> {
    let mut ids: Vec<i64> = result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Int64(id) => *id,
            other => panic!("expected Int64, got {other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

fn scan_ids(database: &Database) -> Vec<i64> {
    sorted_ids(&database.snapshot().run(&keyed_scan_plan()).unwrap())
}

/// Every checkpoint rewrites the tail group and the catalog. Without page
/// retirement and reuse, each cycle leaks that generation forever. Once the
/// ledger warms up, file growth must track only new live data.
#[test]
fn churn_workload_file_size_plateaus() {
    let directory = TestDirectory::new();
    let path = directory.path.join("plateau.devondb");
    let mut database = create_keyed(&path);

    // Warm-up: reach steady state (ledger built, horizon past the
    // one-generation delay, reuse cycling).
    for cycle in 0..12 {
        churn_cycle(&mut database, cycle, 4);
    }
    let warm_len = fs::metadata(&path).unwrap().len();
    let warm = database.free_pages_stats().unwrap().expect("ledger exists");
    assert!(!warm.degraded);
    assert!(
        warm.session_reused > 0,
        "steady-state churn must be reusing pages"
    );

    for cycle in 12..24 {
        churn_cycle(&mut database, cycle, 4);
    }
    let final_len = fs::metadata(&path).unwrap().len();
    let done = database.free_pages_stats().unwrap().expect("ledger exists");

    assert!(
        done.retired_total > warm.retired_total && done.session_reused > warm.session_reused,
        "the second half must retire and reuse ({}→{}, {}→{})",
        warm.retired_total,
        done.retired_total,
        warm.session_reused,
        done.session_reused
    );
    // The data grows (rows accumulate), so allow growth proportional to
    // the new live data, but the leak mode — one full generation plus a
    // catalog chain per cycle, ~5+ pages — must be gone. 12 cycles of
    // leak-free growth at 4 small rows/cycle fits comfortably under 3
    // pages/cycle; a leaking generation grows substantially faster.
    let growth_pages = (final_len - warm_len) / u64::from(PAGE_SIZE);
    // Emit the measured numbers when the test is run with `--nocapture`.
    eprintln!(
        "[gate1] warm(12 cycles)={warm_len}B final(24 cycles)={final_len}B \
         growth={growth_pages}pages retired={} reused={}",
        done.retired_total, done.session_reused
    );
    assert!(
        growth_pages <= 36,
        "file grew {growth_pages} pages over 12 steady-state cycles — the leak is back"
    );
}

/// A pinned scan stays identical while churn retires and reuses pages around
/// it. Substituting the snapshot-LSN minimum for the pinned catalog generation
/// must break the pin-horizon invariant that provides this safety.
#[test]
fn pinned_snapshot_survives_reuse_and_the_wrong_key_severs() {
    let directory = TestDirectory::new();
    let path = directory.path.join("pin-safety.devondb");
    let mut database = create_keyed(&path);
    for cycle in 0..6 {
        churn_cycle(&mut database, cycle, 4);
    }

    // Commit WITHOUT checkpointing: the published state now has
    // last_commit_lsn > catalog_generation — the exact divergence the
    // wrong key mistakes for headroom.
    database
        .execute(&Statement::InsertNode {
            table: "Keyed".to_owned(),
            rows: vec![vec![
                Value::Int64(999_999),
                Value::String("overlay-only".to_owned()),
            ]],
        })
        .unwrap();

    let snapshot = database.snapshot();
    let pinned_before = scan_ids(&database);

    // Churn far past the pin. Every one of these checkpoints computes the
    // horizon over live pins; the snapshot's generation holds it back.
    for cycle in 100..112 {
        churn_cycle(&mut database, cycle, 4);
    }
    let min_pin = database
        .free_pages_stats()
        .unwrap()
        .expect("ledger exists")
        .min_pin;
    // The invariant everything rests on (docs/FREE_PAGES.md § Crash
    // windows): the horizon never exceeds a live pin's catalog generation.
    assert!(
        min_pin <= snapshot.catalog_generation(),
        "min_pin {min_pin} exceeds the pinned generation {} — reuse could \
         trample pages the pinned catalog names",
        snapshot.catalog_generation()
    );

    let pinned_after = sorted_ids(&snapshot.run(&keyed_scan_plan()).unwrap());
    assert_eq!(
        pinned_after, pinned_before,
        "the pinned scan must be byte-identical across retirement and reuse"
    );

    // Negative control: recompute the horizon from snapshot LSNs. The pinned
    // snapshot's LSN exceeds its base generation, so the
    // horizon jumps PAST the generation the pinned catalog still names —
    // pages retired at that generation become eligible while a durable
    // recovery path (the superseded superblock slot) and the pinned
    // catalog can still reach them. min_pin is monotone, so the jump is
    // observable as the invariant breaking.
    let before_sever = min_pin;
    database.debug_raise_min_pin_with_snapshot_lsn_key();
    let severed_pin = database
        .free_pages_stats()
        .unwrap()
        .expect("ledger exists")
        .min_pin;
    assert!(
        severed_pin > before_sever && severed_pin > snapshot.catalog_generation(),
        "the wrong key must break the invariant ({before_sever} → {severed_pin}, \
         pinned generation {}); if this ever stops failing-when-severed, gate 2 \
         has lost its teeth",
        snapshot.catalog_generation()
    );
}

/// Reopen correctness over a reuse-churned file: the full read path — and
/// recovery — over pages that have cycled through the ledger.
#[test]
fn reuse_churned_file_reopens_with_exact_data() {
    let directory = TestDirectory::new();
    let path = directory.path.join("reopen.devondb");
    let mut database = create_keyed(&path);
    for cycle in 0..10 {
        churn_cycle(&mut database, cycle, 4);
    }
    let expected = scan_ids(&database);
    let stats = database.free_pages_stats().unwrap().expect("ledger exists");
    assert!(
        stats.session_reused > 0,
        "the file must actually have recycled pages"
    );
    drop(database);

    let reopened = Database::open(&path).unwrap();
    assert_eq!(scan_ids(&reopened), expected);
    assert!(
        !reopened.free_pages_stats().unwrap().unwrap().degraded,
        "a cleanly closed reuse-churned file reopens non-degraded"
    );
}
