//! ALP encoding (id 6) tests: frozen byte golden, bit-exact round
//! trips over deterministic value sets including every NULL pattern,
//! single-row and 2048-row columns, and boundary floats (min/max,
//! negatives, zero, −0.0, subnormals, NaN, ±inf, huge magnitudes), the
//! corruption matrix with exact messages, a seeded never-panics fuzz over
//! the decoder (`fuzz_decode.rs` idiom), and inadmissible-type refusal.

use std::path::{Path, PathBuf};

use crc32c::crc32c;
use devondb_storage::catalog::{Catalog, TableStorage};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema};
use devondb_types::{DevonError, value::Value};
use proptest::collection;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"enc-alp-test!!!!";
const GOLDEN_DB_ID: [u8; 16] = *b"enc-alp-golden!!";
const ANCHOR_FILE: &str = "encoding-alp.devondb";
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;
const PROPTEST_SEED: u64 = 0x414c_505f_464c_4f41;

/// The frozen payload bytes of the committed golden's ALP columns; both are
/// NULL at zero-indexed row 2:
/// - `score` [1.01, 1.02, NULL, 1.03] → e=2 f=0, integers 101..103 around
///   reference 101, deltas [0, 1, 0, 2], bit_width 2, no exceptions.
/// - `reading` [0.1, NaN, 2.5, NULL] → e=1 f=0, integers {1, 25} around
///   reference 1, delta 24 at row 2, bit_width 5, one exception (the NaN
///   at row 1, kept raw).
#[rustfmt::skip]
const GOLDEN_SCORE_PAYLOAD: [u8; 37] = [
    0b1011, // validity
    2, 0, 0, 0, 0, 0, 0, 0, // e=2, f=0, reserved, exception_count=0
    101, 0, 0, 0, 0, 0, 0, 0, // reference
    2, 0, 0, 0, // bit_width, reserved
    2, 0, 0, 0, 0, 0, 0, 0, // plane 0: delta bit 0 set at row 1
    8, 0, 0, 0, 0, 0, 0, 0, // plane 1: delta bit 1 set at row 3
];
#[rustfmt::skip]
const GOLDEN_READING_PAYLOAD: [u8; 73] = [
    0b0111, // validity
    1, 0, 0, 0, 1, 0, 0, 0, // e=1, f=0, reserved, exception_count=1
    1, 0, 0, 0, 0, 0, 0, 0, // reference
    5, 0, 0, 0, // bit_width, reserved
    0, 0, 0, 0, 0, 0, 0, 0, // plane 0
    0, 0, 0, 0, 0, 0, 0, 0, // plane 1
    0, 0, 0, 0, 0, 0, 0, 0, // plane 2
    4, 0, 0, 0, 0, 0, 0, 0, // plane 3: delta bit 3 set at row 2
    4, 0, 0, 0, 0, 0, 0, 0, // plane 4: delta bit 4 set at row 2
    1, 0, 0, 0, // exception row 1
    0, 0, 0, 0, 0, 0, 0xF8, 0x7F, // f64::NAN bits
];
/// The golden directory's `COLUMN_ENCODINGS` section: id plain, alp with
/// p0=e=2 p1=f=0, alp with p0=e=1 p1=f=0 — p2 zero.
const GOLDEN_SECTION: [u8; 12] = [0, 0, 0, 0, 6, 2, 0, 0, 6, 1, 0, 0];

fn golden_rows() -> Vec<Vec<Value>> {
    vec![
        vec![Value::Int64(1), Value::Float64(1.01), Value::Float64(0.1)],
        vec![
            Value::Int64(2),
            Value::Float64(1.02),
            Value::Float64(f64::NAN),
        ],
        vec![Value::Int64(3), Value::Null, Value::Float64(2.5)],
        vec![Value::Int64(4), Value::Float64(1.03), Value::Null],
    ]
}

fn golden_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Alp".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("score", LogicalType::Float64, false),
            column("reading", LogicalType::Float64, false),
        ],
    )
    .unwrap()
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn golden_types() -> Vec<LogicalType> {
    golden_schema()
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect()
}

