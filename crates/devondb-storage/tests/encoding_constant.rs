//! Constant encoding (id 1) tests: frozen byte golden, round trips
//! over deterministic value sets including all-NULL and single-row, the
//! corruption matrix with exact messages, a seeded never-panics fuzz, and
//! plain-equivalence of a constant-read group.

use std::path::{Path, PathBuf};

use crc32c::crc32c;
use devondb_storage::catalog::{Catalog, TableStorage};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema};
use devondb_types::{Decimal128, DevonError, value::Value};
use proptest::collection;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"encoding-const!!";
const GOLDEN_DB_ID: [u8; 16] = *b"enc-constant-gld";
const ANCHOR_FILE: &str = "encoding-constant.devondb";
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;
const PROPTEST_SEED: u64 = 0x434f_4e53_5441_4e54;

/// The frozen values-section bytes of the committed golden's constant
/// columns; both constants are NULL
/// at row 3 (0-indexed 2) → validity `0b1011`:
/// - `level` Int64 constant 42.
/// - `tag` String constant "frozen".
const GOLDEN_LEVEL_PAYLOAD: [u8; 9] = [0b1011, 42, 0, 0, 0, 0, 0, 0, 0];
const GOLDEN_TAG_PAYLOAD: [u8; 11] = [0b1011, 6, 0, 0, 0, b'f', b'r', b'o', b'z', b'e', b'n'];
/// The golden directory's `COLUMN_ENCODINGS` section: id plain, constant,
/// constant — parameters zero.
const GOLDEN_SECTION: [u8; 12] = [0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0];

fn golden_rows() -> Vec<Vec<Value>> {
    vec![
        vec![
            Value::Int64(1),
            Value::Int64(42),
            Value::String("frozen".to_owned()),
        ],
        vec![
            Value::Int64(2),
            Value::Int64(42),
            Value::String("frozen".to_owned()),
        ],
        vec![Value::Int64(3), Value::Null, Value::Null],
        vec![
            Value::Int64(4),
            Value::Int64(42),
            Value::String("frozen".to_owned()),
        ],
    ]
}

fn golden_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Constant".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("level", LogicalType::Int64, false),
            column("tag", LogicalType::String, false),
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
fn mint_encoding_constant_anchor_at(path: &Path) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, GOLDEN_DB_ID).unwrap();
    let schema = golden_schema();
    let types = golden_types();
    let mut group = NodeGroup::new(types).unwrap();
    for row in golden_rows() {
        group.push_row(row).unwrap();
    }
    let group_id = group
        .write_forcing_encodings(&pager, &[(1, 1), (2, 1)])
        .unwrap();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    catalog
        .set_table_storage(
            "Constant",
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

/// Writes a forced-constant group and publishes the feature bits.
fn write_constant_group(
    directory: &TempDir,
    name: &str,
    ty: LogicalType,
    rows: Vec<Value>,
) -> (Pager, u64) {
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![ty]).unwrap();
    for row in rows {
        group.push_row(vec![row]).unwrap();
    }
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 1)]).unwrap();
    publish_features(&pager);
    (pager, directory_page)
}

fn corrupt_context(error: &DevonError) -> &str {
    match error {
        DevonError::Corrupt { context } => context,
        other => panic!("expected Corrupt, got {other}"),
    }
}

#[test]
#[ignore = "mints the committed encoding-constant golden database"]
fn mint_encoding_constant_anchor() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(ANCHOR_FILE);
    assert!(
        !path.exists(),
        "golden anchors are permanent; {ANCHOR_FILE} already exists"
    );
    mint_encoding_constant_anchor_at(&path);
}

#[test]
fn encoding_constant_anchor_mint_is_byte_deterministic() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("constant-first.devondb");
    let second = directory.path().join("constant-second.devondb");

    mint_encoding_constant_anchor_at(&first);
    mint_encoding_constant_anchor_at(&second);

    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "minting identical encoding-constant anchors produced different bytes"
    );
}

/// The frozen byte golden: the committed file's section and payload bytes
/// must equal the literals above, and the rows must decode exactly.
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
    let group_id = catalog.table_storage("Constant").unwrap().groups[0];
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
        GOLDEN_LEVEL_PAYLOAD,
        "the Int64 constant payload drifted from the frozen bytes"
    );
    assert_eq!(
        column_payload(&pager, &directory, 2),
        GOLDEN_TAG_PAYLOAD,
        "the String constant payload drifted from the frozen bytes"
    );

    let decoded = NodeGroup::read(&pager, group_id, &types).unwrap();
    for (row, expected) in golden_rows().iter().enumerate() {
        for (column, value) in expected.iter().enumerate() {
            assert_eq!(
                decoded.value(row, column),
                Some(value),
                "row {row} column {column}"
            );
        }
    }
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/encoding-constant.devondb")
}

