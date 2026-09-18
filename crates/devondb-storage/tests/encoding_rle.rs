//! RLE encoding (id 2) tests: frozen byte golden, round trips over
//! deterministic value sets including every NULL pattern, single-row,
//! 2048-row, and boundary values, the corruption matrix with exact
//! messages, a seeded never-panics fuzz, plain-equivalence of an
//! rle-read group, and inadmissible-type refusal.

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
const DB_ID: [u8; 16] = *b"encoding-rle!!!!";
const GOLDEN_DB_ID: [u8; 16] = *b"enc-rle-golden!!";
const ANCHOR_FILE: &str = "encoding-rle.devondb";
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;
const PROPTEST_SEED: u64 = 0x524c_4552_4c45_2142;

/// The frozen values-section bytes of the committed golden's RLE columns:
/// - `temp` Timestamp, NULL at row 3 (0-indexed) → validity `0b110111`;
///   slots 100, 100, 100, 0 (the NULL slot), 100, 200 → four runs
///   (3, 100) · (1, 0) · (1, 100) · (1, 200).
/// - `flag` Bool, all valid → validity `0b111111`; slots 1, 1, 0, 0, 0, 0
///   → two runs (2, 1) · (4, 0).
const GOLDEN_TEMP_PAYLOAD: [u8; 53] = [
    0b110111, 4, 0, 0, 0, 3, 0, 0, 0, 100, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 0, 0, 0, 100, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 200, 0, 0, 0, 0, 0, 0, 0,
];
const GOLDEN_FLAG_PAYLOAD: [u8; 15] = [0b111111, 2, 0, 0, 0, 2, 0, 0, 0, 1, 4, 0, 0, 0, 0];
/// The golden directory's `COLUMN_ENCODINGS` section: id plain, rle, rle —
/// parameters zero.
const GOLDEN_SECTION: [u8; 12] = [0, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0];

fn golden_rows() -> Vec<Vec<Value>> {
    vec![
        vec![Value::Int64(1), Value::Timestamp(100), Value::Bool(true)],
        vec![Value::Int64(2), Value::Timestamp(100), Value::Bool(true)],
        vec![Value::Int64(3), Value::Timestamp(100), Value::Bool(false)],
        vec![Value::Int64(4), Value::Null, Value::Bool(false)],
        vec![Value::Int64(5), Value::Timestamp(100), Value::Bool(false)],
        vec![Value::Int64(6), Value::Timestamp(200), Value::Bool(false)],
    ]
}

fn golden_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Rle".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("temp", LogicalType::Timestamp, false),
            column("flag", LogicalType::Bool, false),
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

/// Mints the golden at `path` through the forcing hook, the golden-anchor
/// pattern of `crates/devondb/tests/golden.rs`: deterministic db_id via
/// `Pager::create`, pager-level group write, catalog save (which derives
/// feature bit 13).
fn mint_encoding_rle_anchor_at(path: &Path) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, GOLDEN_DB_ID).unwrap();
    let schema = golden_schema();
    let types = golden_types();
    let mut group = NodeGroup::new(types).unwrap();
    for row in golden_rows() {
        group.push_row(row).unwrap();
    }
    let group_id = group
        .write_forcing_encodings(&pager, &[(1, 2), (2, 2)])
        .unwrap();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    catalog
        .set_table_storage(
            "Rle",
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

/// Writes a forced-rle group and publishes the feature bits.
fn write_rle_group(
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
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 2)]).unwrap();
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
#[ignore = "mints the committed encoding-rle golden database"]
fn mint_encoding_rle_anchor() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(ANCHOR_FILE);
    assert!(
        !path.exists(),
        "golden anchors are permanent; {ANCHOR_FILE} already exists"
    );
    mint_encoding_rle_anchor_at(&path);
}

