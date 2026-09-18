//! Encoding boundary tests (`docs/SCALE.md` §8): the directory-flags bit 1
//! `COLUMN_ENCODINGS` section, its
//! validation law, superblock feature bit 13 derivation, the absent-when-
//! plain byte-identity law, and every registered encoding's seam round trip.

use std::path::Path;

use crc32c::crc32c;
use devondb_storage::catalog::{Catalog, TableStorage};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema};
use devondb_types::{DevonError, DevonResult, value::Value};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"encodings-seam!!";
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;

/// Writes a group and returns the pager and its directory page. The
/// superblock is left without feature bit 13 unless `publish` asks for it.
fn write_group(
    directory: &TempDir,
    name: &str,
    types: &[LogicalType],
    rows: &[Vec<Value>],
    forced: &[(usize, u8)],
) -> (Pager, u64) {
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(types.to_vec()).unwrap();
    for row in rows {
        group.push_row(row.clone()).unwrap();
    }
    // Always through the forcing seam: empty `forced` = always-plain.
    // The adaptive default path would re-select and break the
    // golden byte comparisons this suite exists to pin.
    let directory_page = group.write_forcing_encodings(&pager, forced).unwrap();
    (pager, directory_page)
}

fn publish_column_encodings(pager: &Pager) {
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn += 1;
    // A real publication that reaches a non-plain group also carries
    // ZONE_MAPS (every written directory carries stats, and the bit is
    // sticky); the in-flight pager note only covers the writing session.
    superblock.feature_flags |= COLUMN_ENCODINGS_FLAG | ZONE_MAPS_FLAG;
    pager.commit_superblock(superblock).unwrap();
}

fn seam_types() -> Vec<LogicalType> {
    vec![
        LogicalType::Int64,
        LogicalType::String,
        LogicalType::Float64,
    ]
}

fn seam_rows() -> Vec<Vec<Value>> {
    vec![
        vec![
            Value::Int64(7),
            Value::String("shared".to_owned()),
            Value::Float64(2.5),
        ],
        vec![
            Value::Int64(7),
            Value::String("shared".to_owned()),
            Value::Float64(2.5),
        ],
        vec![Value::Null, Value::Null, Value::Null],
    ]
}

/// Every seam column forced to constant (id 1).
fn forced_constant() -> Vec<(usize, u8)> {
    vec![(0, 1), (1, 1), (2, 1)]
}

fn directory_flags(page: &[u8]) -> u32 {
    u32::from_le_bytes(page[12..16].try_into().unwrap())
}

/// The encodings section's byte range inside a directory page for a group
/// with `column_count` plain main columns and no rescore entries: header +
/// entries + the bit-0 zone-map section, in ascending bit order.
fn encodings_section_range(column_count: usize) -> (usize, usize) {
    let zone_map_start = HEADER_LEN + column_count * ENTRY_LEN;
    let start = zone_map_start + SECTION_HEADER_LEN + column_count * ZONE_MAP_RECORD_LEN;
    (start, start + SECTION_HEADER_LEN + column_count * 4)
}

fn encodings_section_payload(page: &[u8], column_count: usize) -> &[u8] {
    let (start, end) = encodings_section_range(column_count);
    let section_len = u32::from_le_bytes(page[start..start + 4].try_into().unwrap()) as usize;
    assert_eq!(section_len, column_count * 4);
    assert_eq!(end - start - SECTION_HEADER_LEN, section_len);
    &page[start + SECTION_HEADER_LEN..end]
}

fn read_error(pager: &Pager, directory_page: u64, types: &[LogicalType]) -> DevonError {
    NodeGroup::read(pager, directory_page, types).expect_err("doctored directory must not decode")
}

fn corrupt_context(error: &DevonError) -> &str {
    match error {
        DevonError::Corrupt { context } => context,
        other => panic!("expected Corrupt, got {other}"),
    }
}