/// Reads one column's encoded payload bytes through its directory entry.
fn column_payload(pager: &Pager, directory: &[u8], column: usize) -> Vec<u8> {
    let entry = HEADER_LEN + column * ENTRY_LEN;
    let first_page = u64::from_le_bytes(directory[entry..entry + 8].try_into().unwrap());
    let byte_len = u32::from_le_bytes(directory[entry + 8..entry + 12].try_into().unwrap());
    let page = pager.read_page(first_page).unwrap();
    page[..byte_len as usize].to_vec()
}

#[test]
fn round_trip_over_deterministic_sets() {
    let decimal = LogicalType::Decimal {
        precision: 10,
        scale: 2,
    };
    let cases: Vec<(LogicalType, Vec<Value>)> = vec![
        (LogicalType::Int64, vec![Value::Int64(-7); 5]),
        (
            LogicalType::Int64,
            vec![
                Value::Int64(i64::MIN),
                Value::Null,
                Value::Int64(i64::MIN),
                Value::Null,
            ],
        ),
        (LogicalType::Int64, vec![Value::Null; 4]),
        (LogicalType::Int64, vec![Value::Int64(1)]),
        (LogicalType::Int64, vec![Value::Null]),
        (LogicalType::Float64, vec![Value::Float64(-0.0); 3]),
        (
            LogicalType::Float64,
            vec![Value::Null, Value::Float64(7.5), Value::Null],
        ),
        (LogicalType::Bool, vec![Value::Bool(true); 2]),
        (
            LogicalType::Bool,
            vec![Value::Bool(false), Value::Null, Value::Bool(false)],
        ),
        (LogicalType::Timestamp, vec![Value::Timestamp(-1); 3]),
        (
            decimal,
            vec![
                Value::Decimal(Decimal128::new(-4225, 2).unwrap()),
                Value::Null,
                Value::Decimal(Decimal128::new(-4225, 2).unwrap()),
            ],
        ),
        (
            LogicalType::String,
            vec![Value::String("édith 🦀".to_owned()); 3],
        ),
        (
            LogicalType::String,
            vec![Value::String(String::new()), Value::Null],
        ),
        (LogicalType::String, vec![Value::Null; 2]),
        (LogicalType::String, vec![Value::String("x".to_owned())]),
    ];
    for (index, (ty, rows)) in cases.into_iter().enumerate() {
        let directory = tempdir().unwrap();
        let (pager, directory_page) = write_constant_group(
            &directory,
            &format!("case-{index}.devondb"),
            ty,
            rows.clone(),
        );
        let decoded = NodeGroup::read(&pager, directory_page, std::slice::from_ref(&ty)).unwrap();
        for (row, value) in rows.iter().enumerate() {
            assert_eq!(
                decoded.value(row, 0),
                Some(value),
                "case {index} ({ty}) row {row} diverged"
            );
        }
        // The typed scan path (§6.5) decodes the same column identically.
        let (typed, row_count) =
            NodeGroup::read_column_typed(&pager, directory_page, &[ty], 0).unwrap();
        assert_eq!(row_count, rows.len());
        assert_eq!(typed, rows, "case {index} typed decode diverged");
    }
}

#[test]
fn constant_group_reads_back_identical_to_plain() {
    let directory = tempdir().unwrap();
    let rows = vec![
        Value::Int64(99),
        Value::Null,
        Value::Int64(99),
        Value::Int64(99),
    ];
    let (constant_pager, constant_page) = write_constant_group(
        &directory,
        "constant.devondb",
        LogicalType::Int64,
        rows.clone(),
    );

    let plain_pager =
        Pager::create(directory.path().join("plain.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut plain = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    for row in &rows {
        plain.push_row(vec![row.clone()]).unwrap();
    }
    let plain_page = plain.write(&plain_pager).unwrap();
    publish_features(&plain_pager);

    let boxed_constant =
        NodeGroup::read(&constant_pager, constant_page, &[LogicalType::Int64]).unwrap();
    let boxed_plain = NodeGroup::read(&plain_pager, plain_page, &[LogicalType::Int64]).unwrap();
    for row in 0..rows.len() {
        assert_eq!(boxed_constant.value(row, 0), boxed_plain.value(row, 0));
    }

    let (typed_constant, _) =
        NodeGroup::read_column_typed(&constant_pager, constant_page, &[LogicalType::Int64], 0)
            .unwrap();
    let (typed_plain, _) =
        NodeGroup::read_column_typed(&plain_pager, plain_page, &[LogicalType::Int64], 0).unwrap();
    assert_eq!(typed_constant, typed_plain);

    // The whole-group typed scan path agrees too.
    let directory_constant =
        NodeGroup::read_directory(&constant_pager, constant_page, &[LogicalType::Int64]).unwrap();
    let columns = NodeGroup::read_columns_typed_from_directory(
        &constant_pager,
        directory_constant,
        &[LogicalType::Int64],
    )
    .unwrap();
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0], typed_plain);
}

