//! Projection pushdown end-to-end: the facade computes the
//! columns a plan references, the scan decodes only those (plus the primary
//! key and the hidden offset), and every downstream column index is remapped
//! through the same set. Results must be identical to the full decode, and a
//! 1-of-6 projection must read fewer payload pages, two-sided.
//!
//! Fixture shape mirrors `typed_scan_e2e.rs`: a 6-column table across three
//! checkpointed node groups plus an uncheckpointed MVCC overlay (updates, a
//! delete, fresh inserts). Ground truth is the full decode itself — a plan
//! whose root passes the scan's binding through (`nodes(T) as t`, or the
//! same shape without its trailing `project`) reads every column.

use std::{
    fs,
    path::PathBuf,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, QueryResult, Statement};
use devondb_plan::{
    expr::Expr,
    ops::{Operator, ProjectionItem},
    statement::SetItem,
};
use devondb_types::{Decimal128, value::Value};

const PAGE_SIZE: u32 = 4096;
const ROW_COUNT: usize = 4_196; // 2048 + 2048 + 100: three node groups.
const OVERLAY_INSERTS: usize = 3;
const VISIBLE_ROWS: usize = ROW_COUNT + OVERLAY_INSERTS - 1; // one overlay delete.

/// The typed-group scan counter is process-global; every test in this file
/// scans, so all of them serialize on this lock to keep the counter (and
/// the page counters) attributable to one query at a time.
static SERIAL: Mutex<()> = Mutex::new(());

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
            "devondb-projection-pushdown-{label}-{timestamp}-{sequence}-{}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run(database: &mut Database, query: &str) -> QueryResult {
    let devondb::text::parser::Parsed::Query(plan) =
        devondb::text::parser::parse(query).expect("parse projection-pushdown query")
    else {
        panic!("expected query plan: {query}");
    };
    database.run(&plan).unwrap()
}

fn execute(database: &mut Database, input: &str) {
    let devondb::text::parser::Parsed::Statement(statement) =
        devondb::text::parser::parse(input).expect("statement parses")
    else {
        panic!("expected statement: {input}");
    };
    database.execute(&statement.stmt).unwrap();
}

/// Picks `indices` out of each row, in the given order.
fn pick(rows: &[Vec<Value>], indices: &[usize]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| indices.iter().map(|&index| row[index].clone()).collect())
        .collect()
}

/// One deterministic fixture row: NULLs in `f`, `s`, and `v` at row
/// multiples of 97 (spreading NULLs across all three groups); `a` is a
/// unique-per-row Int64; `b` never NULL (filter/sort/aggregate models stay
/// exact).
fn wide_row(row: usize) -> Vec<Value> {
    let null = row.is_multiple_of(97);
    let mut values = vec![
        Value::Int64(row as i64),
        Value::Int64((row * 2503 % 8191) as i64),
    ];
    {
        let mut push = |value: Value| values.push(if null { Value::Null } else { value });
        push(Value::Float64(row as f64 / 8.0 - 100.0));
        push(Value::String(format!("name-{row}")));
    }
    values.push(Value::Bool(row.is_multiple_of(2)));
    values.push(if null {
        Value::Null
    } else {
        Value::Vector(vec![row as f32 / 100.0, 0.5])
    });
    values
}