#[test]
fn section_round_trip_declares_the_forced_encodings() {
    let directory = tempdir().unwrap();
    let types = seam_types();
    let (pager, directory_page) = write_group(
        &directory,
        "round-trip.devondb",
        &types,
        &seam_rows(),
        &forced_constant(),
    );
    publish_column_encodings(&pager);

    let page = pager.read_page(directory_page).unwrap();
    assert_eq!(directory_flags(&page), 0b11, "zone maps + encodings bits");
    let payload = encodings_section_payload(&page, types.len());
    assert_eq!(
        payload,
        &[1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],
        "one zero-parameter constant record per main column"
    );

    let decoded = NodeGroup::read(&pager, directory_page, &types).unwrap();
    for (row, expected) in seam_rows().iter().enumerate() {
        for (column, value) in expected.iter().enumerate() {
            assert_eq!(decoded.value(row, column), Some(value));
        }
    }
}

#[test]
fn forcing_plain_writes_no_section_and_stays_byte_identical() {
    let directory = tempdir().unwrap();
    let types = seam_types();
    let rows = seam_rows();
    let (plain_pager, plain_page) = write_group(&directory, "plain.devondb", &types, &rows, &[]);
    let (forced_pager, forced_page) =
        write_group(&directory, "forced.devondb", &types, &rows, &[(0, 0)]);

    let plain = plain_pager.read_page(plain_page).unwrap();
    let forced = forced_pager.read_page(forced_page).unwrap();
    assert_eq!(directory_flags(&forced), 0b1, "zone maps only, no section");
    assert_eq!(
        plain, forced,
        "an explicitly plain selection is byte-identical to a default write"
    );
}

#[test]
fn all_plain_group_is_byte_identical_to_the_pre_change_golden() {
    // The §8.1 law: an all-plain group is byte-identical to the pre-feature
    // representation. The pre-freeze v0 anchor has different
    // superblock/catalog bytes from a fresh mint, so only the group is
    // compared. It supplies the
    // pre-change bytes; re-minting its group page-by-page proves the
    // node-group writer did not move.
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/s1-stats.devondb");
    let directory = tempdir().unwrap();
    let golden_copy = directory.path().join("s1-golden.devondb");
    std::fs::copy(&golden, &golden_copy).unwrap();

    let golden_pager = Pager::open(&golden_copy).unwrap();
    let catalog = Catalog::load(&golden_pager).unwrap();
    let schema = catalog.node_table("Stats").unwrap();
    let types: Vec<LogicalType> = schema.columns().iter().map(|column| column.ty).collect();
    let golden_group = catalog.table_storage("Stats").unwrap().groups[0];
    let golden_pages =
        devondb_storage::node_group::group_page_inventory(&golden_pager, golden_group, &types)
            .unwrap();

    let remint = directory.path().join("s1-remint.devondb");
    let remint_group = mint_s1_stats_at(&remint);
    let remint_pager = Pager::open(&remint).unwrap();
    let remint_pages =
        devondb_storage::node_group::group_page_inventory(&remint_pager, remint_group, &types)
            .unwrap();

    assert_eq!(
        golden_pages.len(),
        remint_pages.len(),
        "page inventory diverged"
    );
    for (golden_page, remint_page) in golden_pages.iter().zip(&remint_pages) {
        assert_eq!(
            golden_pager.read_page(*golden_page).unwrap(),
            remint_pager.read_page(*remint_page).unwrap(),
            "group page {golden_page} diverged from the pre-change golden"
        );
    }
}