/// Mints the golden at `path` through the forcing hook, the
/// golden-anchor pattern of `crates/devondb/tests/golden.rs`:
/// deterministic db_id via `Pager::create`, pager-level group write,
/// catalog save (which derives feature bit 13).
fn mint_encoding_alp_anchor_at(path: &Path) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, GOLDEN_DB_ID).unwrap();
    let schema = golden_schema();
    let types = golden_types();
    let mut group = NodeGroup::new(types).unwrap();
    for row in golden_rows() {
        group.push_row(row).unwrap();
    }
    let group_id = group
        .write_forcing_encodings(&pager, &[(1, 6), (2, 6)])
        .unwrap();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    catalog
        .set_table_storage(
            "Alp",
            TableStorage {
                groups: vec![group_id],
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();
    drop(pager);
    group_id
}

fn publish_features(pager: &Pager) {
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn += 1;
    superblock.feature_flags |= COLUMN_ENCODINGS_FLAG | ZONE_MAPS_FLAG;
    pager.commit_superblock(superblock).unwrap();
}

/// Writes a forced-alp single-column Float64 group and publishes the
/// feature bits.
fn write_alp_group(directory: &TempDir, name: &str, rows: Vec<Value>) -> (Pager, u64) {
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Float64]).unwrap();
    for row in rows {
        group.push_row(vec![row]).unwrap();
    }
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 6)]).unwrap();
    publish_features(&pager);
    (pager, directory_page)
}

fn corrupt_context(error: &DevonError) -> &str {
    match error {
        DevonError::Corrupt { context } => context,
        other => panic!("expected Corrupt, got {other}"),
    }
}

/// Bit-exact comparison of a decoded boxed group against the input rows
/// (NaN-aware: `Value` equality is NaN-averse, ALP's law is bit equality).
fn assert_group_bit_exact(group: &NodeGroup, rows: &[Value]) {
    for (row, expected) in rows.iter().enumerate() {
        match expected {
            Value::Null => assert_eq!(group.value(row, 0), Some(&Value::Null), "row {row}"),
            Value::Float64(value) => match group.value(row, 0) {
                Some(Value::Float64(decoded)) => assert_eq!(
                    decoded.to_bits(),
                    value.to_bits(),
                    "row {row} diverged from the input bits"
                ),
                other => panic!("row {row}: expected Float64, got {other:?}"),
            },
            other => panic!("unexpected value {other}"),
        }
    }
}

/// Bit-exact comparison of a typed decoded column (`docs/SCALE.md` §6.5).
fn assert_column_bit_exact(column: &devondb_types::column::Column, rows: &[Value]) {
    for (row, expected) in rows.iter().enumerate() {
        match (expected, column.value_at(row)) {
            (Value::Null, Value::Null) => {}
            (Value::Float64(expected), Value::Float64(decoded)) => assert_eq!(
                decoded.to_bits(),
                expected.to_bits(),
                "typed row {row} diverged from the input bits"
            ),
            (expected, decoded) => panic!("row {row}: expected {expected}, got {decoded}"),
        }
    }
}

#[test]
#[ignore = "mints the committed encoding-alp golden database"]
fn mint_encoding_alp_anchor() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(ANCHOR_FILE);
    assert!(
        !path.exists(),
        "golden anchors are permanent; {ANCHOR_FILE} already exists"
    );
    mint_encoding_alp_anchor_at(&path);
}

#[test]
fn encoding_alp_anchor_mint_is_byte_deterministic() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("alp-first.devondb");
    let second = directory.path().join("alp-second.devondb");

    mint_encoding_alp_anchor_at(&first);
    mint_encoding_alp_anchor_at(&second);

    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "minting identical encoding-alp anchors produced different bytes"
    );
}