/// The 6-column table across three groups plus an uncheckpointed overlay:
/// updates in groups 0 and 2, a delete in group 2, and fresh inserts — so
/// group 1 alone is untouched by MVCC deltas (the typed_scan_e2e shape).
/// With `mutations: false` the overlay is insert-only, as expand requires
/// destination rows prefix-identical to the checkpointed rows.
fn build_wide_fixture(path: &std::path::Path, mutations: bool) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    execute(
        &mut database,
        "create node table Wide (id Int64 primary key, a Int64, f Float64, s String, b Bool, v Vector(2))",
    );
    for chunk in (0..ROW_COUNT).collect::<Vec<_>>().chunks(500) {
        database
            .execute(&Statement::InsertNode {
                table: "Wide".to_owned(),
                rows: chunk.iter().map(|&row| wide_row(row)).collect(),
            })
            .unwrap();
    }
    database.checkpoint().unwrap();

    if mutations {
        for (key, name) in [(5_i64, "updated-5"), (4100, "updated-4100")] {
            database
                .execute(&Statement::UpdateNode {
                    table: "Wide".to_owned(),
                    set: vec![
                        SetItem {
                            column: "s".to_owned(),
                            value: Value::String(name.to_owned()),
                        },
                        SetItem {
                            column: "f".to_owned(),
                            value: Value::Float64(key as f64 + 0.5),
                        },
                    ],
                    key_column: "id".to_owned(),
                    key: Value::Int64(key),
                })
                .unwrap();
        }
        database
            .execute(&Statement::DeleteNode {
                table: "Wide".to_owned(),
                key_column: "id".to_owned(),
                key: Value::Int64(4_150),
            })
            .unwrap();
    }
    database
        .execute(&Statement::InsertNode {
            table: "Wide".to_owned(),
            rows: (ROW_COUNT..ROW_COUNT + OVERLAY_INSERTS)
                .map(wide_row)
                .collect(),
        })
        .unwrap();
    // The rel table comes after the delete: detach-delete is unsupported,
    // so a relationship endpoint table cannot see node deletes.
    execute(
        &mut database,
        "create rel table Link from Wide to Wide (w Int64)",
    );
    database
        .execute(&Statement::InsertRel {
            table: "Link".to_owned(),
            rows: [(0, 1, 10), (1, 2, 20), (2, 3, 30), (3, 0, 40)]
                .iter()
                .map(|(from, to, w)| devondb_plan::statement::RelRow {
                    from_key: Value::Int64(*from),
                    to_key: Value::Int64(*to),
                    values: vec![Value::Int64(*w)],
                })
                .collect(),
        })
        .unwrap();
    database
}

fn interface_plan(exprs: Option<Vec<ProjectionItem>>) -> devondb::Plan {
    let scan = Operator::ScanInterface {
        interface: "Named".to_owned(),
        binding: "n".to_owned(),
    };
    devondb::Plan {
        v: 0,
        plan: match exprs {
            Some(exprs) => Operator::Project {
                exprs,
                input: Box::new(scan),
            },
            None => scan,
        },
    }
}

/// Full decode of the fixture: the bare scan's root exposes every column,
/// so pushdown stays off and all six columns are read.
fn full_rows(database: &mut Database) -> Vec<Vec<Value>> {
    let result = run(database, "nodes(Wide) as w");
    assert_eq!(
        result.columns,
        ["w.id", "w.a", "w.f", "w.s", "w.b", "w.v"],
        "the bare scan must keep exposing the full column list"
    );
    assert_eq!(result.rows.len(), VISIBLE_ROWS);
    result.rows
}

#[test]
fn filter_late_column_project_early_column_matches_full_decode() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("remap");
    let mut database = build_wide_fixture(&directory.0.join("remap.devondb"), true);

    // The index-remapping proof: the predicate reads a LATE column (b,
    // table index 4), the projection an EARLY one (a, table index 1); the
    // scan decodes {id, a, b} and both expressions must resolve through the
    // remapped positions.
    let reference = run(&mut database, "nodes(Wide) as w | filter w.b = true");
    let pushed = run(
        &mut database,
        "nodes(Wide) as w | filter w.b = true | project w.a",
    );
    assert_eq!(pushed.columns, ["w.a"]);
    assert_eq!(pushed.rows, pick(&reference.rows, &[1]));
}

#[test]
fn project_sort_limit_and_bare_scan_match_full_decode() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("project");
    let mut database = build_wide_fixture(&directory.0.join("project.devondb"), true);
    let full = full_rows(&mut database);

    let pushed = run(
        &mut database,
        "nodes(Wide) as w | project w.s, w.f | limit 25",
    );
    assert_eq!(pushed.rows, pick(&full[..25], &[3, 2]));

    // `a` is unique per row, so the desc sort is tie-free and the pushed
    // pipeline must reproduce the full decode's order exactly.
    let reference = run(&mut database, "nodes(Wide) as w | sort w.a desc | limit 20");
    let pushed = run(
        &mut database,
        "nodes(Wide) as w | sort w.a desc | project w.id, w.a | limit 20",
    );
    assert_eq!(pushed.rows, pick(&reference.rows, &[0, 1]));
    assert!(
        pushed
            .rows
            .windows(2)
            .all(|pair| match (&pair[0][1], &pair[1][1]) {
                (Value::Int64(higher), Value::Int64(lower)) => higher > lower,
                (left, right) => panic!("column a is never NULL: {left}, {right}"),
            })
    );
}