/// Byte-for-byte replica of the group mint in `crates/devondb/tests/
/// golden.rs` `mint_s1_stats_anchor_at` (the golden-anchor pattern),
/// returning the written group's directory page id.
fn mint_s1_stats_at(path: &Path) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, *b"s1-stats-anchor!").unwrap();
    let points = [
        devondb_types::GeoPoint::from_canonical(45.5152, -122.6784).unwrap(),
        devondb_types::GeoPoint::from_canonical(51.4779, 0.0).unwrap(),
        devondb_types::GeoPoint::from_canonical(90.0, 0.0).unwrap(),
    ];
    let schema = NodeTableSchema::new(
        "Stats".to_owned(),
        vec![
            Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "sample".to_owned(),
                ty: LogicalType::Float64,
                primary_key: false,
            },
            Column {
                name: "empty".to_owned(),
                ty: LogicalType::Int64,
                primary_key: false,
            },
            Column {
                name: "location".to_owned(),
                ty: LogicalType::GeoPoint,
                primary_key: false,
            },
        ],
    )
    .unwrap();
    let types: Vec<LogicalType> = schema.columns().iter().map(|column| column.ty).collect();
    let mut group = NodeGroup::new(types).unwrap();
    for (index, point) in points.iter().enumerate() {
        let sample = [f64::NAN, -0.0, 7.5][index];
        group
            .push_row(vec![
                Value::Int64(index as i64 + 1),
                Value::Float64(sample),
                Value::Null,
                Value::GeoPoint(*point),
            ])
            .unwrap();
    }
    let group_id = group.write_forcing_encodings(&pager, &[]).unwrap();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    catalog
        .set_table_storage(
            "Stats",
            TableStorage {
                groups: vec![group_id],
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();
    drop(pager);
    group_id
}

#[test]
fn feature_bit_13_is_derived_at_catalog_save() {
    let directory = tempdir().unwrap();
    let types = vec![LogicalType::Int64];
    let rows = vec![vec![Value::Int64(3)], vec![Value::Int64(3)]];

    // Plain-only groups never set the bit.
    let (plain_pager, _) = write_group(&directory, "plain.devondb", &types, &rows, &[]);
    let mut catalog = Catalog::default();
    catalog
        .add_node_table(
            NodeTableSchema::new(
                "Plain".to_owned(),
                vec![Column {
                    name: "id".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                }],
            )
            .unwrap(),
        )
        .unwrap();
    catalog.save(&plain_pager, 1).unwrap();
    assert_eq!(
        plain_pager.superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        0,
        "a plain-only publication must not set bit 13"
    );

    // A group carrying a non-plain payload sets the bit in the same
    // publication that reaches it.
    let (pager, group_id) = write_group(&directory, "constant.devondb", &types, &rows, &[(0, 1)]);
    let mut catalog = Catalog::default();
    catalog
        .add_node_table(
            NodeTableSchema::new(
                "Constant".to_owned(),
                vec![Column {
                    name: "id".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                }],
            )
            .unwrap(),
        )
        .unwrap();
    catalog
        .set_table_storage(
            "Constant",
            TableStorage {
                groups: vec![group_id],
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();
    assert_eq!(
        pager.superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        COLUMN_ENCODINGS_FLAG,
        "the publication reaching a non-plain group must set bit 13"
    );
    // ... after which the group reads through the seam.
    let decoded = NodeGroup::read(&pager, group_id, &types).unwrap();
    assert_eq!(decoded.value(0, 0), Some(&Value::Int64(3)));
    assert_eq!(decoded.value(1, 0), Some(&Value::Int64(3)));
}

#[test]
fn flags_bit_1_without_feature_bit_13_is_corruption() {
    let directory = tempdir().unwrap();
    let types = vec![LogicalType::Int64];
    let rows = vec![vec![Value::Int64(3)]];
    let (pager, directory_page) =
        write_group(&directory, "ungoverned.devondb", &types, &rows, &[(0, 1)]);

    let error = read_error(&pager, directory_page, &types);
    assert_eq!(
        corrupt_context(&error),
        "node-group directory COLUMN_ENCODINGS bit is set but COLUMN_ENCODINGS feature is clear"
    );

    // The feature bit alone governs: once set, the same bytes decode.
    publish_column_encodings(&pager);
    NodeGroup::read(&pager, directory_page, &types).unwrap();
}

/// Rewrites the encodings section payload of the seam group's directory
/// (recomputing the section CRC) and returns the read error.
fn doctor_section(
    pager: &Pager,
    directory_page: u64,
    column_count: usize,
    payload: &[u8],
) -> DevonError {
    let mut page = pager.read_page(directory_page).unwrap();
    let (start, _) = encodings_section_range(column_count);
    page[start..start + 4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    page[start + 4..start + 8].copy_from_slice(&crc32c(payload).to_le_bytes());
    page[start + SECTION_HEADER_LEN..start + SECTION_HEADER_LEN + payload.len()]
        .copy_from_slice(payload);
    pager.write_page(directory_page, &page).unwrap();
    NodeGroup::read(pager, directory_page, &seam_types())
        .expect_err("doctored section must not decode")
}

fn doctored_section_fixture() -> (TempDir, Pager, u64) {
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_group(
        &directory,
        "fixture.devondb",
        &seam_types(),
        &seam_rows(),
        &forced_constant(),
    );
    publish_column_encodings(&pager);
    (directory, pager, directory_page)
}

#[test]
fn unregistered_encoding_id_is_corruption() {
    let (_directory, pager, directory_page) = doctored_section_fixture();
    let error = doctor_section(
        &pager,
        directory_page,
        3,
        &[7, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],
    );
    assert_eq!(
        corrupt_context(&error),
        "COLUMN_ENCODINGS column 0 encoding id 7 is not registered"
    );
}

#[test]
fn inadmissible_encoding_for_the_column_type_is_corruption() {
    let (_directory, pager, directory_page) = doctored_section_fixture();
    // dictionary (id 4) admits String only; column 0 is Int64.
    let error = doctor_section(
        &pager,
        directory_page,
        3,
        &[4, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],
    );
    assert_eq!(
        corrupt_context(&error),
        "COLUMN_ENCODINGS column 0 encoding dictionary is not admissible for Int64"
    );
}

#[test]
fn wrong_section_length_is_corruption() {
    let (_directory, pager, directory_page) = doctored_section_fixture();
    let error = doctor_section(&pager, directory_page, 3, &[1, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(
        corrupt_context(&error),
        "COLUMN_ENCODINGS section_len is 8, expected 12"
    );
}

#[test]
fn nonzero_plain_or_constant_parameters_are_corruption() {
    let (_directory, pager, directory_page) = doctored_section_fixture();
    let error = doctor_section(
        &pager,
        directory_page,
        3,
        &[1, 9, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],
    );
    assert_eq!(
        corrupt_context(&error),
        "COLUMN_ENCODINGS column 0 encoding constant parameters are not zero"
    );

    let (_directory, pager, directory_page) = doctored_section_fixture();
    // plain on columns 0-1 (params zero), constant on 2 with a parameter.
    let error = doctor_section(
        &pager,
        directory_page,
        3,
        &[0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1],
    );
    assert_eq!(
        corrupt_context(&error),
        "COLUMN_ENCODINGS column 2 encoding constant parameters are not zero"
    );
}

#[test]
fn an_all_plain_section_is_corruption() {
    let (_directory, pager, directory_page) = doctored_section_fixture();
    let error = doctor_section(
        &pager,
        directory_page,
        3,
        &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_eq!(
        corrupt_context(&error),
        "COLUMN_ENCODINGS section is present but every column is plain"
    );
}

/// Every registered non-plain encoding must write through
/// the seam and read back identical to the plain group, boxed and typed.
#[test]
fn every_registered_encoding_writes_and_reads_back_like_plain() {
    // (id, name, column with an admissible type in `seam_types`)
    let cases = [
        (1, "constant", 0),
        (2, "rle", 0),
        (3, "bitpack_for", 0),
        (4, "dictionary", 1),
        (5, "fsst", 1),
        (6, "alp", 2),
    ];
    let types = seam_types();
    let rows = seam_rows();
    let directory = tempdir().unwrap();
    let (plain_pager, plain_page) = write_group(&directory, "plain.devondb", &types, &rows, &[]);
    let plain = NodeGroup::read(&plain_pager, plain_page, &types).unwrap();
    for (id, name, column) in cases {
        let (pager, page) = write_group(
            &directory,
            &format!("{name}.devondb"),
            &types,
            &rows,
            &[(column, id)],
        );
        publish_column_encodings(&pager);
        let encoded = NodeGroup::read(&pager, page, &types).unwrap();
        for row in 0..rows.len() {
            for col in 0..types.len() {
                assert_eq!(
                    encoded.value(row, col),
                    plain.value(row, col),
                    "{name}: row {row} column {col}"
                );
            }
        }
        let (typed, _) = NodeGroup::read_column_typed(&pager, page, &types, column).unwrap();
        let (typed_plain, _) =
            NodeGroup::read_column_typed(&plain_pager, plain_page, &types, column).unwrap();
        assert_eq!(typed, typed_plain, "{name}: typed column {column}");
    }
}

#[test]
fn unregistered_forced_id_is_a_writer_error() {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("bad-id.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    group.push_row(vec![Value::Int64(1)]).unwrap();
    let error = group
        .write_forcing_encodings(&pager, &[(0, 9)])
        .expect_err("an unregistered encoding id must be refused");
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument");
    };
    assert_eq!(context, "encoding id 9 is not registered");
}

#[test]
fn read_row_count_also_enforces_the_governing_law() {
    let directory = tempdir().unwrap();
    let types = vec![LogicalType::Int64];
    let (pager, directory_page) = write_group(
        &directory,
        "count.devondb",
        &types,
        &[vec![Value::Int64(1)]],
        &[(0, 1)],
    );
    let error = NodeGroup::read_row_count(&pager, directory_page, 1)
        .expect_err("the metadata-only path must enforce the governing law too");
    assert_eq!(
        corrupt_context(&error),
        "node-group directory COLUMN_ENCODINGS bit is set but COLUMN_ENCODINGS feature is clear"
    );
    publish_column_encodings(&pager);
    assert_eq!(
        NodeGroup::read_row_count(&pager, directory_page, 1).unwrap(),
        1
    );
}

#[test]
fn zone_maps_only_directory_still_decodes_with_bit_13_set() {
    // A legacy (bit-0-only) directory coexists with the feature: groups
    // written before any non-plain payload stay readable.
    let directory = tempdir().unwrap();
    let types = seam_types();
    let (pager, directory_page) =
        write_group(&directory, "legacy.devondb", &types, &seam_rows(), &[]);
    publish_column_encodings(&pager);
    let decoded = NodeGroup::read(&pager, directory_page, &types).unwrap();
    assert_eq!(decoded.value(0, 0), Some(&Value::Int64(7)));
}

#[test]
fn zone_map_section_still_validates_alongside_encodings() {
    // Sections validate in ascending bit order; a damaged zone-map section
    // (bit 0) is reported before the encodings section (bit 1) is read.
    let (_directory, pager, directory_page) = doctored_section_fixture();
    let mut page = pager.read_page(directory_page).unwrap();
    let zone_map_start = HEADER_LEN + seam_types().len() * ENTRY_LEN;
    page[zone_map_start + 4] ^= 0xff; // section CRC
    pager.write_page(directory_page, &page).unwrap();
    let error = read_error(&pager, directory_page, &seam_types());
    assert_eq!(
        corrupt_context(&error),
        "node-group directory section bit 0 CRC-32C does not match"
    );
}

#[test]
fn seam_group_survives_reopen() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("reopen.devondb");
    let types = seam_types();
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(types.clone()).unwrap();
    for row in seam_rows() {
        group.push_row(row).unwrap();
    }
    let directory_page = group
        .write_forcing_encodings(&pager, &forced_constant())
        .unwrap();
    publish_column_encodings(&pager);
    drop(pager);

    let reopened = Pager::open(&path).unwrap();
    assert_ne!(
        reopened.superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        0
    );
    let decoded = NodeGroup::read(&reopened, directory_page, &types).unwrap();
    assert_eq!(decoded.value(2, 0), Some(&Value::Null));
}

#[test]
fn devon_result_stays_typed() {
    // Compile-time note: the read paths return DevonResult throughout.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_group(
        &directory,
        "typed.devondb",
        &seam_types(),
        &seam_rows(),
        &forced_constant(),
    );
    publish_column_encodings(&pager);
    let result: DevonResult<NodeGroup> = NodeGroup::read(&pager, directory_page, &seam_types());
    result.unwrap();
}
