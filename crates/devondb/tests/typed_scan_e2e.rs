//! Typed scan end-to-end (`docs/SCALE.md` §6.5/§6.7):
//! a table carrying every scalar type plus a Vector, NULLs spread across
//! three checkpointed node groups, and an uncheckpointed MVCC overlay
//! (updates + delete + inserts). Scan, filter, project, and aggregate
//! results must be byte-identical to the boxed row path.
//!
//! The expected values below are FNV-1a digests of the `Debug`
//! serialization of the full result rows, captured from the boxed row path
//! before the typed scan landed (freeze the behavior, then change the
//! machinery). Regenerate with:
//!
//! ```sh
//! DEVONDB_TYPED_SCAN_CAPTURE=1 cargo test -p devondb --test typed_scan_e2e -- --nocapture
//! ```

use std::{
    fs,
    path::PathBuf,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, Plan, Statement};
use devondb_plan::statement::SetItem;
use devondb_types::{
    Decimal128, GeoPoint, logical_type::LogicalType, schema::Column, value::Value,
};

const PAGE_SIZE: u32 = 4096;
const ROW_COUNT: usize = 4_196; // 2048 + 2048 + 100: three node groups.

/// The typed-group scan counter is process-global; tests reading it must
/// not run concurrently.
static SERIAL: Mutex<()> = Mutex::new(());

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-typed-scan-e2e-{}-{timestamp}-{sequence}",
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

struct Expectation {
    name: &'static str,
    query: &'static str,
    rows: usize,
    digest: u64,
}

/// Frozen from the boxed row path (see the module docs). The digests are
/// FNV-1a over `format!("{rows:?}")`.
const EXPECTED: &[Expectation] = &[
    Expectation {
        name: "scan_all",
        query: "nodes(Mixed) as m | project m.id, m.f, m.b, m.ts, m.dec, m.s, m.bin, m.j, m.v, m.g",
        rows: 4_198,
        digest: 0xf993_a95f_ae28_5d26,
    },
    Expectation {
        name: "projection",
        query: "nodes(Mixed) as m | project m.id, m.s",
        rows: 4_198,
        digest: 0xc5fd_626c_3826_5a37,
    },
    Expectation {
        name: "filter_shadowed_group",
        query: "nodes(Mixed) as m | filter m.id >= 1000 and m.id < 1010 | project m.id, m.f, m.s",
        rows: 10,
        digest: 0x9b26_1c81_996a_ba46,
    },
    Expectation {
        name: "filter_typed_group",
        query: "nodes(Mixed) as m | filter m.id >= 2048 and m.id < 2058 | project m.id, m.f",
        rows: 10,
        digest: 0x7ce2_af4d_55da_c590,
    },
    Expectation {
        name: "aggregate",
        query: "nodes(Mixed) as m | aggregate count(m.id) as n, sum(m.f) as total",
        rows: 1,
        digest: 0x8204_dc0e_c0b0_d2dd,
    },
];

