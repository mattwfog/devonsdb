use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, Plan};
use devondb_geo::covering::{MAX_CENTER_TO_BOUNDARY_M, great_circle_meters};
use devondb_storage::{
    catalog::{Catalog, TableStorage},
    node_group::{NODE_GROUP_CAPACITY, NodeGroup, ZoneMapValue},
    pager::Pager,
    superblock::ZONE_MAPS_FLAG,
};
use devondb_types::{
    GeoPoint,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
    value::Value,
};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"zone-map-e2e-db!";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-zone-map-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("zone-maps.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn pruning_is_result_equivalent_and_two_sidedly_avoids_payload_reads() {
    let directory = TestDirectory::new();
    let path = directory.database();
    let group_ids = write_fixture(&path);
    assert_presence_rules(&path, &group_ids);

    let mut database = Database::open(&path).unwrap();

    database.reset_page_read_count();
    let selective = database.run(&selective_plan()).unwrap();
    let selective_reads = database.page_read_count();

    database.reset_page_read_count();
    let unpruned = database.run(&unprunable_equivalent_plan()).unwrap();
    let unpruned_reads = database.page_read_count();
    assert_rows_bit_identical(&selective.rows, &unpruned.rows);

    database.reset_page_read_count();
    let full = database.run(&full_scan_plan()).unwrap();
    let full_reads = database.page_read_count();

    database.reset_page_read_count();
    let all_covering = database.run(&all_covering_plan()).unwrap();
    let all_covering_reads = database.page_read_count();
    assert_rows_bit_identical(&full.rows, &all_covering.rows);

    assert!(
        selective_reads < full_reads,
        "selective scan read {selective_reads} pages, full scan read {full_reads}"
    );
    assert!(
        selective_reads < unpruned_reads,
        "selective scan read {selective_reads} pages, equivalent unpruned scan read {unpruned_reads}"
    );
    assert!(
        all_covering_reads > selective_reads,
        "all-covering scan read {all_covering_reads} pages, selective scan read {selective_reads}"
    );
}