#[test]
fn aggregate_and_group_keys_match_full_decode_model() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("aggregate");
    let mut database = build_wide_fixture(&directory.0.join("aggregate.devondb"), true);
    let full = full_rows(&mut database);

    let expected_count = full.len() as i64;
    let expected_sum: i64 = full
        .iter()
        .map(|row| match row[1] {
            Value::Int64(a) => a,
            ref other => panic!("column a is never NULL: {other}"),
        })
        .sum();
    let floats = full.iter().filter_map(|row| match row[2] {
        Value::Float64(f) => Some(f),
        _ => None,
    });
    let expected_min = floats.clone().fold(f64::INFINITY, f64::min);
    let expected_max = floats.fold(f64::NEG_INFINITY, f64::max);

    let pushed = run(
        &mut database,
        "nodes(Wide) as w | aggregate count(w.id) as n, sum(w.a) as total, min(w.f) as lo, max(w.f) as hi",
    );
    assert_eq!(
        pushed.rows,
        vec![vec![
            Value::Int64(expected_count),
            Value::Int64(expected_sum),
            Value::Float64(expected_min),
            Value::Float64(expected_max),
        ]]
    );

    // Group keys narrow the scan too: only id and b are read.
    let grouped = run(
        &mut database,
        "nodes(Wide) as w | aggregate count(w.id) as n by w.b",
    );
    let mut expected: Vec<Vec<Value>> = vec![
        vec![
            Value::Bool(true),
            Value::Int64(
                full.iter()
                    .filter(|row| row[4] == Value::Bool(true))
                    .count() as i64,
            ),
        ],
        vec![
            Value::Bool(false),
            Value::Int64(
                full.iter()
                    .filter(|row| row[4] == Value::Bool(false))
                    .count() as i64,
            ),
        ],
    ];
    let mut actual = grouped.rows;
    let key = |rows: &mut Vec<Vec<Value>>| rows.sort_by_key(|row| format!("{row:?}"));
    key(&mut expected);
    key(&mut actual);
    assert_eq!(actual, expected);
}

#[test]
fn expand_matches_full_decode() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("expand");
    let mut database = build_wide_fixture(&directory.0.join("expand.devondb"), false);

    // The from-binding's scan narrows to {id} (its offset is synthesized);
    // the neighbor side also narrows to its referenced columns.
    let reference = run(
        &mut database,
        "nodes(Wide) as w | filter w.id >= 0 and w.id < 4 | expand Link out as n",
    );
    let pushed = run(
        &mut database,
        "nodes(Wide) as w | filter w.id >= 0 and w.id < 4 | expand Link out as n | project w.id, n.id, n.s",
    );
    assert_eq!(pushed.columns, ["w.id", "n.id", "n.s"]);
    // Full expand row: w's six columns then n's six columns.
    assert_eq!(pushed.rows, pick(&reference.rows, &[0, 6, 9]));
    assert_eq!(pushed.rows.len(), 4);
}

#[test]
fn knn_matches_full_decode() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("knn");
    let mut database = build_wide_fixture(&directory.0.join("knn.devondb"), true);

    // A bare KnnScan root exposes every column plus `distance` — the full
    // decode. The projected form narrows the exact scan to {id, v}.
    let reference = run(&mut database, "knn(Wide.v, [0.25, 1.5], 5, l2)");
    assert_eq!(reference.rows.len(), 5);
    // (`distance` is a parser keyword in projection position, so the id
    // column carries the equality proof; the scan narrows to {id, v}.)
    let pushed = run(
        &mut database,
        "knn(Wide.v, [0.25, 1.5], 5, l2) | project Wide.id",
    );
    assert_eq!(pushed.rows, pick(&reference.rows, &[0]));
}