#[test]
fn encoding_rle_anchor_mint_is_byte_deterministic() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("rle-first.devondb");
    let second = directory.path().join("rle-second.devondb");

    mint_encoding_rle_anchor_at(&first);
    mint_encoding_rle_anchor_at(&second);

    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "minting identical encoding-rle anchors produced different bytes"
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
    let group_id = catalog.table_storage("Rle").unwrap().groups[0];
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
        GOLDEN_TEMP_PAYLOAD,
        "the Timestamp rle payload drifted from the frozen bytes"
    );
    assert_eq!(
        column_payload(&pager, &directory, 2),
        GOLDEN_FLAG_PAYLOAD,
        "the Bool rle payload drifted from the frozen bytes"
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/encoding-rle.devondb")
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
    let wide = LogicalType::Decimal {
        precision: 38,
        scale: 0,
    };
    // 2048 rows in 32 runs of 64 (value = row / 64), with rows 5 and 2047
    // NULL — a full-node-group shape at the CHUNK_CAPACITY bound.
    let mut big: Vec<Value> = (0..2048_i64).map(|row| Value::Int64(row / 64)).collect();
    big[5] = Value::Null;
    big[2047] = Value::Null;
    let cases: Vec<(LogicalType, Vec<Value>)> = vec![
        (LogicalType::Int64, vec![Value::Int64(-7); 5]),
        (
            LogicalType::Int64,
            vec![
                Value::Int64(i64::MIN),
                Value::Int64(i64::MIN),
                Value::Null,
                Value::Int64(i64::MAX),
                Value::Int64(i64::MAX),
            ],
        ),
        // NULL slots are zero by law: [0, NULL, 0] is ONE merged run.
        (
            LogicalType::Int64,
            vec![Value::Int64(0), Value::Null, Value::Int64(0)],
        ),
        (LogicalType::Int64, vec![Value::Null; 4]),
        (LogicalType::Int64, vec![Value::Int64(1)]),
        (LogicalType::Int64, vec![Value::Null]),
        (LogicalType::Int64, big),
        (
            LogicalType::Timestamp,
            vec![
                Value::Timestamp(i64::MIN),
                Value::Null,
                Value::Timestamp(i64::MIN),
                Value::Timestamp(-1),
            ],
        ),
        (
            LogicalType::Bool,
            vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false),
                Value::Null,
                Value::Bool(true),
            ],
        ),
        (LogicalType::Bool, vec![Value::Bool(false)]),
        (
            decimal,
            vec![
                Value::Decimal(Decimal128::new(-4225, 2).unwrap()),
                Value::Null,
                Value::Decimal(Decimal128::new(-4225, 2).unwrap()),
                Value::Decimal(Decimal128::new(0, 2).unwrap()),
            ],
        ),
        (
            wide,
            vec![
                Value::Decimal(
                    Decimal128::new(99_999_999_999_999_999_999_999_999_999_999_999_999, 0).unwrap(),
                ),
                Value::Decimal(
                    Decimal128::new(-99_999_999_999_999_999_999_999_999_999_999_999_999, 0)
                        .unwrap(),
                ),
            ],
        ),
    ];
    for (index, (ty, rows)) in cases.into_iter().enumerate() {
        let directory = tempdir().unwrap();
        let (pager, directory_page) = write_rle_group(
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

/// Every NULL pattern of a 4-row column round-trips: all sixteen validity
/// combinations of `[5, 5, 5, 5]`.
#[test]
fn every_null_pattern_round_trips() {
    for mask in 0_u8..16 {
        let rows: Vec<Value> = (0..4)
            .map(|row| {
                if mask & (1 << row) != 0 {
                    Value::Null
                } else {
                    Value::Int64(5)
                }
            })
            .collect();
        let directory = tempdir().unwrap();
        let (pager, directory_page) =
            write_rle_group(&directory, "mask.devondb", LogicalType::Int64, rows.clone());
        let decoded = NodeGroup::read(&pager, directory_page, &[LogicalType::Int64]).unwrap();
        for (row, value) in rows.iter().enumerate() {
            assert_eq!(
                decoded.value(row, 0),
                Some(value),
                "mask {mask:#06b} row {row} diverged"
            );
        }
    }
}

/// Runs merge across NULL slots (a NULL slot's value is zero by law):
/// `[0, NULL, 0]` encodes as ONE run of three zeros.
#[test]
fn null_slots_merge_into_zero_runs() {
    let directory = tempdir().unwrap();
    let rows = vec![Value::Int64(0), Value::Null, Value::Int64(0)];
    let (pager, directory_page) = write_rle_group(
        &directory,
        "merged.devondb",
        LogicalType::Int64,
        rows.clone(),
    );
    let directory_bytes = pager.read_page(directory_page).unwrap();
    // validity 0b101, run_count 1, run (3, 0).
    let mut expected = vec![0b101, 1, 0, 0, 0, 3, 0, 0, 0];
    expected.extend_from_slice(&[0; 8]);
    assert_eq!(column_payload(&pager, &directory_bytes, 0), expected);
    let decoded = NodeGroup::read(&pager, directory_page, &[LogicalType::Int64]).unwrap();
    for (row, value) in rows.iter().enumerate() {
        assert_eq!(decoded.value(row, 0), Some(value), "row {row}");
    }
}

#[test]
fn rle_group_reads_back_identical_to_plain() {
    let directory = tempdir().unwrap();
    let rows = vec![
        Value::Int64(99),
        Value::Int64(99),
        Value::Null,
        Value::Int64(7),
    ];
    let (rle_pager, rle_page) =
        write_rle_group(&directory, "rle.devondb", LogicalType::Int64, rows.clone());

    let plain_pager =
        Pager::create(directory.path().join("plain.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut plain = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    for row in &rows {
        plain.push_row(vec![row.clone()]).unwrap();
    }
    let plain_page = plain.write(&plain_pager).unwrap();
    publish_features(&plain_pager);

    let boxed_rle = NodeGroup::read(&rle_pager, rle_page, &[LogicalType::Int64]).unwrap();
    let boxed_plain = NodeGroup::read(&plain_pager, plain_page, &[LogicalType::Int64]).unwrap();
    for row in 0..rows.len() {
        assert_eq!(boxed_rle.value(row, 0), boxed_plain.value(row, 0));
    }

    let (typed_rle, _) =
        NodeGroup::read_column_typed(&rle_pager, rle_page, &[LogicalType::Int64], 0).unwrap();
    let (typed_plain, _) =
        NodeGroup::read_column_typed(&plain_pager, plain_page, &[LogicalType::Int64], 0).unwrap();
    assert_eq!(typed_rle, typed_plain);

    // The whole-group typed scan path agrees too.
    let directory_rle =
        NodeGroup::read_directory(&rle_pager, rle_page, &[LogicalType::Int64]).unwrap();
    let columns = NodeGroup::read_columns_typed_from_directory(
        &rle_pager,
        directory_rle,
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

/// A 3-row Int64 rle group (`[7, NULL, 7]`): validity `0b101`, runs
/// (1, 7) · (1, 0) · (1, 7).
fn three_row_int64_group(directory: &TempDir, name: &str) -> (Pager, u64) {
    write_rle_group(
        directory,
        name,
        LogicalType::Int64,
        vec![Value::Int64(7), Value::Null, Value::Int64(7)],
    )
}

#[test]
fn rle_corruption_matrix_reports_exact_messages() {
    // Shorter than the 4-byte run_count prefix.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = three_row_int64_group(&directory, "short.devondb");
    let error = doctor_payload(&pager, directory_page, 0, &[0b101, 1, 0]);
    assert_eq!(
        corrupt_context(&error),
        "rle values section is 2 bytes, smaller than the 4-byte run_count prefix"
    );

    // run_count 2 but only one run record present.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = three_row_int64_group(&directory, "run-short.devondb");
    let error = doctor_payload(
        &pager,
        directory_page,
        0,
        &[0b101, 2, 0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_eq!(
        corrupt_context(&error),
        "rle run section is 12 bytes, expected 24 for 2 runs"
    );

    // A trailing byte past the declared runs.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = three_row_int64_group(&directory, "trailing.devondb");
    let error = doctor_payload(
        &pager,
        directory_page,
        0,
        &[0b101, 1, 0, 0, 0, 3, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0xff],
    );
    assert_eq!(
        corrupt_context(&error),
        "rle values section has 1 trailing bytes after 1 runs"
    );

    // A zero-length run.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = three_row_int64_group(&directory, "zero-run.devondb");
    let error = doctor_payload(
        &pager,
        directory_page,
        0,
        &[0b101, 1, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_eq!(corrupt_context(&error), "rle run 0 has length 0");

    // Runs cover 2 rows of a 3-row column.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = three_row_int64_group(&directory, "coverage.devondb");
    let error = doctor_payload(
        &pager,
        directory_page,
        0,
        &[0b101, 1, 0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_eq!(corrupt_context(&error), "rle runs cover 2 rows, expected 3");

    // A nonzero parameter byte in the directory's COLUMN_ENCODINGS record
    // (the section frame is `len u32 ‖ crc32c u32 ‖ payload`, so the CRC
    // is fixed to keep the corruption in the parameter byte).
    let directory = tempdir().unwrap();
    let (pager, directory_page) = three_row_int64_group(&directory, "params.devondb");
    let mut directory_bytes = pager.read_page(directory_page).unwrap();
    // 1-column layout: header 16 + entry 16 + zone-map section header 8 +
    // record 24 = 64; the encodings section header follows (8 B), then the
    // record `id · p0 · p1 · p2` — p0 sits at offset 73.
    directory_bytes[73] = 1;
    let crc = crc32c(&directory_bytes[72..76]);
    directory_bytes[68..72].copy_from_slice(&crc.to_le_bytes());
    pager.write_page(directory_page, &directory_bytes).unwrap();
    let error = NodeGroup::read(&pager, directory_page, &[LogicalType::Int64])
        .expect_err("nonzero rle parameters must be corruption");
    assert_eq!(
        corrupt_context(&error),
        "encoding rle parameters are not zero"
    );
}

#[test]
fn rle_bool_rejects_bytes_above_one() {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("bool.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Bool]).unwrap();
    group.push_row(vec![Value::Bool(true)]).unwrap();
    group.push_row(vec![Value::Bool(true)]).unwrap();
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 2)]).unwrap();
    publish_features(&pager);

    // validity 0b11, run_count 1, run (2, byte 2).
    let payload = [0b11, 1, 0, 0, 0, 2, 0, 0, 0, 2];
    let mut directory = pager.read_page(directory_page).unwrap();
    let entry = HEADER_LEN;
    let first_page = u64::from_le_bytes(directory[entry..entry + 8].try_into().unwrap());
    directory[entry + 8..entry + 12].copy_from_slice(&10_u32.to_le_bytes());
    directory[entry + 12..entry + 16].copy_from_slice(&crc32c(&payload).to_le_bytes());
    pager.write_page(first_page, &page_with(&payload)).unwrap();
    pager.write_page(directory_page, &directory).unwrap();

    let error = NodeGroup::read(&pager, directory_page, &[LogicalType::Bool])
        .expect_err("Bool byte 2 must be corruption");
    assert_eq!(
        corrupt_context(&error),
        "rle Bool value is 2, expected 0 or 1"
    );
}

#[test]
fn rle_decimal_enforces_the_declared_precision() {
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
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 2)]).unwrap();
    publish_features(&pager);

    // Four digits through Decimal(3, 0): validity 0b1, run (1, 1000).
    let mut payload = vec![1_u8, 1, 0, 0, 0, 1, 0, 0, 0];
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
        "rle Decimal digits exceed declared precision 3"
    );
}

#[test]
fn rle_refuses_inadmissible_types() {
    // Write-time: forcing id 2 onto a String column is a writer-policy
    // error.
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("string.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
    group
        .push_row(vec![Value::String("ab".to_owned())])
        .unwrap();
    let error = group
        .write_forcing_encodings(&pager, &[(0, 2)])
        .expect_err("rle must refuse String at write time");
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert_eq!(context, "encoding rle is not admissible for String");

    // Read-time: an rle payload read through an inadmissible type is
    // corruption. The group is all-NULL so its zone-map record carries no
    // min/max (a present min/max would trip the stats law first and mask
    // the encoding refusal under test).
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_rle_group(
        &directory,
        "as-string.devondb",
        LogicalType::Int64,
        vec![Value::Null, Value::Null],
    );
    let error = NodeGroup::read(&pager, directory_page, &[LogicalType::String])
        .expect_err("rle must refuse String at read time");
    assert_eq!(
        corrupt_context(&error),
        "COLUMN_ENCODINGS column 0 encoding rle is not admissible for String"
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
    /// frame-valid directory must never panic the rle decoder.
    #[test]
    fn arbitrary_rle_payloads_never_panic(
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
        let directory_page = group.write_forcing_encodings(&pager, &[(0, 2)]).unwrap();
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
        prop_assert!(outcome.is_ok(), "rle decoder panicked");
    }

    /// The same never-panics law over the 1-byte-value Bool layout.
    #[test]
    fn arbitrary_rle_bool_payloads_never_panic(
        byte_len in 0_usize..=64,
        noise in collection::vec(any::<u8>(), 0..=64),
    ) {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("fuzz-bool.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let mut group = NodeGroup::new(vec![LogicalType::Bool]).unwrap();
        group.push_row(vec![Value::Bool(true)]).unwrap();
        group.push_row(vec![Value::Bool(false)]).unwrap();
        let directory_page = group.write_forcing_encodings(&pager, &[(0, 2)]).unwrap();
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
            let _ = NodeGroup::read(&pager, directory_page, &[LogicalType::Bool]);
            let _ = NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::Bool], 0);
        });
        prop_assert!(outcome.is_ok(), "rle Bool decoder panicked");
    }

    /// The same never-panics law over the 16-byte-value Decimal layout.
    #[test]
    fn arbitrary_rle_decimal_payloads_never_panic(
        byte_len in 0_usize..=64,
        noise in collection::vec(any::<u8>(), 0..=64),
    ) {
        let decimal = LogicalType::Decimal {
            precision: 10,
            scale: 2,
        };
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("fuzz-decimal.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let mut group = NodeGroup::new(vec![decimal]).unwrap();
        group
            .push_row(vec![Value::Decimal(Decimal128::new(125, 2).unwrap())])
            .unwrap();
        group.push_row(vec![Value::Null]).unwrap();
        let directory_page = group.write_forcing_encodings(&pager, &[(0, 2)]).unwrap();
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
            let _ = NodeGroup::read(&pager, directory_page, &[decimal]);
            let _ = NodeGroup::read_column_typed(&pager, directory_page, &[decimal], 0);
        });
        prop_assert!(outcome.is_ok(), "rle Decimal decoder panicked");
    }
}
