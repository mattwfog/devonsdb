use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use devondb::{Database, DevonError, Options, Plan, Statement};
use devondb_plan::{
    expr::Expr,
    ops::{Direction, Operator, ProjectionItem},
    statement::RelRow,
};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

const PAGE_SIZE: u32 = 4096;
const TIGHT_LIMIT: usize = 1024 * 1024;
const SUFFICIENT_LIMIT: usize = 8 * 1024 * 1024;
const BUDGET_ROWS: usize = 4_096;
const OVERLAY_ROWS: usize = 20_000;
const OVERLAY_BATCH: usize = 500;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-read-budget-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn database(&self) -> PathBuf {
        self.0.join("graph.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn expand_working_sets_respect_budget_and_release_after_failure() {
    let directory = TestDirectory::new("expand");
    let path = directory.database();
    seed_budget_graph(&path);

    let mut tight = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: TIGHT_LIMIT,
        },
    )
    .unwrap();
    let error = tight.run(&expand_plan()).unwrap_err();
    assert!(matches!(error, DevonError::BudgetExceeded { .. }));
    let message = error.to_string();
    assert!(
        message.contains("working set") && (message.contains("scan") || message.contains("expand")),
        "budget failure must name the scan/expand working set: {message}"
    );

    let small = tight.run(&limited_scan_plan()).unwrap();
    assert_eq!(
        small.rows.len(),
        1,
        "failed pipeline leaked its budget charge"
    );
    drop(tight);

    let mut sufficient = Database::open_with(
        &path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit: SUFFICIENT_LIMIT,
        },
    )
    .unwrap();
    let result = sufficient.run(&expand_plan()).unwrap();
    assert_eq!(result.rows.len(), BUDGET_ROWS);
}

#[test]
fn overlay_endpoint_resolution_stays_inside_wall_clock_ceiling() {
    let directory = TestDirectory::new("overlay-keys");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_schema(&mut database, false);

    for start in (0..OVERLAY_ROWS).step_by(OVERLAY_BATCH) {
        let end = (start + OVERLAY_BATCH).min(OVERLAY_ROWS);
        database
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: (start..end)
                    .map(|id| vec![Value::Int64(id as i64)])
                    .collect(),
            })
            .unwrap();
    }

    let rows = (0..OVERLAY_ROWS)
        .map(|id| RelRow {
            from_key: Value::Int64(id as i64),
            to_key: Value::Int64(((id + 1) % OVERLAY_ROWS) as i64),
            values: Vec::new(),
        })
        .collect();
    let started = Instant::now();
    database
        .execute(&Statement::InsertRel {
            table: "Knows".to_owned(),
            rows,
        })
        .unwrap();
    let elapsed = started.elapsed();
    let ceiling = Duration::from_secs(20);
    assert!(
        elapsed < ceiling,
        "resolving {OVERLAY_ROWS} overlay-backed edges took {elapsed:?}, ceiling {ceiling:?}"
    );
}

#[test]
fn user_alias_cannot_forge_an_internal_offset_column() {
    let directory = TestDirectory::new("offset-alias");
    let path = directory.database();
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    create_schema(&mut database, false);
    database
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: vec![vec![Value::Int64(1)], vec![Value::Int64(2)]],
        })
        .unwrap();
    database
        .execute(&Statement::InsertRel {
            table: "Knows".to_owned(),
            rows: vec![RelRow {
                from_key: Value::Int64(1),
                to_key: Value::Int64(2),
                values: Vec::new(),
            }],
        })
        .unwrap();

    let plan = Plan {
        v: 0,
        plan: Operator::Expand {
            rel: "Knows".to_owned(),
            direction: Direction::Out,
            from_binding: "person".to_owned(),
            binding: "friend".to_owned(),
            input: Box::new(Operator::Project {
                exprs: vec![ProjectionItem {
                    expr: Expr::Col("person.id".to_owned()),
                    alias: "person.#offset".to_owned(),
                }],
                input: Box::new(Operator::ScanNodes {
                    table: "Person".to_owned(),
                    binding: "person".to_owned(),
                }),
            }),
        },
    };
    let result = database.run(&plan).unwrap();
    assert_eq!(
        result.columns,
        vec!["person.#offset".to_owned(), "friend.id".to_owned()]
    );
    assert_eq!(result.rows, vec![vec![Value::Int64(1), Value::Int64(2)]]);
}

fn seed_budget_graph(path: &PathBuf) {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    create_schema(&mut database, true);
    let payload = "x".repeat(128);
    database
        .execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: (0..BUDGET_ROWS)
                .map(|id| vec![Value::Int64(id as i64), Value::String(payload.clone())])
                .collect(),
        })
        .unwrap();
    database
        .execute(&Statement::InsertRel {
            table: "Knows".to_owned(),
            rows: (0..BUDGET_ROWS)
                .map(|id| RelRow {
                    from_key: Value::Int64(id as i64),
                    to_key: Value::Int64(((id + 1) % BUDGET_ROWS) as i64),
                    values: Vec::new(),
                })
                .collect(),
        })
        .unwrap();
    database.checkpoint().unwrap();
}

fn create_schema(database: &mut Database, with_payload: bool) {
    let mut columns = vec![Column {
        name: "id".to_owned(),
        ty: LogicalType::Int64,
        primary_key: true,
    }];
    if with_payload {
        columns.push(Column {
            name: "payload".to_owned(),
            ty: LogicalType::String,
            primary_key: false,
        });
    }
    database
        .execute(&Statement::CreateNodeTable {
            name: "Person".to_owned(),
            columns,
        })
        .unwrap();
    database
        .execute(&Statement::CreateRelTable {
            name: "Knows".to_owned(),
            from: "Person".to_owned(),
            to: "Person".to_owned(),
            columns: Vec::new(),
        })
        .unwrap();
}

fn expand_plan() -> Plan {
    Plan {
        v: 0,
        plan: Operator::Expand {
            rel: "Knows".to_owned(),
            direction: Direction::Out,
            from_binding: "person".to_owned(),
            binding: "friend".to_owned(),
            input: Box::new(Operator::ScanNodes {
                table: "Person".to_owned(),
                binding: "person".to_owned(),
            }),
        },
    }
}

fn limited_scan_plan() -> Plan {
    Plan {
        v: 0,
        plan: Operator::Limit {
            count: 1,
            offset: None,
            input: Box::new(Operator::ScanNodes {
                table: "Person".to_owned(),
                binding: "person".to_owned(),
            }),
        },
    }
}