/// The frozen byte golden: the committed file's section and payload bytes
/// must equal the literals above, and the rows must decode bit-exactly.
#[test]
fn committed_anchor_matches_the_frozen_bytes() {
    let golden = golden_path();
    let pager = Pager::open(&golden).unwrap();
    assert_eq!(
        pager.superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        COLUMN_ENCODINGS_FLAG,
        "the golden must carry feature bit 13"
    );
    let catalog = Catalog::load(&pager).unwrap();
    let types = golden_types();
    let group_id = catalog.table_storage("Alp").unwrap().groups[0];
    let directory = pager.read_page(group_id).unwrap();

    assert_eq!(
        u32::from_le_bytes(directory[12..16].try_into().unwrap()),
        0b11,
        "zone maps + encodings directory flags"
    );
    let section_start = HEADER_LEN + 3 * ENTRY_LEN + SECTION_HEADER_LEN + 3 * ZONE_MAP_RECORD_LEN;
    assert_eq!(
        u32::from_le_bytes(
            directory[section_start..section_start + 4]
                .try_into()
                .unwrap()
        ),
        12
    );
    assert_eq!(
        &directory[section_start + SECTION_HEADER_LEN..section_start + SECTION_HEADER_LEN + 12],
        &GOLDEN_SECTION,
        "the directory section drifted from the frozen bytes"
    );

    assert_eq!(
        column_payload(&pager, &directory, 1),
        GOLDEN_SCORE_PAYLOAD,
        "the score alp payload drifted from the frozen bytes"
    );
    assert_eq!(
        column_payload(&pager, &directory, 2),
        GOLDEN_READING_PAYLOAD,
        "the reading alp payload drifted from the frozen bytes"
    );

    let decoded = NodeGroup::read(&pager, group_id, &types).unwrap();
    for (row, expected) in golden_rows().iter().enumerate() {
        assert_eq!(decoded.value(row, 0), Some(&expected[0]), "row {row} id");
        for (column, value) in expected.iter().enumerate().skip(1) {
            match value {
                Value::Null => assert_eq!(decoded.value(row, column), Some(&Value::Null)),
                Value::Float64(bits) => match decoded.value(row, column) {
                    Some(Value::Float64(decoded)) => assert_eq!(decoded.to_bits(), bits.to_bits()),
                    other => panic!("row {row} column {column}: {other:?}"),
                },
                other => panic!("unexpected golden value {other}"),
            }
        }
    }
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/encoding-alp.devondb")
}

/// Reads one column's encoded payload bytes through its directory entry.
fn column_payload(pager: &Pager, directory: &[u8], column: usize) -> Vec<u8> {
    let entry = HEADER_LEN + column * ENTRY_LEN;
    let first_page = u64::from_le_bytes(directory[entry..entry + 8].try_into().unwrap());
    let byte_len = u32::from_le_bytes(directory[entry + 8..entry + 12].try_into().unwrap());
    let page = pager.read_page(first_page).unwrap();
    page[..byte_len as usize].to_vec()
}

/// Round trips through the pager (boxed and typed read paths) for one
/// single-column Float64 value set.
fn assert_round_trip(index: usize, rows: Vec<Value>) {
    let directory = tempdir().unwrap();
    let (pager, directory_page) =
        write_alp_group(&directory, &format!("case-{index}.devondb"), rows.clone());
    let decoded = NodeGroup::read(&pager, directory_page, &[LogicalType::Float64]).unwrap();
    assert_group_bit_exact(&decoded, &rows);
    // The typed scan path (§6.5) decodes the same column identically.
    let (typed, row_count) =
        NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::Float64], 0).unwrap();
    assert_eq!(row_count, rows.len());
    assert_column_bit_exact(&typed, &rows);
}

#[test]
fn round_trip_over_every_null_pattern_and_boundary_floats() {
    let cases: Vec<Vec<Value>> = vec![
        vec![Value::Float64(1.5); 4],
        vec![Value::Null; 4],
        vec![Value::Null, Value::Float64(2.5), Value::Float64(2.5)],
        vec![Value::Float64(2.5), Value::Float64(2.5), Value::Null],
        vec![
            Value::Float64(-1.25),
            Value::Null,
            Value::Float64(-1.25),
            Value::Null,
        ],
        vec![Value::Float64(0.0), Value::Float64(-0.0)],
        vec![Value::Float64(42.5)],
        vec![Value::Null],
        vec![
            Value::Float64(f64::MIN),
            Value::Float64(f64::MAX),
            Value::Float64(-1.0),
            Value::Float64(0.0),
        ],
        vec![
            Value::Float64(f64::from_bits(1)), // smallest subnormal
            Value::Float64(1e300),
            Value::Float64(5e-324),
            Value::Float64(-f64::from_bits(1)),
        ],
        vec![
            Value::Float64(f64::NAN),
            Value::Float64(f64::INFINITY),
            Value::Float64(f64::NEG_INFINITY),
            Value::Float64(-0.0),
        ],
        vec![
            Value::Float64(0.1),
            Value::Float64(0.2),
            Value::Float64(0.3),
        ],
    ];
    for (index, rows) in cases.into_iter().enumerate() {
        assert_round_trip(index, rows);
    }
}