/// Doctors one column's payload bytes in place, fixing the directory
/// entry's byte_len and CRC so the corruption under test is the section
/// content, not the frame.
fn doctor_payload(pager: &Pager, directory_page: u64, column: usize, payload: &[u8]) -> DevonError {
    let mut directory = pager.read_page(directory_page).unwrap();
    let entry = HEADER_LEN + column * ENTRY_LEN;
    let first_page = u64::from_le_bytes(directory[entry..entry + 8].try_into().unwrap());
    directory[entry + 8..entry + 12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    directory[entry + 12..entry + 16].copy_from_slice(&crc32c(payload).to_le_bytes());
    pager.write_page(first_page, &page_with(payload)).unwrap();
    pager.write_page(directory_page, &directory).unwrap();
    NodeGroup::read(pager, directory_page, &[LogicalType::Int64])
        .expect_err("doctored payload must not decode")
}

fn page_with(payload: &[u8]) -> Vec<u8> {
    let mut page = vec![0_u8; PAGE_SIZE as usize];
    page[..payload.len()].copy_from_slice(payload);
    page
}

#[test]
fn constant_corruption_matrix_reports_exact_messages() {
    let rows = vec![Value::Int64(7), Value::Null, Value::Int64(7)];

    // byte_len smaller than the fixed constant law.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_constant_group(
        &directory,
        "short.devondb",
        LogicalType::Int64,
        rows.clone(),
    );
    let error = doctor_payload(&pager, directory_page, 0, &[0b101, 7, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        corrupt_context(&error),
        "column 0 byte_len is 8, expected exactly 9 for constant-encoded Int64"
    );

    // Nonzero value slot in an all-NULL column.
    let all_null = tempdir().unwrap();
    let (pager, directory_page) = write_constant_group(
        &all_null,
        "all-null.devondb",
        LogicalType::Int64,
        vec![Value::Null, Value::Null],
    );
    let error = doctor_payload(&pager, directory_page, 0, &[0, 9, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        corrupt_context(&error),
        "constant value slot is not the zero spelling but every row is NULL"
    );

    // Trailing garbage past the fixed width.
    let trailing = tempdir().unwrap();
    let (pager, directory_page) =
        write_constant_group(&trailing, "long.devondb", LogicalType::Int64, rows);
    let error = doctor_payload(
        &pager,
        directory_page,
        0,
        &[0b101, 7, 0, 0, 0, 0, 0, 0, 0, 0xff],
    );
    assert_eq!(
        corrupt_context(&error),
        "column 0 byte_len is 10, expected exactly 9 for constant-encoded Int64"
    );
}

#[test]
fn constant_bool_rejects_bytes_above_one() {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("bool.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Bool]).unwrap();
    group.push_row(vec![Value::Bool(true)]).unwrap();
    group.push_row(vec![Value::Bool(true)]).unwrap();
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 1)]).unwrap();
    publish_features(&pager);

    // validity 0b11, Bool value byte 2.
    let mut directory = pager.read_page(directory_page).unwrap();
    let entry = HEADER_LEN;
    let first_page = u64::from_le_bytes(directory[entry..entry + 8].try_into().unwrap());
    let payload = [0b11, 2];
    directory[entry + 8..entry + 12].copy_from_slice(&2_u32.to_le_bytes());
    directory[entry + 12..entry + 16].copy_from_slice(&crc32c(&payload).to_le_bytes());
    pager.write_page(first_page, &page_with(&payload)).unwrap();
    pager.write_page(directory_page, &directory).unwrap();

    let error = NodeGroup::read(&pager, directory_page, &[LogicalType::Bool])
        .expect_err("Bool byte 2 must be corruption");
    assert_eq!(
        corrupt_context(&error),
        "constant Bool value is 2, expected 0 or 1"
    );
}