/// One deterministic fixture row: NULLs in every non-key column at row
/// multiples of 97 (spreading NULLs across all three groups), an empty
/// string at row % 211 == 1.
fn fixture_row(row: usize) -> Vec<Value> {
    let null = row.is_multiple_of(97);
    let mut values = vec![Value::Int64(row as i64)];
    let mut push = |value: Value| values.push(if null { Value::Null } else { value });
    push(Value::Float64(row as f64 / 8.0 - 100.0));
    push(Value::Bool(row.is_multiple_of(2)));
    push(Value::Timestamp(row as i64 * 1_000_000));
    push(Value::Decimal(
        Decimal128::new(row as i128 * 13 - 7_000, 2).unwrap(),
    ));
    push(Value::String(if row % 211 == 1 {
        String::new()
    } else {
        format!("name-{row}")
    }));
    push(Value::Bytes(vec![(row % 256) as u8, (row % 7) as u8]));
    push(Value::Json(format!(r#"{{"row":{row}}}"#)));
    push(Value::Vector(vec![
        row as f32 / 100.0,
        0.5,
        -(row as f32) / 50.0,
    ]));
    push(Value::GeoPoint(
        GeoPoint::from_canonical(
            40.0 + (row % 90) as f64 / 100.0,
            -74.0 + (row % 90) as f64 / 100.0,
        )
        .unwrap(),
    ));
    values
}

/// Builds the fixture: three checkpointed groups, then an uncheckpointed
/// overlay — updates in groups 0 and 2, a delete in group 2, and fresh
/// inserts — so group 1 alone is untouched by MVCC deltas.
fn build_fixture(path: &std::path::Path) -> Database {
    let mut database = Database::create(path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Mixed".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("f", LogicalType::Float64, false),
                column("b", LogicalType::Bool, false),
                column("ts", LogicalType::Timestamp, false),
                column(
                    "dec",
                    LogicalType::Decimal {
                        precision: 12,
                        scale: 2,
                    },
                    false,
                ),
                column("s", LogicalType::String, false),
                column("bin", LogicalType::Bytes, false),
                column("j", LogicalType::Json, false),
                column("v", LogicalType::Vector { dim: 3 }, false),
                column("g", LogicalType::GeoPoint, false),
            ],
        })
        .unwrap();
    for chunk in (0..ROW_COUNT).collect::<Vec<_>>().chunks(500) {
        database
            .execute(&Statement::InsertNode {
                table: "Mixed".to_owned(),
                rows: chunk.iter().map(|&row| fixture_row(row)).collect(),
            })
            .unwrap();
    }
    database.checkpoint().unwrap();

    // Uncheckpointed overlay: shadow rows of groups 0 and 2, leave group 1
    // clean, and add brand-new rows.
    for (key, name) in [
        (5_i64, "updated-5"),
        (1000, "updated-1000"),
        (4100, "updated-4100"),
    ] {
        database
            .execute(&Statement::UpdateNode {
                table: "Mixed".to_owned(),
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
            table: "Mixed".to_owned(),
            key_column: "id".to_owned(),
            key: Value::Int64(4_150),
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "Mixed".to_owned(),
            rows: (ROW_COUNT..ROW_COUNT + 3).map(fixture_row).collect(),
        })
        .unwrap();
    database
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn plan(query: &str) -> Plan {
    let devondb::text::parser::Parsed::Query(plan) =
        devondb::text::parser::parse(query).expect("parse typed-scan e2e query")
    else {
        panic!("expected query plan: {query}");
    };
    plan
}

/// FNV-1a over the `Debug` serialization of the whole result: byte-level
/// equality between the frozen row path and the typed path.
fn digest_rows(rows: &[Vec<Value>]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in format!("{rows:?}").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[test]
fn typed_scan_matches_frozen_row_path_results() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new();
    let mut database = build_fixture(&directory.0.join("typed.devondb"));
    let capture = std::env::var_os("DEVONDB_TYPED_SCAN_CAPTURE").is_some();

    for expectation in EXPECTED {
        database.reset_typed_group_scan_count();
        let result = database.run(&plan(expectation.query)).unwrap();
        let typed_groups = database.typed_group_scan_count();
        let digest = digest_rows(&result.rows);
        if capture {
            println!(
                "CAPTURE {} rows={} digest={digest:#x}",
                expectation.name,
                result.rows.len()
            );
            continue;
        }
        assert_eq!(
            result.rows.len(),
            expectation.rows,
            "{} row count diverged from the frozen row path",
            expectation.name
        );
        assert_eq!(
            digest, expectation.digest,
            "{} results diverged from the frozen row path",
            expectation.name
        );

        // The typed path must actually run where groups are
        // clean, and the boxed fallback where the overlay shadows rows. The
        // fixture shadows groups 0 and 2 and leaves group 1 clean, and none
        // of these predicates is a single top-level comparison, so zone-map
        // pruning stays off (it only kicks in for one comparison — see
        // ZoneMapFilter::from_expression) and every group is visited.
        let expected_typed = 1;
        assert_eq!(
            typed_groups, expected_typed,
            "{} decoded {typed_groups} groups through the typed path, expected {expected_typed}",
            expectation.name
        );
    }
}

#[test]
fn clean_table_decodes_every_group_typed() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = TestDirectory::new();
    let path = directory.0.join("clean.devondb");
    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database
        .execute(&Statement::CreateNodeTable {
            name: "Clean".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("s", LogicalType::String, false),
            ],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "Clean".to_owned(),
            rows: (0..ROW_COUNT)
                .map(|row| {
                    vec![
                        Value::Int64(row as i64),
                        Value::String(format!("clean-{row}")),
                    ]
                })
                .collect(),
        })
        .unwrap();
    database.checkpoint().unwrap();

    database.reset_typed_group_scan_count();
    let result = database
        .run(&plan("nodes(Clean) as c | project c.id, c.s"))
        .unwrap();
    assert_eq!(result.rows.len(), ROW_COUNT);
    assert_eq!(
        database.typed_group_scan_count(),
        3,
        "a clean three-group table must decode every group through the typed path"
    );
    assert_eq!(
        result.rows[0],
        vec![Value::Int64(0), Value::String("clean-0".into())]
    );
    assert_eq!(
        result.rows[ROW_COUNT - 1],
        vec![
            Value::Int64((ROW_COUNT - 1) as i64),
            Value::String(format!("clean-{}", ROW_COUNT - 1))
        ]
    );

    // A single-comparison filter activates zone-map pruning: group 0 (ids
    // 0..=2047) is skipped without decoding, the two surviving clean groups
    // decode typed. (Zone-map pruning × the typed path is further fenced by
    // the page-counter asserts in `zone_maps.rs`.)
    database.reset_typed_group_scan_count();
    let pruned = database
        .run(&plan(
            "nodes(Clean) as c | filter c.id >= 3000 | project c.id",
        ))
        .unwrap();
    assert_eq!(pruned.rows.len(), ROW_COUNT - 3000);
    assert_eq!(
        database.typed_group_scan_count(),
        2,
        "the pruned scan must decode exactly the two surviving groups, typed"
    );
    assert_eq!(pruned.rows[0], vec![Value::Int64(3000)]);
}