#[test]
fn round_trip_2048_rows_with_exceptions_and_nulls() {
    let mut rows = Vec::with_capacity(2048);
    for i in 0..2048_i64 {
        rows.push(Value::Float64(
            f64::from(i32::try_from(i).unwrap()) * 0.25 - 100.0,
        ));
    }
    rows[7] = Value::Float64(-0.0); // exception: decodes to +0.0
    rows[1000] = Value::Float64(f64::NAN); // exception: raw
    rows[1500] = Value::Null;
    rows[2047] = Value::Null;
    assert_round_trip(1000, rows);
}

#[test]
fn alp_group_reads_back_identical_to_plain() {
    let directory = tempdir().unwrap();
    let rows = vec![
        Value::Float64(9.75),
        Value::Null,
        Value::Float64(f64::NAN),
        Value::Float64(-0.0),
    ];
    let (alp_pager, alp_page) = write_alp_group(&directory, "alp.devondb", rows.clone());

    let plain_pager =
        Pager::create(directory.path().join("plain.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut plain = NodeGroup::new(vec![LogicalType::Float64]).unwrap();
    for row in &rows {
        plain.push_row(vec![row.clone()]).unwrap();
    }
    let plain_page = plain.write(&plain_pager).unwrap();
    publish_features(&plain_pager);

    let boxed_alp = NodeGroup::read(&alp_pager, alp_page, &[LogicalType::Float64]).unwrap();
    let boxed_plain = NodeGroup::read(&plain_pager, plain_page, &[LogicalType::Float64]).unwrap();
    for row in 0..rows.len() {
        match (boxed_alp.value(row, 0), boxed_plain.value(row, 0)) {
            (Some(Value::Float64(a)), Some(Value::Float64(p))) => {
                assert_eq!(a.to_bits(), p.to_bits(), "row {row}")
            }
            (a, p) => assert_eq!(a, p, "row {row}"),
        }
    }

    let (typed_alp, _) =
        NodeGroup::read_column_typed(&alp_pager, alp_page, &[LogicalType::Float64], 0).unwrap();
    let (typed_plain, _) =
        NodeGroup::read_column_typed(&plain_pager, plain_page, &[LogicalType::Float64], 0).unwrap();
    assert_column_bit_exact(&typed_alp, &rows);
    assert_column_bit_exact(&typed_plain, &rows);
}

#[test]
fn alp_is_refused_for_inadmissible_types() {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("int.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    group.push_row(vec![Value::Int64(7)]).unwrap();
    let error = group
        .write_forcing_encodings(&pager, &[(0, 6)])
        .expect_err("alp on Int64 must be refused");
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert_eq!(context, "encoding alp is not admissible for Int64");
}

/// Doctors one column's payload bytes in place, fixing the directory
/// entry's byte_len and CRC so the corruption under test is the section
/// content, not the frame.
fn doctor_payload(pager: &Pager, directory_page: u64, payload: &[u8]) -> DevonError {
    let mut directory = pager.read_page(directory_page).unwrap();
    let entry = HEADER_LEN;
    let first_page = u64::from_le_bytes(directory[entry..entry + 8].try_into().unwrap());
    directory[entry + 8..entry + 12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    directory[entry + 12..entry + 16].copy_from_slice(&crc32c(payload).to_le_bytes());
    pager.write_page(first_page, &page_with(payload)).unwrap();
    pager.write_page(directory_page, &directory).unwrap();
    NodeGroup::read(pager, directory_page, &[LogicalType::Float64])
        .expect_err("doctored payload must not decode")
}

fn page_with(payload: &[u8]) -> Vec<u8> {
    let mut page = vec![0_u8; PAGE_SIZE as usize];
    page[..payload.len()].copy_from_slice(payload);
    page
}

/// The untampered payload bytes of a written group.
fn read_payload(pager: &Pager, directory_page: u64) -> Vec<u8> {
    let directory = pager.read_page(directory_page).unwrap();
    column_payload(pager, &directory, 0)
}

/// Patches one byte of the directory's `COLUMN_ENCODINGS` record for the
/// single column (offset: id, then p0 p1 p2), refreshing the framed
/// section's CRC so the corruption under test is the parameter, not the
/// frame.
fn doctor_directory_param(
    pager: &Pager,
    directory_page: u64,
    param: usize,
    value: u8,
) -> DevonError {
    let mut directory = pager.read_page(directory_page).unwrap();
    let section = HEADER_LEN + ENTRY_LEN + SECTION_HEADER_LEN + ZONE_MAP_RECORD_LEN;
    let record = section + SECTION_HEADER_LEN + 1 + param;
    // Earlier doctoring persists on the page; restore the written [e, f, 0].
    directory[section + SECTION_HEADER_LEN + 1..section + SECTION_HEADER_LEN + 4]
        .copy_from_slice(&[2, 0, 0]);
    directory[record] = value;
    let payload = &directory[section + SECTION_HEADER_LEN..section + SECTION_HEADER_LEN + 4];
    let crc = crc32c(payload);
    directory[section + 4..section + 8].copy_from_slice(&crc.to_le_bytes());
    pager.write_page(directory_page, &directory).unwrap();
    NodeGroup::read(pager, directory_page, &[LogicalType::Float64])
        .expect_err("doctored parameters must not decode")
}

#[test]
fn alp_corruption_matrix_reports_exact_messages() {
    let rows = vec![
        Value::Float64(1.01),
        Value::Float64(1.02),
        Value::Float64(1.03),
        Value::Float64(1.04),
    ];
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_alp_group(&directory, "base.devondb", rows);
    let valid = read_payload(&pager, directory_page);
    // validity 0b1111 + e=2 f=0 + exception_count 0 + reference 101 +
    // bit_width 2 + 2 planes × 1 lane × 8 bytes: 1 + 20 + 16 = 37.
    assert_eq!(valid.len(), 37);
    assert_eq!(valid[0], 0b1111);

    // Header shorter than 8 bytes.
    let error = doctor_payload(&pager, directory_page, &valid[..8]);
    assert_eq!(
        corrupt_context(&error),
        "alp values section is 7 bytes, shorter than the 8-byte header"
    );

    // Reserved header byte set.
    let mut reserved = valid.clone();
    reserved[3] = 1;
    let error = doctor_payload(&pager, directory_page, &reserved);
    assert_eq!(corrupt_context(&error), "alp reserved bytes are not zero");

    // exception_count beyond row_count.
    let mut count = valid.clone();
    count[5] = 5;
    let error = doctor_payload(&pager, directory_page, &count);
    assert_eq!(
        corrupt_context(&error),
        "alp exception_count 5 exceeds row_count 4"
    );

    // FOR header truncated.
    let error = doctor_payload(&pager, directory_page, &valid[..20]);
    assert_eq!(
        corrupt_context(&error),
        "alp values section is 19 bytes, shorter than the 20-byte frame-of-reference header"
    );

    // bit_width beyond 64.
    let mut wide = valid.clone();
    wide[17] = 65;
    let error = doctor_payload(&pager, directory_page, &wide);
    assert_eq!(
        corrupt_context(&error),
        "alp bit_width is 65, expected at most 64"
    );

    // FOR reserved byte set.
    let mut for_reserved = valid.clone();
    for_reserved[18] = 1;
    let error = doctor_payload(&pager, directory_page, &for_reserved);
    assert_eq!(
        corrupt_context(&error),
        "alp frame-of-reference reserved bytes are not zero"
    );

    // Trailing garbage past the exact length law.
    let mut trailing = valid.clone();
    trailing.push(0xff);
    let error = doctor_payload(&pager, directory_page, &trailing);
    assert_eq!(
        corrupt_context(&error),
        "alp values section is 37 bytes, expected exactly 36 for row_count 4, bit_width 2, exception_count 0"
    );

    // Tail-block padding bits set (rows occupy bits 0..4 of each lane word).
    let mut padding = valid.clone();
    padding[28] = 0b1111_0000;
    let error = doctor_payload(&pager, directory_page, &padding);
    assert_eq!(
        corrupt_context(&error),
        "alp tail block padding bits are not zero"
    );

    // Directory parameters disagreeing with the section header.
    let error = doctor_directory_param(&pager, directory_page, 2, 1);
    assert_eq!(
        corrupt_context(&error),
        "alp directory parameter p2 is 1, expected zero"
    );
    let error = doctor_directory_param(&pager, directory_page, 0, 3);
    assert_eq!(
        corrupt_context(&error),
        "alp directory exponent 3 does not match the values-section header 2"
    );
    let error = doctor_directory_param(&pager, directory_page, 1, 1);
    assert_eq!(
        corrupt_context(&error),
        "alp directory factor 1 does not match the values-section header 0"
    );
}

#[test]
fn alp_exception_table_corruption_reports_exact_messages() {
    // Two NaN exceptions at rows 1 and 2; deltas all zero (w=0).
    let rows = vec![
        Value::Float64(0.5),
        Value::Float64(f64::NAN),
        Value::Float64(f64::NAN),
        Value::Float64(0.5),
    ];
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_alp_group(&directory, "exc.devondb", rows);
    let valid = read_payload(&pager, directory_page);
    // 1 + 20 header + 0 block bytes + 2 × 12 exception records.
    assert_eq!(valid.len(), 45);

    // Exception row out of bounds.
    let mut oob = valid.clone();
    oob[21..25].copy_from_slice(&4_u32.to_le_bytes());
    let error = doctor_payload(&pager, directory_page, &oob);
    assert_eq!(
        corrupt_context(&error),
        "alp exception row 4 is out of bounds for row_count 4"
    );

    // Exception rows not strictly increasing.
    let mut dup = valid.clone();
    dup[33..37].copy_from_slice(&1_u32.to_le_bytes());
    let error = doctor_payload(&pager, directory_page, &dup);
    assert_eq!(
        corrupt_context(&error),
        "alp exception rows are not strictly increasing"
    );

    // Exception on a NULL row: one NaN exception at row 1, row 2 NULL;
    // move the exception onto the NULL row.
    let rows = vec![
        Value::Float64(0.5),
        Value::Float64(f64::NAN),
        Value::Null,
        Value::Float64(0.5),
    ];
    let null_directory = tempdir().unwrap();
    let (pager, directory_page) = write_alp_group(&null_directory, "null.devondb", rows);
    let mut moved = read_payload(&pager, directory_page);
    assert_eq!(moved[0], 0b1011);
    moved[21..25].copy_from_slice(&2_u32.to_le_bytes());
    let error = doctor_payload(&pager, directory_page, &moved);
    assert_eq!(corrupt_context(&error), "alp exception row 2 is a NULL row");
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    /// The fuzz_decode idiom: arbitrary payload bytes under a fixed,
    /// frame-valid directory must never panic the alp decoder — and neither
    /// must arbitrary directory parameter bytes.
    #[test]
    fn arbitrary_alp_payloads_never_panic(
        byte_len in 0_usize..=256,
        noise in collection::vec(any::<u8>(), 0..=256),
        params in proptest::array::uniform3(any::<u8>()),
    ) {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("fuzz.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let mut group = NodeGroup::new(vec![LogicalType::Float64]).unwrap();
        group.push_row(vec![Value::Float64(0.5)]).unwrap();
        group.push_row(vec![Value::Null]).unwrap();
        group.push_row(vec![Value::Float64(f64::NAN)]).unwrap();
        let directory_page = group.write_forcing_encodings(&pager, &[(0, 6)]).unwrap();
        publish_features(&pager);

        let mut directory_page_bytes = pager.read_page(directory_page).unwrap();
        let first_page =
            u64::from_le_bytes(directory_page_bytes[HEADER_LEN..HEADER_LEN + 8].try_into().unwrap());
        let byte_len = byte_len.min(noise.len());
        let payload = &noise[..byte_len];
        directory_page_bytes[HEADER_LEN + 8..HEADER_LEN + 12]
            .copy_from_slice(&(byte_len as u32).to_le_bytes());
        directory_page_bytes[HEADER_LEN + 12..HEADER_LEN + 16]
            .copy_from_slice(&crc32c(payload).to_le_bytes());
        // p0 · p1 · p2 of the single column's section record; refresh the
        // framed section CRC so the params themselves reach the decoder.
        let section = HEADER_LEN + ENTRY_LEN + SECTION_HEADER_LEN + ZONE_MAP_RECORD_LEN;
        let record = section + SECTION_HEADER_LEN + 1;
        directory_page_bytes[record..record + 3].copy_from_slice(&params);
        let section_payload = directory_page_bytes[section + SECTION_HEADER_LEN..section + SECTION_HEADER_LEN + 4].to_vec();
        let section_crc = crc32c(&section_payload);
        directory_page_bytes[section + 4..section + 8].copy_from_slice(&section_crc.to_le_bytes());
        pager.write_page(first_page, &page_with(payload)).unwrap();
        pager.write_page(directory_page, &directory_page_bytes).unwrap();

        let outcome = std::panic::catch_unwind(|| {
            let _ = NodeGroup::read(&pager, directory_page, &[LogicalType::Float64]);
            let _ = NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::Float64], 0);
        });
        prop_assert!(outcome.is_ok(), "alp decoder panicked");
    }
}
