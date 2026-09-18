//! The GeoPoint seam, end to end (`docs/GEO.md` §5 SEAM STATUS): the value
//! type works through the real text→execute path, including exact
//! `WithinScan` execution and the remaining documented rejections.

use std::{
    env, fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, Statement};
use devondb_geo::covering::{MAX_CENTER_TO_BOUNDARY_M, great_circle_meters};
use devondb_plan::text::parser::{Parsed, parse};
use devondb_types::{GeoPoint, value::Value};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "devondb-geo-seam-{label}-{timestamp}-{sequence}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }

    fn db_path(&self) -> PathBuf {
        self.path.join("db.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn parsed_statement(text: &str) -> Statement {
    let Parsed::Statement(envelope) = parse(text).expect("statement parses") else {
        panic!("expected a statement: {text}");
    };
    envelope.stmt
}

/// GeoPoint node columns are live end to end (`docs/GEO.md` §5): text DDL,
/// inserts with nulls, WAL-recovered reopen, and checkpointed node groups
/// all round-trip the canonical values; the raw file carries feature bit 4.
/// Relationship properties stay rejected.
#[test]
fn geo_point_columns_live_end_to_end() {
    let directory = TestDirectory::new("live");
    let path = directory.db_path();
    let mut database = Database::create(&path, 4096).expect("create database");

    database
        .execute(&parsed_statement(
            "create node table Place (id Int64 primary key, loc GeoPoint)",
        ))
        .expect("GeoPoint node DDL is live");
    database
        .execute(&parsed_statement(
            "insert into Place values (1, geo(45.5, -122.625)), (2, null), (3, geo(-90.0, 0.0))",
        ))
        .expect("insert GeoPoint rows");

    database
        .execute(&parsed_statement(
            "create node table Person (id Int64 primary key)",
        ))
        .expect("create rel endpoint table");
    // Both endpoints exist, so the only possible failure below is the
    // GeoPoint relationship-property rejection itself.
    let rel = parsed_statement("create rel table Visited from Person to Place (at GeoPoint)");
    let rel_error = database
        .execute(&rel)
        .expect_err("GeoPoint rel property must be rejected");
    assert!(
        rel_error
            .to_string()
            .contains("GeoPoint relationship properties are not supported"),
        "{rel_error}"
    );

    // DDL and inserts persist at checkpoint, not at execute (the facade's
    // deferred-catalog design) — checkpoint so the file carries the geo
    // catalog, the node groups run through the geo codec, and feature bit
    // 4 is derived onto the superblock.
    database.checkpoint().expect("checkpoint geo rows");
    drop(database);
    let mut reopened = Database::open(&path).expect("reopen with geo columns");
    let Parsed::Query(plan) = parse("nodes(Place) as p | project p.loc as loc").expect("query")
    else {
        panic!("expected a query");
    };
    let result = reopened.run(&plan).expect("scan geo column");
    let mut values = result.rows.iter().map(|row| &row[0]).collect::<Vec<_>>();
    values.sort_by_key(|value| value.to_string());
    assert_eq!(values.len(), 3);
    assert_eq!(values[0].to_string(), "geo(-90, 0)");
    assert_eq!(values[1].to_string(), "geo(45.5, -122.625)");
    assert_eq!(values[2].to_string(), "null");
    drop(reopened);

    let pager = devondb_storage::pager::Pager::open(&path).expect("raw open");
    let flags = pager.superblock().feature_flags;
    assert_eq!(
        flags & devondb_storage::superblock::GEO_COLUMNS_FLAG,
        devondb_storage::superblock::GEO_COLUMNS_FLAG,
        "feature bit 4 must be set on a geo-column file (flags {flags:#x})"
    );
}

/// A geo literal projected through a real parsed query comes back as the
/// canonical value — the type's live surface while columns are staged.
#[test]
fn geo_literal_projects_through_a_real_query() {
    let directory = TestDirectory::new("project");
    let mut database = Database::create(directory.db_path(), 4096).expect("create database");

    database
        .execute(&parsed_statement(
            "create node table Person (id Int64 primary key)",
        ))
        .expect("create table");
    database
        .execute(&parsed_statement("insert into Person values (1)"))
        .expect("insert row");

    let Parsed::Query(plan) =
        parse("nodes(Person) as p | project geo(45.5, -122.625) as place").expect("query parses")
    else {
        panic!("expected a query");
    };
    let result = database.run(&plan).expect("run query");
    assert_eq!(result.rows.len(), 1);
    let value = &result.rows[0][0];
    let Value::GeoPoint(point) = value else {
        panic!("expected a GeoPoint, got {value:?}");
    };
    assert_eq!(point.lat_deg(), 45.5);
    assert_eq!(point.lng_deg(), -122.625);
    assert_eq!(value.to_string(), "geo(45.5, -122.625)");
}

/// Comparisons stay staged out until the geo predicates task: validation
/// rejects a GeoPoint comparison before execution.
#[test]
fn geo_point_comparison_is_rejected_by_validation() {
    let directory = TestDirectory::new("compare");
    let mut database = Database::create(directory.db_path(), 4096).expect("create database");

    database
        .execute(&parsed_statement(
            "create node table Person (id Int64 primary key)",
        ))
        .expect("create table");

    let Parsed::Query(plan) =
        parse("nodes(Person) as p | filter geo(1.0, 2.0) = geo(1.0, 2.0)").expect("query parses")
    else {
        panic!("expected a query");
    };
    let error = database
        .run(&plan)
        .expect_err("GeoPoint comparison must be rejected while staged");
    assert!(
        error.to_string().contains("GeoPoint"),
        "error must name the type: {error}"
    );
}

/// The WithinScan seam (`docs/GEO.md` §7, `docs/PLAN_IR.md` § within
/// semantics): text parses to the operator and round-trips through the
/// canonical printer, validation enforces the GeoPoint-column and
/// positive-finite-radius laws at run, and execution returns exact rows.
/// Severing any of those wires fails this test.
#[test]
fn within_scan_seam_parses_validates_and_executes() {
    let directory = TestDirectory::new("within-seam");
    let mut database = Database::create(directory.db_path(), 4096).expect("create");
    database
        .execute(&parsed_statement(
            "create node table Place (id Int64 primary key, name String, location GeoPoint)",
        ))
        .expect("create table");
    database
        .execute(&parsed_statement(
            "insert into Place values \
             (1, \"Burnside\", geo(45.5231, -122.6765)), \
             (2, \"Seattle\", geo(47.6062, -122.3321)), \
             (3, \"Unknown\", null)",
        ))
        .expect("insert places");

    let text = "within(Place.location, geo(45.5152, -122.6784), 3218.688)";
    let Parsed::Query(plan) = parse(text).expect("within parses") else {
        panic!("expected a query: {text}");
    };
    let printed = devondb_plan::text::printer::print_plan(&plan).expect("prints");
    assert_eq!(printed, text, "canonical text round-trip");
    let Parsed::Query(reparsed) = parse(&printed).expect("printed text parses") else {
        panic!("expected a query: {printed}");
    };
    assert_eq!(reparsed, plan, "parse(print(plan)) is identity");

    let result = database.run(&plan).expect("WithinScan executes");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::Int64(1));
    assert_eq!(result.rows[0][1], Value::String("Burnside".to_owned()));

    let Parsed::Query(wrong_column) =
        parse("within(Place.name, geo(0.0, 0.0), 10.0)").expect("parses")
    else {
        panic!("expected a query");
    };
    let error = database
        .run(&wrong_column)
        .expect_err("non-GeoPoint column rejected");
    let message = error.to_string();
    assert!(
        message.contains("expected GeoPoint"),
        "unexpected column-type message: {message}"
    );

    let Parsed::Query(bad_radius) =
        parse("within(Place.location, geo(0.0, 0.0), -5.0)").expect("parses")
    else {
        panic!("expected a query");
    };
    let error = database
        .run(&bad_radius)
        .expect_err("nonpositive radius rejected");
    let message = error.to_string();
    assert!(
        message.contains("radius must be a finite number of meters > 0"),
        "unexpected radius message: {message}"
    );
}