#[test]
fn constant_string_length_and_utf8_are_validated() {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("string.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
    group
        .push_row(vec![Value::String("ab".to_owned())])
        .unwrap();
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 1)]).unwrap();
    publish_features(&pager);

    let doctor = |pager: &Pager, payload: &[u8]| {
        let mut directory = pager.read_page(directory_page).unwrap();
        let first_page =
            u64::from_le_bytes(directory[HEADER_LEN..HEADER_LEN + 8].try_into().unwrap());
        directory[HEADER_LEN + 8..HEADER_LEN + 12]
            .copy_from_slice(&(payload.len() as u32).to_le_bytes());
        directory[HEADER_LEN + 12..HEADER_LEN + 16].copy_from_slice(&crc32c(payload).to_le_bytes());
        pager.write_page(first_page, &page_with(payload)).unwrap();
        pager.write_page(directory_page, &directory).unwrap();
        NodeGroup::read(pager, directory_page, &[LogicalType::String])
            .expect_err("doctored String section must not decode")
    };

    // Length prefix disagrees with the section size: validity 0b1, len 5,
    // two bytes.
    let error = doctor(&pager, &[1, 5, 0, 0, 0, b'a', b'b']);
    assert_eq!(
        corrupt_context(&error),
        "constant String value section is 6 bytes, expected 9 for a 5-byte value"
    );

    // Invalid UTF-8 in the shared value.
    let error = doctor(&pager, &[1, 1, 0, 0, 0, 0xff]);
    assert!(
        corrupt_context(&error).contains("constant String is not valid UTF-8"),
        "{}",
        corrupt_context(&error)
    );

    // Section shorter than the length prefix.
    let error = doctor(&pager, &[1, 0, 0]);
    assert_eq!(
        corrupt_context(&error),
        "column 0 byte_len is 3, smaller than constant-encoded String prefix 5"
    );
}

#[test]
fn constant_decimal_enforces_the_declared_precision() {
    let decimal = LogicalType::Decimal {
        precision: 3,
        scale: 0,
    };
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("decimal.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![decimal]).unwrap();
    group
        .push_row(vec![Value::Decimal(Decimal128::new(7, 0).unwrap())])
        .unwrap();
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 1)]).unwrap();
    publish_features(&pager);

    // Four digits through Decimal(3, 0): validity 0b1, digits 1000.
    let mut payload = vec![1_u8];
    payload.extend(1000_i128.to_le_bytes());
    let mut directory = pager.read_page(directory_page).unwrap();
    let first_page = u64::from_le_bytes(directory[HEADER_LEN..HEADER_LEN + 8].try_into().unwrap());
    directory[HEADER_LEN + 12..HEADER_LEN + 16].copy_from_slice(&crc32c(&payload).to_le_bytes());
    pager.write_page(first_page, &page_with(&payload)).unwrap();
    pager.write_page(directory_page, &directory).unwrap();

    let error = NodeGroup::read(&pager, directory_page, &[decimal])
        .expect_err("four digits must not decode through Decimal(3, 0)");
    assert_eq!(
        corrupt_context(&error),
        "constant Decimal digits exceed declared precision 3"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    /// The fuzz_decode idiom: arbitrary payload bytes under a fixed,
    /// frame-valid directory must never panic the constant decoder.
    #[test]
    fn arbitrary_constant_payloads_never_panic(
        byte_len in 0_usize..=64,
        noise in collection::vec(any::<u8>(), 0..=64),
    ) {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("fuzz.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
        group.push_row(vec![Value::Int64(7)]).unwrap();
        group.push_row(vec![Value::Null]).unwrap();
        let directory_page = group.write_forcing_encodings(&pager, &[(0, 1)]).unwrap();
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
        pager.write_page(first_page, &page_with(payload)).unwrap();
        pager.write_page(directory_page, &directory_page_bytes).unwrap();

        let outcome = std::panic::catch_unwind(|| {
            let _ = NodeGroup::read(&pager, directory_page, &[LogicalType::Int64]);
            let _ = NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::Int64], 0);
        });
        prop_assert!(outcome.is_ok(), "constant decoder panicked");
    }

    /// The same never-panics law over the String (variable-length) layout.
    #[test]
    fn arbitrary_constant_string_payloads_never_panic(
        byte_len in 0_usize..=64,
        noise in collection::vec(any::<u8>(), 0..=64),
    ) {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("fuzz-string.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
        group.push_row(vec![Value::String("ab".to_owned())]).unwrap();
        group.push_row(vec![Value::Null]).unwrap();
        let directory_page = group.write_forcing_encodings(&pager, &[(0, 1)]).unwrap();
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
        pager.write_page(first_page, &page_with(payload)).unwrap();
        pager.write_page(directory_page, &directory_page_bytes).unwrap();

        let outcome = std::panic::catch_unwind(|| {
            let _ = NodeGroup::read(&pager, directory_page, &[LogicalType::String]);
            let _ = NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::String], 0);
        });
        prop_assert!(outcome.is_ok(), "constant String decoder panicked");
    }
}