#[test]
fn scalar_subqueries_match_full_decode() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("scalar");
    let mut database = build_wide_fixture(&directory.0.join("scalar.devondb"), true);

    // Uncorrelated: the inner plan scans the same table under its own
    // binding; the outer scan narrows to {id, f, s}.
    let reference = run(
        &mut database,
        "nodes(Wide) as w | filter w.f < scalar(nodes(Wide) as m | aggregate avg(m.f) as mean)",
    );
    let pushed = run(
        &mut database,
        "nodes(Wide) as w | filter w.f < scalar(nodes(Wide) as m | aggregate avg(m.f) as mean) | project w.id, w.s",
    );
    assert!(!reference.rows.is_empty());
    assert_eq!(pushed.rows, pick(&reference.rows, &[0, 3]));

    // Correlated: `w.b` inside the subquery is supplied by the OUTER scan —
    // the analysis must collect it there or the pipeline has no column to
    // substitute from. (The id pre-filter keeps the per-row subquery cheap.)
    let reference = run(
        &mut database,
        "nodes(Wide) as w | filter w.id >= 4000 and w.id < 4200 | filter w.f > scalar(nodes(Wide) as inner | filter inner.b = w.b | aggregate avg(inner.f) as m)",
    );
    let pushed = run(
        &mut database,
        "nodes(Wide) as w | filter w.id >= 4000 and w.id < 4200 | filter w.f > scalar(nodes(Wide) as inner | filter inner.b = w.b | aggregate avg(inner.f) as m) | project w.id",
    );
    assert!(!reference.rows.is_empty());
    assert_eq!(pushed.rows, pick(&reference.rows, &[0]));
}

#[test]
fn interface_scan_matches_full_decode_per_implementing_table() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("interface");
    let mut database = Database::create(directory.0.join("interface.devondb"), PAGE_SIZE).unwrap();
    // Different column order and an extra unreferenced column per class:
    // the per-table remapping must hold for each implementing scan.
    execute(
        &mut database,
        "create node table A (id Int64 primary key, s String, f Float64, payload String)",
    );
    execute(
        &mut database,
        "create node table B (id Int64 primary key, f Float64, s String)",
    );
    execute(
        &mut database,
        "create interface Named (s String, f Float64)",
    );
    execute(&mut database, "create class for A (implements (Named))");
    execute(&mut database, "create class for B (implements (Named))");
    execute(
        &mut database,
        "insert into A values (1, \"a1\", 1.5, \"p1\"), (2, \"a2\", 2.5, \"p2\")",
    );
    execute(
        &mut database,
        "insert into B values (10, 10.5, \"b1\"), (11, 11.5, \"b2\")",
    );
    database.checkpoint().unwrap();
    // Overlay: an update on A and a fresh insert on B.
    database
        .execute(&Statement::UpdateNode {
            table: "A".to_owned(),
            set: vec![SetItem {
                column: "s".to_owned(),
                value: Value::String("a1-updated".to_owned()),
            }],
            key_column: "id".to_owned(),
            key: Value::Int64(1),
        })
        .unwrap();
    execute(&mut database, "insert into B values (12, 12.5, \"b3\")");

    // Root interface scan: every interface column, full decode per table.
    // (Interface scans parse from text only with a schema-aware parser, so
    // these plans are built directly, as in scan_interface_e2e.)
    let reference = database.run(&interface_plan(None)).unwrap();
    assert_eq!(reference.columns, ["n.s", "n.f"]);
    assert_eq!(reference.rows.len(), 5);

    // Projecting one interface column narrows each implementing table's
    // scan to {f, pk} — A skips s+payload, B skips s.
    let pushed = database
        .run(&interface_plan(Some(vec![ProjectionItem {
            expr: Expr::Col("n.f".to_owned()),
            alias: "f".to_owned(),
        }])))
        .unwrap();
    assert_eq!(pushed.rows, pick(&reference.rows, &[1]));

    // classof(binding) is unclassifiable as a column reference: it forces
    // the full interface declaration, and results still match.
    let classof = database
        .run(&interface_plan(Some(vec![
            ProjectionItem {
                expr: Expr::Col("n.s".to_owned()),
                alias: "s".to_owned(),
            },
            ProjectionItem {
                expr: Expr::ClassOf("n".to_owned()),
                alias: "class".to_owned(),
            },
        ])))
        .unwrap();
    let expected: Vec<Vec<Value>> = reference
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            vec![
                row[0].clone(),
                Value::String(if index < 2 { "A" } else { "B" }.to_owned()),
            ]
        })
        .collect();
    assert_eq!(classof.rows, expected);
}