/// `docs/GEO.md` §7 equivalence fence: accelerated covering consumption is
/// identical, row for row and in scan order, to a naive deterministic
/// great-circle filter across interior, edge, vertex, pole, antimeridian,
/// and every resolution-scale boundary case.
#[test]
fn within_scan_acceleration_matches_naive_full_scan_row_for_row() {
    let directory = TestDirectory::new("within-equivalence");
    let mut database = Database::create(directory.db_path(), 4096).expect("create");
    database
        .execute(&parsed_statement(
            "create node table Place \
             (id Int64 primary key, name String, location GeoPoint)",
        ))
        .expect("create table");

    let cases = equivalence_cases();
    let rows = equivalence_fixture(&cases);
    database
        .execute(&Statement::InsertNode {
            table: "Place".to_owned(),
            rows,
        })
        .expect("insert equivalence fixture");

    let Parsed::Query(full_scan) = parse("nodes(Place) as Place").expect("full scan parses") else {
        panic!("expected a query");
    };
    for case in cases {
        let scanned = database.run(&full_scan).expect("naive full scan");
        let expected = scanned
            .rows
            .into_iter()
            .filter(|row| naive_match(row, case.center, case.meters))
            .collect::<Vec<_>>();
        let text = format!(
            "within(Place.location, geo({}, {}), {})",
            case.center.lat_deg(),
            case.center.lng_deg(),
            case.meters
        );
        let Parsed::Query(plan) = parse(&text).expect("WithinScan parses") else {
            panic!("expected a query: {text}");
        };
        let accelerated = database.run(&plan).expect("accelerated WithinScan");

        assert_eq!(
            accelerated.columns, scanned.columns,
            "{} columns",
            case.name
        );
        assert_eq!(accelerated.rows, expected, "{} rows", case.name);
    }
}