fn write_fixture(path: &Path) -> Vec<u64> {
    let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
    let schema = fixture_schema();
    let types = fixture_types();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();

    let fully_matching = write_group(&pager, &types, NODE_GROUP_CAPACITY, |row| {
        fixture_row(row as i64, 20, float_value(row), Value::Null)
    });
    let partly_matching = write_group(&pager, &types, NODE_GROUP_CAPACITY, |row| {
        let score = if row % 2 == 0 { 5 } else { 20 };
        fixture_row((NODE_GROUP_CAPACITY + row) as i64, score, 2.0, Value::Null)
    });
    let cannot_match = write_group(&pager, &types, 4, |row| {
        fixture_row((2 * NODE_GROUP_CAPACITY + row) as i64, 0, 3.0, Value::Null)
    });
    let group_ids = vec![fully_matching, partly_matching, cannot_match];
    catalog
        .set_table_storage(
            "Reading",
            TableStorage {
                groups: group_ids.clone(),
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();
    assert_ne!(pager.superblock().feature_flags & ZONE_MAPS_FLAG, 0);
    group_ids
}

fn write_group(
    pager: &Pager,
    types: &[LogicalType],
    rows: usize,
    mut row: impl FnMut(usize) -> Vec<Value>,
) -> u64 {
    let mut group = NodeGroup::new(types.to_vec()).unwrap();
    for index in 0..rows {
        group.push_row(row(index)).unwrap();
    }
    group.write(pager).unwrap()
}

#[test]
fn within_pruning_matches_unpruned_geo_fence_and_is_two_sided() {
    let directory = TestDirectory::new();
    let path = directory.database();
    let cases = geo_equivalence_cases();
    write_geo_fixture(&path, &cases);

    let mut database = Database::open(&path).unwrap();
    database.reset_page_read_count();
    let unpruned = database.run(&geo_full_scan_plan("Place")).unwrap();
    let unpruned_reads = database.page_read_count();

    let mut selective_reads = None;
    for case in &cases {
        let expected = unpruned
            .rows
            .iter()
            .filter(|row| naive_geo_match(row, case.center, case.meters))
            .cloned()
            .collect::<Vec<_>>();
        database.reset_page_read_count();
        let pruned = database
            .run(&within_plan("Place", case.center, case.meters))
            .unwrap();
        let reads = database.page_read_count();
        assert_rows_bit_identical(&pruned.rows, &expected);
        if case.name == "ordinary" {
            selective_reads = Some(reads);
        }
    }

    let selective_reads = selective_reads.unwrap();
    assert!(
        selective_reads < unpruned_reads,
        "selective WithinScan read {selective_reads} pages, unpruned equivalent read {unpruned_reads}"
    );

    database.reset_page_read_count();
    let all_covering = database
        .run(&within_plan("Place", geo(0.0, 0.0), 25_000_000.0))
        .unwrap();
    let all_covering_reads = database.page_read_count();
    let all_non_null = unpruned
        .rows
        .iter()
        .filter(|row| matches!(row[1], Value::GeoPoint(_)))
        .cloned()
        .collect::<Vec<_>>();
    assert_rows_bit_identical(&all_covering.rows, &all_non_null);
    assert!(
        all_covering_reads > selective_reads,
        "all-covering WithinScan read {all_covering_reads} pages, selective WithinScan read {selective_reads}"
    );
}

#[test]
fn within_scans_all_null_geo_stats_instead_of_pruning() {
    let directory = TestDirectory::new();
    let path = directory.database();
    let group_id = write_all_null_geo_fixture(&path);

    let pager = Pager::open(&path).unwrap();
    let directory =
        NodeGroup::read_directory(&pager, group_id, &geo_fixture_types("EmptyPlace")).unwrap();
    let stats = directory.zone_maps().unwrap();
    assert_eq!(stats[1].null_count, 4);
    assert_eq!(stats[1].min, None);
    assert_eq!(stats[1].max, None);
    drop(pager);

    let mut database = Database::open(&path).unwrap();
    database.reset_page_read_count();
    let result = database
        .run(&within_plan("EmptyPlace", geo(0.0, 0.0), 1_000.0))
        .unwrap();
    let reads = database.page_read_count();
    assert!(result.rows.is_empty());
    assert!(
        reads > 1,
        "all-NULL GeoPoint group read only {reads} page; its payload was incorrectly pruned"
    );
}

#[derive(Clone)]
struct GeoCase {
    name: String,
    center: GeoPoint,
    meters: f64,
}

fn geo_equivalence_cases() -> Vec<GeoCase> {
    let mut cases = vec![
        GeoCase {
            name: "ordinary".to_owned(),
            center: geo(45.5152, -122.6784),
            meters: 3_218.688,
        },
        GeoCase {
            name: "icosahedral-vertex".to_owned(),
            center: geo(64.700_000_127_934_89, 10.536_199_075_467_67),
            meters: 10_000.0,
        },
        GeoCase {
            name: "north-pole".to_owned(),
            center: geo(90.0, 0.0),
            meters: 30_000.0,
        },
        GeoCase {
            name: "antimeridian-crossing".to_owned(),
            center: geo(0.0, 179.99),
            meters: 20_000.0,
        },
    ];
    cases.extend(
        MAX_CENTER_TO_BOUNDARY_M
            .into_iter()
            .enumerate()
            .map(|(resolution, meters)| GeoCase {
                name: format!("resolution-{resolution}-boundary"),
                center: geo(12.345, 67.89),
                meters,
            }),
    );
    cases
}

fn write_geo_fixture(path: &Path, cases: &[GeoCase]) {
    let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
    let schema = geo_fixture_schema("Place");
    let types = geo_fixture_types("Place");
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();

    let mut group_ids = Vec::with_capacity(cases.len() + 1);
    for (case_index, case) in cases.iter().enumerate() {
        let points = geo_case_points(case);
        group_ids.push(write_group(&pager, &types, NODE_GROUP_CAPACITY, |row| {
            let id = case_index * NODE_GROUP_CAPACITY + row;
            geo_fixture_row(id as i64, points[row % points.len()])
        }));
    }
    let first_far_id = cases.len() * NODE_GROUP_CAPACITY;
    group_ids.push(write_group(&pager, &types, 4, |row| {
        geo_fixture_row((first_far_id + row) as i64, Some(geo(-90.0, 0.0)))
    }));
    catalog
        .set_table_storage("Place", TableStorage { groups: group_ids })
        .unwrap();
    catalog.save(&pager, 1).unwrap();
    assert_ne!(pager.superblock().feature_flags & ZONE_MAPS_FLAG, 0);
}

fn write_all_null_geo_fixture(path: &Path) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
    let schema = geo_fixture_schema("EmptyPlace");
    let types = geo_fixture_types("EmptyPlace");
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    let group_id = write_group(&pager, &types, 4, |row| geo_fixture_row(row as i64, None));
    catalog
        .set_table_storage(
            "EmptyPlace",
            TableStorage {
                groups: vec![group_id],
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();
    group_id
}

fn geo_fixture_schema(name: &str) -> NodeTableSchema {
    NodeTableSchema::new(
        name.to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("location", LogicalType::GeoPoint, false),
        ],
    )
    .unwrap()
}

fn geo_fixture_types(name: &str) -> Vec<LogicalType> {
    geo_fixture_schema(name)
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect()
}

fn geo_fixture_row(id: i64, point: Option<GeoPoint>) -> Vec<Value> {
    vec![Value::Int64(id), point.map_or(Value::Null, Value::GeoPoint)]
}

fn geo_case_points(case: &GeoCase) -> Vec<Option<GeoPoint>> {
    let cell_width = MAX_CENTER_TO_BOUNDARY_M
        .into_iter()
        .find(|bound| *bound <= case.meters)
        .unwrap_or(MAX_CENTER_TO_BOUNDARY_M[15]);
    let mut points = vec![
        Some(case.center),
        Some(meridian_point(case.center, case.meters * 0.5)),
        Some(meridian_point(case.center, case.meters - cell_width * 0.5)),
        Some(meridian_point(case.center, case.meters + cell_width * 0.5)),
        None,
    ];
    if case.name == "antimeridian-crossing" {
        points.push(Some(equator_east_point(
            case.center,
            case.meters - cell_width * 0.5,
        )));
        points.push(Some(equator_east_point(
            case.center,
            case.meters + cell_width * 0.5,
        )));
    }
    points
}

fn naive_geo_match(row: &[Value], center: GeoPoint, meters: f64) -> bool {
    let Some(Value::GeoPoint(point)) = row.get(1) else {
        return false;
    };
    great_circle_meters(
        point.lat_deg(),
        point.lng_deg(),
        center.lat_deg(),
        center.lng_deg(),
    ) <= meters
}

fn within_plan(table: &str, center: GeoPoint, meters: f64) -> Plan {
    let text = format!(
        "within({table}.location, geo({}, {}), {meters})",
        center.lat_deg(),
        center.lng_deg()
    );
    let devondb::text::parser::Parsed::Query(plan) = devondb::text::parser::parse(&text).unwrap()
    else {
        panic!("expected query plan: {text}");
    };
    plan
}

fn geo_full_scan_plan(table: &str) -> Plan {
    let text = format!("nodes({table}) as {table}");
    let devondb::text::parser::Parsed::Query(plan) = devondb::text::parser::parse(&text).unwrap()
    else {
        panic!("expected query plan: {text}");
    };
    plan
}

fn meridian_point(center: GeoPoint, distance_m: f64) -> GeoPoint {
    let latitude_delta = distance_m / 6_371_007.180_918_475 * 180.0 / core::f64::consts::PI;
    geo(center.lat_deg() - latitude_delta, center.lng_deg())
}

fn equator_east_point(center: GeoPoint, distance_m: f64) -> GeoPoint {
    let longitude_delta = distance_m / 6_371_007.180_918_475 * 180.0 / core::f64::consts::PI;
    let mut longitude = center.lng_deg() + longitude_delta;
    if longitude >= 180.0 {
        longitude -= 360.0;
    }
    geo(0.0, longitude)
}

fn geo(lat_deg: f64, lng_deg: f64) -> GeoPoint {
    GeoPoint::new(lat_deg, lng_deg).unwrap()
}

fn fixture_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Reading".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("score", LogicalType::Int64, false),
            column("sample", LogicalType::Float64, false),
            column("empty", LogicalType::Int64, false),
        ],
    )
    .unwrap()
}