#[test]
fn one_of_six_projection_reads_fewer_payload_pages_two_sided() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("pages");
    let path = directory.0.join("pages.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    // Three fat string columns make payload pages dominate directory reads,
    // so the pushdown delta dwarfs the per-column directory re-reads.
    execute(
        &mut database,
        "create node table Pages (id Int64 primary key, a Int64, x String, y String, z String, f Float64)",
    );
    let fat = |row: usize, tag: &str| format!("{tag}-{row}-{}", "y".repeat(160));
    for chunk in (0..ROW_COUNT).collect::<Vec<_>>().chunks(500) {
        database
            .execute(&Statement::InsertNode {
                table: "Pages".to_owned(),
                rows: chunk
                    .iter()
                    .map(|&row| {
                        vec![
                            Value::Int64(row as i64),
                            Value::Int64((row * 7) as i64),
                            Value::String(fat(row, "x")),
                            Value::String(fat(row, "y")),
                            Value::String(fat(row, "z")),
                            Value::Float64(row as f64),
                        ]
                    })
                    .collect(),
            })
            .unwrap();
    }
    database.checkpoint().unwrap();

    database.reset_page_read_count();
    let full = run(&mut database, "nodes(Pages) as p");
    let full_reads = database.page_read_count();

    database.reset_typed_group_scan_count();
    database.reset_page_read_count();
    let one = run(&mut database, "nodes(Pages) as p | project p.a");
    let one_reads = database.page_read_count();
    let one_typed_groups = database.typed_group_scan_count();
    assert_eq!(one.rows, pick(&full.rows, &[1]));

    // Projecting all six columns normalizes to the full decode, providing the
    // unchanged comparison path.
    database.reset_page_read_count();
    let all = run(
        &mut database,
        "nodes(Pages) as p | project p.id, p.a, p.x, p.y, p.z, p.f",
    );
    let all_reads = database.page_read_count();
    assert_eq!(all.rows, full.rows);

    assert!(
        one_reads < full_reads,
        "1-of-6 projection read {one_reads} pages, full scan read {full_reads}"
    );
    assert!(
        all_reads > one_reads,
        "all-column projection read {all_reads} pages, 1-of-6 projection read {one_reads}"
    );
    assert_eq!(
        all_reads, full_reads,
        "projecting every column must take the same decode path as the bare scan"
    );
    // Counter-proof: the narrowed decode still goes through the typed
    // column path for all three clean groups (docs/SCALE.md §6.5).
    assert_eq!(
        one_typed_groups, 3,
        "the narrowed scan must decode all three clean groups through the typed path"
    );
}