#[derive(Clone)]
struct EquivalenceCase {
    name: String,
    center: GeoPoint,
    meters: f64,
}

fn equivalence_cases() -> Vec<EquivalenceCase> {
    let mut cases = vec![
        EquivalenceCase {
            name: "ordinary-interior-and-edge".to_owned(),
            center: geo(45.5152, -122.6784),
            meters: 3_218.688,
        },
        EquivalenceCase {
            name: "icosahedral-vertex-center".to_owned(),
            center: geo(64.700_000_127_934_89, 10.536_199_075_467_67),
            meters: 10_000.0,
        },
        EquivalenceCase {
            name: "north-pole".to_owned(),
            center: geo(90.0, 0.0),
            meters: 30_000.0,
        },
        EquivalenceCase {
            name: "antimeridian-crossing".to_owned(),
            center: geo(0.0, 179.99),
            meters: 20_000.0,
        },
    ];
    cases.extend(
        MAX_CENTER_TO_BOUNDARY_M
            .into_iter()
            .enumerate()
            .map(|(resolution, meters)| EquivalenceCase {
                name: format!("resolution-{resolution}-radius-boundary"),
                center: geo(12.345, 67.89),
                meters,
            }),
    );
    cases
}

fn equivalence_fixture(cases: &[EquivalenceCase]) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    let mut id = 1_i64;
    for case in cases {
        let cell_width = covering_cell_width(case.meters);
        let points = [
            ("center", case.center),
            ("interior", meridian_point(case.center, case.meters * 0.5)),
            (
                "edge-inside",
                meridian_point(case.center, case.meters - cell_width * 0.5),
            ),
            (
                "edge-outside",
                meridian_point(case.center, case.meters + cell_width * 0.5),
            ),
        ];
        for (kind, point) in points {
            rows.push(vec![
                Value::Int64(id),
                Value::String(format!("{}-{kind}", case.name)),
                Value::GeoPoint(point),
            ]);
            id += 1;
        }
        if case.name == "antimeridian-crossing" {
            for (kind, distance) in [
                ("across-inside", case.meters - cell_width * 0.5),
                ("across-outside", case.meters + cell_width * 0.5),
            ] {
                rows.push(vec![
                    Value::Int64(id),
                    Value::String(format!("{}-{kind}", case.name)),
                    Value::GeoPoint(equator_east_point(case.center, distance)),
                ]);
                id += 1;
            }
        }
    }
    rows.push(vec![
        Value::Int64(id),
        Value::String("null-location".to_owned()),
        Value::Null,
    ]);
    rows
}

fn naive_match(row: &[Value], center: GeoPoint, meters: f64) -> bool {
    let Some(Value::GeoPoint(point)) = row.get(2) else {
        return false;
    };
    great_circle_meters(
        point.lat_deg(),
        point.lng_deg(),
        center.lat_deg(),
        center.lng_deg(),
    ) <= meters
}

fn covering_cell_width(meters: f64) -> f64 {
    MAX_CENTER_TO_BOUNDARY_M
        .into_iter()
        .find(|bound| *bound <= meters)
        .unwrap_or(MAX_CENTER_TO_BOUNDARY_M[15])
}

fn meridian_point(center: GeoPoint, distance_m: f64) -> GeoPoint {
    let latitude_delta = distance_m / 6_371_007.180_918_475 * 180.0 / core::f64::consts::PI;
    geo(center.lat_deg() - latitude_delta, center.lng_deg())
}

fn equator_east_point(center: GeoPoint, distance_m: f64) -> GeoPoint {
    assert_eq!(center.lat_deg(), 0.0);
    let longitude_delta = distance_m / 6_371_007.180_918_475 * 180.0 / core::f64::consts::PI;
    let mut longitude = center.lng_deg() + longitude_delta;
    if longitude >= 180.0 {
        longitude -= 360.0;
    }
    geo(0.0, longitude)
}

fn geo(lat_deg: f64, lng_deg: f64) -> GeoPoint {
    GeoPoint::from_canonical(lat_deg, lng_deg).expect("fixture point is canonical")
}