fn fixture_types() -> Vec<LogicalType> {
    fixture_schema()
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect()
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn fixture_row(id: i64, score: i64, sample: f64, empty: Value) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::Int64(score),
        Value::Float64(sample),
        empty,
    ]
}

fn float_value(row: usize) -> f64 {
    if row == 0 { f64::NAN } else { 1.0 }
}

fn assert_presence_rules(path: &Path, group_ids: &[u64]) {
    let pager = Pager::open(path).unwrap();
    let directory = NodeGroup::read_directory(&pager, group_ids[0], &fixture_types()).unwrap();
    let stats = directory.zone_maps().unwrap();
    assert!(matches!(stats[0].min, Some(ZoneMapValue::Int64(0))));
    assert!(matches!(stats[0].max, Some(ZoneMapValue::Int64(2047))));
    assert!(matches!(stats[1].min, Some(ZoneMapValue::Int64(20))));
    assert!(matches!(stats[1].max, Some(ZoneMapValue::Int64(20))));
    assert_eq!(stats[2].min, None, "Float64+NaN must omit min");
    assert_eq!(stats[2].max, None, "Float64+NaN must omit max");
    assert_eq!(stats[3].null_count, NODE_GROUP_CAPACITY as u32);
    assert_eq!(stats[3].min, None, "all-NULL column must omit min");
    assert_eq!(stats[3].max, None, "all-NULL column must omit max");
}