/// The integration half of the exhaustive-variant guard (the structural
/// half — every `Expr`/`Operator` variant classified — is the unit test in
/// `src/database/projection.rs`): one runnable plan using every `Expr`
/// variant over a table with an unreferenced column. The reference plan
/// additionally projects that column, forcing the full decode; shared
/// output columns must match exactly.
#[test]
fn every_expression_variant_runs_under_pushdown_and_matches() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("kitchen-sink");
    let mut database =
        Database::create(directory.0.join("kitchen-sink.devondb"), PAGE_SIZE).unwrap();
    execute(
        &mut database,
        "create node table Expr8 (id Int64 primary key, f Float64, b Bool, ts Timestamp, dec Decimal(12, 2), s String, v Vector(2), payload String)",
    );
    database
        .execute(&Statement::InsertNode {
            table: "Expr8".to_owned(),
            rows: (0..50)
                .map(|row| {
                    vec![
                        Value::Int64(row),
                        Value::Float64(row as f64 / 4.0 - 3.0),
                        Value::Bool(row % 2 == 0),
                        Value::Timestamp(row * 86_400_000_000 + 5_000_000),
                        Value::Decimal(Decimal128::new(row as i128 * 13 + 1, 2).unwrap()),
                        Value::String(format!("s-{row}")),
                        Value::Vector(vec![row as f32, 1.0]),
                        Value::String(format!("payload-{row}")),
                    ]
                })
                .collect(),
        })
        .unwrap();
    database.checkpoint().unwrap();

    const EXPRESSIONS: &str = "e.id as id, \
        e.f + 1.0 as add, e.f - 1.0 as sub, e.f * 2.0 as mul, e.f / 2.0 as div, \
        not e.b as nb, \
        if(e.b, e.f, 0.0) as cond, \
        coalesce(e.s, \"fallback\") as c, \
        least(e.f, 5.0) as lo, greatest(e.f, 1.0) as hi, \
        date_trunc(\"day\", e.ts) as day, date_add(\"day\", e.ts, 1) as nxt, \
        round(e.dec, 1) as r, round_div(e.dec, decimal(\"2.00\"), 1) as rd, \
        distance(e.v, [1.0, 2.0], l2) as dist, \
        scalar(nodes(Expr8) as m | aggregate count(m.id) as n) as cnt";
    let pushed = run(
        &mut database,
        &format!("nodes(Expr8) as e | project {EXPRESSIONS}"),
    );
    let full = run(
        &mut database,
        &format!("nodes(Expr8) as e | project {EXPRESSIONS}, e.payload as payload"),
    );
    assert_eq!(pushed.rows.len(), 50);
    assert_eq!(full.columns.len(), pushed.columns.len() + 1);
    for (pushed_row, full_row) in pushed.rows.iter().zip(&full.rows) {
        assert_eq!(pushed_row, &&full_row[..pushed_row.len()]);
    }
    // Spot-check semantics survived: row 0 arithmetic and the scalar count.
    assert_eq!(pushed.rows[0][0], Value::Int64(0));
    assert_eq!(pushed.rows[0][1], Value::Float64(-2.0));
    assert_eq!(pushed.rows[0][15], Value::Int64(50));
}

fn neighbor_plan(text: &str) -> devondb::Plan {
    let devondb::text::parser::Parsed::Query(plan) = devondb::text::parser::parse(text).unwrap()
    else {
        panic!("expected query");
    };
    plan
}

fn wide_neighbor_database(path: &std::path::Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    execute(
        &mut database,
        "create node table Payload (id Int64 primary key, payload Bytes, label String)",
    );
    execute(
        &mut database,
        "create rel table Link from Payload to Payload (weight Int64)",
    );
    database
        .execute(&Statement::InsertNode {
            table: "Payload".into(),
            rows: (0..128)
                .map(|id| {
                    vec![
                        Value::Int64(id),
                        Value::Bytes(vec![id as u8; 16_384]),
                        Value::String(format!("row-{id}")),
                    ]
                })
                .collect(),
        })
        .unwrap();
    for id in 0..128 {
        execute(
            &mut database,
            &format!(
                "insert rel into Link values ({id} -> {}, 1)",
                (id + 1) % 128
            ),
        );
    }
    execute(
        &mut database,
        "create node table Tiny (id Int64 primary key)",
    );
    execute(&mut database, "insert into Tiny values (0)");
    execute(
        &mut database,
        "create rel table ToTiny from Payload to Tiny (weight Int64)",
    );
    for id in 0..128 {
        execute(
            &mut database,
            &format!("insert rel into ToTiny values ({id} -> 0, 1)"),
        );
    }
    database.checkpoint().unwrap();
    database
}