fn selective_plan() -> Plan {
    plan_with_predicate(r#"{"ge":[{"col":"r.score"},{"lit":10}]}"#)
}

fn unprunable_equivalent_plan() -> Plan {
    plan_with_predicate(r#"{"ge":[{"add":[{"col":"r.score"},{"lit":0}]},{"lit":10}]}"#)
}

fn all_covering_plan() -> Plan {
    plan_with_predicate(r#"{"ge":[{"col":"r.score"},{"lit":0}]}"#)
}

fn plan_with_predicate(predicate: &str) -> Plan {
    Plan::from_json(&format!(
        r#"{{"v":0,"plan":{{"op":"Filter","predicate":{predicate},"input":{{"op":"ScanNodes","table":"Reading","binding":"r"}}}}}}"#
    ))
    .unwrap()
}

fn full_scan_plan() -> Plan {
    Plan::from_json(r#"{"v":0,"plan":{"op":"ScanNodes","table":"Reading","binding":"r"}}"#).unwrap()
}

fn assert_rows_bit_identical(left: &[Vec<Value>], right: &[Vec<Value>]) {
    assert_eq!(left.len(), right.len());
    for (row_index, (left, right)) in left.iter().zip(right).enumerate() {
        assert_eq!(left.len(), right.len(), "row {row_index} width differs");
        for (column_index, (left, right)) in left.iter().zip(right).enumerate() {
            let equal = match (left, right) {
                (Value::Float64(left), Value::Float64(right)) => left.to_bits() == right.to_bits(),
                _ => left == right,
            };
            assert!(equal, "row {row_index} column {column_index} differs");
        }
    }
}