#[test]
fn expand_neighbor_projection_reclaims_unused_wide_payload_budget() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("neighbor-budget");
    let path = directory.0.join("wide.devondb");
    let mut reference = wide_neighbor_database(&path);
    reference.reset_page_read_count();
    assert_eq!(run(&mut reference, "nodes(Payload) as p | expand Link out as n | project n.id, n.payload, n.label | aggregate count(n.id) as total").rows,
        vec![vec![Value::Int64(128)]]);
    let full_reads = reference.page_read_count();
    drop(reference);
    const LIMIT: usize = 1024 * 1024;
    let mut database = Database::open_with(
        &path,
        devondb::Options {
            page_size: PAGE_SIZE,
            memory_limit: LIMIT,
        },
    )
    .unwrap();
    for direction in ["out", "in"] {
        database.reset_page_read_count();
        let rows = run(
            &mut database,
            &format!(
                "nodes(Payload) as p | expand Link {direction} as n | aggregate count(n.id) as total"
            ),
        );
        assert_eq!(rows.rows, vec![vec![Value::Int64(128)]]);
        assert!(
            database.page_read_count() < full_reads,
            "narrowed destination read {} pages versus full destination {full_reads}",
            database.page_read_count()
        );
        assert!(database.memory_budget().charged() <= LIMIT);
    }
    let error = database
        .run(&neighbor_plan(
            "nodes(Payload) as p | expand Link out as n | project n.payload",
        ))
        .unwrap_err();
    assert!(
        matches!(error, devondb::DevonError::BudgetExceeded { .. }),
        "{error}"
    );
    assert_eq!(
        run(
            &mut database,
            "nodes(Payload) as p | expand Link out as n | aggregate count(n.id) as total"
        )
        .rows,
        vec![vec![Value::Int64(128)]]
    );
}

#[test]
fn projected_neighbors_preserve_slots_snapshots_chains_and_bare_columns() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("neighbor-mvcc");
    let path = directory.0.join("wide.devondb");
    let mut database = wide_neighbor_database(&path);
    let count = neighbor_plan(
        "nodes(Payload) as p | expand Link out as n | expand Link out as q | aggregate count(q.id) as total",
    );
    let snapshot = database.snapshot();
    assert_eq!(
        database.run(&count).unwrap().rows,
        vec![vec![Value::Int64(128)]]
    );
    execute(
        &mut database,
        "update Payload set label = \"updated\" where id = 2",
    );
    execute(&mut database, "detach delete from Payload where id = 1");
    execute(
        &mut database,
        "insert into Payload values (1, bytes(\"ab\"), \"replacement\")",
    );
    execute(
        &mut database,
        "insert rel into Link values (0 -> 1, 7), (1 -> 2, 9), (1 -> 2, 11)",
    );
    let query = "nodes(Payload) as p | filter p.id = 0 | expand Link out as n | expand Link out as q | project n.label, q.label";
    let expected = vec![
        vec![
            Value::String("replacement".into()),
            Value::String("updated".into())
        ];
        2
    ];
    assert_eq!(run(&mut database, query).rows, expected);
    let bare = run(
        &mut database,
        "nodes(Payload) as p | filter p.id = 0 | expand Link out as n",
    );
    assert_eq!(
        bare.columns,
        [
            "p.id",
            "p.payload",
            "p.label",
            "n.id",
            "n.payload",
            "n.label"
        ]
    );
    assert_eq!(bare.rows[0][4], Value::Bytes(vec![0xab]));
    let bare_rel = run(
        &mut database,
        "nodes(Payload) as p | filter p.id = 0 | expand_rel ToTiny out as n via e",
    );
    assert_eq!(bare_rel.columns.last().unwrap(), "e.weight");
    assert_eq!(bare_rel.rows[0].last().unwrap(), &Value::Int64(1));
    database.checkpoint().unwrap();
    assert_eq!(
        snapshot.run(&count).unwrap().rows,
        vec![vec![Value::Int64(128)]]
    );
    assert_eq!(run(&mut database, query).rows, expected);
    drop(snapshot);
    drop(database);
    assert_eq!(
        run(&mut Database::open(&path).unwrap(), query).rows,
        expected
    );
}

#[test]
fn narrow_expand_feeds_property_expansion_with_the_same_budget() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new("neighbor-properties-budget");
    let path = directory.0.join("wide.devondb");
    drop(wide_neighbor_database(&path));
    let mut database = Database::open_with(
        &path,
        devondb::Options {
            page_size: PAGE_SIZE,
            memory_limit: 2 * 1024 * 1024,
        },
    )
    .unwrap();
    let result = run(
        &mut database,
        "nodes(Payload) as p | expand Link out as n | expand_rel ToTiny out as t via e | aggregate sum(e.weight) as total",
    );
    assert_eq!(result.rows, vec![vec![Value::Int64(128)]]);
}
