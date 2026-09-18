//! Bitpack-for encoding (id 3) tests: frozen byte golden, round
//! trips over deterministic value sets (every 4-row NULL pattern,
//! single-row, 1025/2048-row, boundary values), the corruption matrix
//! with exact messages, a seeded never-panics fuzz, and
//! inadmissible-type refusal.

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
const DB_ID: [u8; 16] = *b"encoding-bitpak!";
const GOLDEN_DB_ID: [u8; 16] = *b"enc-bitpack-gld!";
const ANCHOR_FILE: &str = "encoding-bitpack_for.devondb";
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;
const SECTION_HEADER_BYTES: usize = 12;
const BLOCK_BYTES_PER_PLANE: usize = 128;
const PROPTEST_SEED: u64 = 0x4250_4652_4c41_5953; // "BPFR LAYS"

/// The golden directory's `COLUMN_ENCODINGS` section: id plain, then
/// bitpack_for with `p0 = bit_width` 5 (`temp`) and 7 (`at`).
const GOLDEN_SECTION: [u8; 12] = [0, 0, 0, 0, 3, 5, 0, 0, 3, 7, 0, 0];

/// Golden rows: `temp` Int64 [10, 11, NULL, 13, 27] (reference 10,
/// deltas [0, 1, -, 3, 17], bit width 5) and `at` Timestamp
/// [1000, 1004, 1008, NULL, 1064] (reference 1000, deltas [0, 4, 8, -,
/// 64], bit width 7).
fn golden_rows() -> Vec<Vec<Value>> {
    vec![
        vec![Value::Int64(1), Value::Int64(10), Value::Timestamp(1000)],
        vec![Value::Int64(2), Value::Int64(11), Value::Timestamp(1004)],
        vec![Value::Int64(3), Value::Null, Value::Timestamp(1008)],
        vec![Value::Int64(4), Value::Int64(13), Value::Null],
        vec![Value::Int64(5), Value::Int64(27), Value::Timestamp(1064)],
    ]
}

fn golden_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Bitpack".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("temp", LogicalType::Int64, false),
            column("at", LogicalType::Timestamp, false),
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

/// Sets one plane word of a payload whose values section starts after a
/// 1-byte validity bitmap and the 12-byte section header.
fn set_word(payload: &mut [u8], plane: usize, word: usize, value: u64) {
    let offset = 1 + SECTION_HEADER_BYTES + (plane * 16 + word) * 8;
    payload[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// The frozen `temp` payload:
/// validity `0b11011`, reference 10, bit width 5, slots 0..=4 deltas
/// [0, 1, 0, 3, 17]. Slot `l`'s bit `p` lands at plane `p`, word
/// `l mod 16`, lane `l div 16`:
/// - slot 1 delta 1  = 0b00001 → plane 0 word 1 lane 0
/// - slot 3 delta 3  = 0b00011 → planes 0-1 word 3 lane 0
/// - slot 4 delta 17 = 0b10001 → planes 0 and 4 word 4 lane 0
fn golden_temp_payload() -> Vec<u8> {
    let mut payload = vec![0_u8; 1 + SECTION_HEADER_BYTES + 5 * BLOCK_BYTES_PER_PLANE];
    payload[0] = 0b11011;
    payload[1..9].copy_from_slice(&10_i64.to_le_bytes());
    payload[9] = 5;
    set_word(&mut payload, 0, 1, 1);
    set_word(&mut payload, 0, 3, 1);
    set_word(&mut payload, 1, 3, 1);
    set_word(&mut payload, 0, 4, 1);
    set_word(&mut payload, 4, 4, 1);
    payload
}

/// The frozen `at` payload: validity `0b10111`, reference 1000, bit
/// width 7, slots 0..=4 deltas [0, 4, 8, 0, 64]:
/// - slot 1 delta 4  = 0b0000100 → plane 2 word 1 lane 0
/// - slot 2 delta 8  = 0b0001000 → plane 3 word 2 lane 0
/// - slot 4 delta 64 = 0b1000000 → plane 6 word 4 lane 0
fn golden_at_payload() -> Vec<u8> {
    let mut payload = vec![0_u8; 1 + SECTION_HEADER_BYTES + 7 * BLOCK_BYTES_PER_PLANE];
    payload[0] = 0b10111;
    payload[1..9].copy_from_slice(&1000_i64.to_le_bytes());
    payload[9] = 7;
    set_word(&mut payload, 2, 1, 1);
    set_word(&mut payload, 3, 2, 1);
    set_word(&mut payload, 6, 4, 1);
    payload
}

/// Mints the golden at `path` through the forcing hook, the
/// golden-anchor pattern of `crates/devondb/tests/golden.rs`:
/// deterministic db_id via `Pager::create`, pager-level group write,
/// catalog save (which derives feature bit 13).
fn mint_encoding_bitpack_for_anchor_at(path: &Path) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, GOLDEN_DB_ID).unwrap();
    let schema = golden_schema();
    let types = golden_types();
    let mut group = NodeGroup::new(types).unwrap();
    for row in golden_rows() {
        group.push_row(row).unwrap();
    }
    let group_id = group
        .write_forcing_encodings(&pager, &[(1, 3), (2, 3)])
        .unwrap();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    catalog
        .set_table_storage(
            "Bitpack",
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

/// Writes a forced-bitpack_for group and publishes the feature bits.
fn write_bitpack_group(
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
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 3)]).unwrap();
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
#[ignore = "mints the committed encoding-bitpack_for golden database"]
fn mint_encoding_bitpack_for_anchor() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(ANCHOR_FILE);
    assert!(
        !path.exists(),
        "golden anchors are permanent; {ANCHOR_FILE} already exists"
    );
    mint_encoding_bitpack_for_anchor_at(&path);
}

#[test]
fn encoding_bitpack_for_anchor_mint_is_byte_deterministic() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("bitpack-first.devondb");
    let second = directory.path().join("bitpack-second.devondb");

    mint_encoding_bitpack_for_anchor_at(&first);
    mint_encoding_bitpack_for_anchor_at(&second);

    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "minting identical encoding-bitpack_for anchors produced different bytes"
    );
}

/// The frozen byte golden: the committed file's section and payload
/// bytes must equal the layout pinned above, and the rows must decode
/// exactly.
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
    let group_id = catalog.table_storage("Bitpack").unwrap().groups[0];
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
        golden_temp_payload(),
        "the Int64 bitpack_for payload drifted from the frozen bytes"
    );
    assert_eq!(
        column_payload(&pager, &directory, 2),
        golden_at_payload(),
        "the Timestamp bitpack_for payload drifted from the frozen bytes"
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/encoding-bitpack_for.devondb")
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
    let two_blocks: Vec<Value> = (0..2048_i64)
        .map(|row| {
            if row % 5 == 0 {
                Value::Null
            } else {
                Value::Int64(1_700_000_000_000 + row % 97)
            }
        })
        .collect();
    let past_one_block: Vec<Value> = (0..1025_i64)
        .map(|row| {
            if row % 7 == 3 {
                Value::Null
            } else {
                Value::Int64(-500 + row)
            }
        })
        .collect();
    let timestamp_blocks: Vec<Value> = two_blocks
        .iter()
        .map(|value| match value {
            Value::Int64(value) => Value::Timestamp(*value),
            other => other.clone(),
        })
        .collect();
    let cases: Vec<(LogicalType, Vec<Value>)> = vec![
        (LogicalType::Int64, vec![Value::Int64(7)]),
        (LogicalType::Int64, vec![Value::Null]),
        (LogicalType::Int64, vec![Value::Null; 5]),
        (LogicalType::Int64, vec![Value::Int64(0), Value::Int64(0)]),
        (
            LogicalType::Int64,
            vec![Value::Int64(-9), Value::Null, Value::Int64(-1)],
        ),
        (
            LogicalType::Int64,
            vec![
                Value::Int64(i64::MIN),
                Value::Int64(0),
                Value::Null,
                Value::Int64(i64::MAX),
            ],
        ),
        (
            LogicalType::Timestamp,
            vec![
                Value::Timestamp(i64::MIN),
                Value::Timestamp(i64::MAX),
                Value::Null,
            ],
        ),
        (LogicalType::Timestamp, vec![Value::Timestamp(0); 3]),
        (LogicalType::Int64, two_blocks),
        (LogicalType::Int64, past_one_block),
        (LogicalType::Timestamp, timestamp_blocks),
    ];
    for (index, (ty, rows)) in cases.into_iter().enumerate() {
        let directory = tempdir().unwrap();
        let (pager, directory_page) = write_bitpack_group(
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

/// Every NULL pattern over four rows: all 16 validity masks of
/// [10, 11, 12, 13].
#[test]
fn round_trip_covers_every_null_pattern() {
    let base = [10_i64, 11, 12, 13];
    for mask in 0_u8..16 {
        let rows: Vec<Value> = base
            .iter()
            .enumerate()
            .map(|(row, value)| {
                if mask & (1 << row) != 0 {
                    Value::Int64(*value)
                } else {
                    Value::Null
                }
            })
            .collect();
        let directory = tempdir().unwrap();
        let (pager, directory_page) = write_bitpack_group(
            &directory,
            &format!("mask-{mask}.devondb"),
            LogicalType::Int64,
            rows.clone(),
        );
        let decoded = NodeGroup::read(&pager, directory_page, &[LogicalType::Int64]).unwrap();
        for (row, value) in rows.iter().enumerate() {
            assert_eq!(
                decoded.value(row, 0),
                Some(value),
                "mask {mask:04b} row {row} diverged"
            );
        }
        let (typed, _) =
            NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::Int64], 0).unwrap();
        assert_eq!(typed, rows, "mask {mask:04b} typed decode diverged");
    }
}

#[test]
fn bitpack_for_group_reads_back_identical_to_plain() {
    let directory = tempdir().unwrap();
    let rows = vec![
        Value::Int64(41),
        Value::Null,
        Value::Int64(44),
        Value::Int64(40),
    ];
    let (bitpack_pager, bitpack_page) = write_bitpack_group(
        &directory,
        "bitpack.devondb",
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

    let boxed_bitpack =
        NodeGroup::read(&bitpack_pager, bitpack_page, &[LogicalType::Int64]).unwrap();
    let boxed_plain = NodeGroup::read(&plain_pager, plain_page, &[LogicalType::Int64]).unwrap();
    for row in 0..rows.len() {
        assert_eq!(boxed_bitpack.value(row, 0), boxed_plain.value(row, 0));
    }

    let (typed_bitpack, _) =
        NodeGroup::read_column_typed(&bitpack_pager, bitpack_page, &[LogicalType::Int64], 0)
            .unwrap();
    let (typed_plain, _) =
        NodeGroup::read_column_typed(&plain_pager, plain_page, &[LogicalType::Int64], 0).unwrap();
    assert_eq!(typed_bitpack, typed_plain);

    // The whole-group typed scan path agrees too.
    let directory_bitpack =
        NodeGroup::read_directory(&bitpack_pager, bitpack_page, &[LogicalType::Int64]).unwrap();
    let columns = NodeGroup::read_columns_typed_from_directory(
        &bitpack_pager,
        directory_bitpack,
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

/// The fixture payload for corruption cases: rows [10, NULL, 13] —
/// validity `0b101`, reference 10, bit width 2, deltas [0, -, 3].
fn fixture_payload() -> Vec<u8> {
    let mut payload = vec![0_u8; 1 + SECTION_HEADER_BYTES + 2 * BLOCK_BYTES_PER_PLANE];
    payload[0] = 0b101;
    payload[1..9].copy_from_slice(&10_i64.to_le_bytes());
    payload[9] = 2;
    set_word(&mut payload, 0, 2, 1); // slot 2 delta 3, plane 0
    set_word(&mut payload, 1, 2, 1); // slot 2 delta 3, plane 1
    payload
}

fn fixture_group(directory: &TempDir, name: &str) -> (Pager, u64) {
    write_bitpack_group(
        directory,
        name,
        LogicalType::Int64,
        vec![Value::Int64(10), Value::Null, Value::Int64(13)],
    )
}

#[test]
fn bitpack_for_corruption_matrix_reports_exact_messages() {
    // Header shorter than 12 bytes: validity + 8 section bytes.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = fixture_group(&directory, "short.devondb");
    let error = doctor_payload(&pager, directory_page, 0, &[0b101, 10, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        corrupt_context(&error),
        "bitpack_for header is 8 bytes, expected at least 12"
    );

    // Reserved header bytes nonzero.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = fixture_group(&directory, "reserved.devondb");
    let mut payload = fixture_payload();
    payload[12] = 1;
    let error = doctor_payload(&pager, directory_page, 0, &payload);
    assert_eq!(
        corrupt_context(&error),
        "bitpack_for reserved header bytes are not zero"
    );

    // Bit width above 64.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = fixture_group(&directory, "wide.devondb");
    let mut payload = fixture_payload();
    payload[9] = 65;
    let error = doctor_payload(&pager, directory_page, 0, &payload[..13]);
    assert_eq!(
        corrupt_context(&error),
        "bitpack_for bit width 65 exceeds 64"
    );

    // Section length mismatch: one trailing byte past the exact law.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = fixture_group(&directory, "long.devondb");
    let mut payload = fixture_payload();
    payload.push(0xff);
    let error = doctor_payload(&pager, directory_page, 0, &payload);
    assert_eq!(
        corrupt_context(&error),
        "bitpack_for section is 269 bytes, expected exactly 268 for 3 rows at bit width 2"
    );

    // A NULL row carrying a nonzero delta: slot 1, plane 0 word 1 lane 0.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = fixture_group(&directory, "null.devondb");
    let mut payload = fixture_payload();
    set_word(&mut payload, 0, 1, 1);
    let error = doctor_payload(&pager, directory_page, 0, &payload);
    assert_eq!(
        corrupt_context(&error),
        "bitpack_for NULL slot 1 holds nonzero delta 1"
    );

    // A padding slot past row_count carrying a nonzero delta: slot 100 =
    // word 4 lane 6.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = fixture_group(&directory, "padding.devondb");
    let mut payload = fixture_payload();
    set_word(&mut payload, 0, 4, 0b100_0000);
    let error = doctor_payload(&pager, directory_page, 0, &payload);
    assert_eq!(
        corrupt_context(&error),
        "bitpack_for padding slot 100 of block 0 is not zero"
    );

    // reference + delta overflows i64: reference doctored to i64::MAX,
    // slot 2's delta 3 survives.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = fixture_group(&directory, "overflow.devondb");
    let mut payload = fixture_payload();
    payload[1..9].copy_from_slice(&i64::MAX.to_le_bytes());
    let error = doctor_payload(&pager, directory_page, 0, &payload);
    assert_eq!(
        corrupt_context(&error),
        "bitpack_for reference 9223372036854775807 + delta 3 overflows i64 at row 2"
    );

    // reference nonzero in an all-NULL column.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_bitpack_group(
        &directory,
        "all-null.devondb",
        LogicalType::Int64,
        vec![Value::Null, Value::Null],
    );
    let mut payload = vec![0_u8; 1 + SECTION_HEADER_BYTES];
    payload[1..9].copy_from_slice(&5_i64.to_le_bytes());
    let error = doctor_payload(&pager, directory_page, 0, &payload);
    assert_eq!(
        corrupt_context(&error),
        "bitpack_for reference is 5 but every row is NULL"
    );
}

/// Rewrites the single column's `COLUMN_ENCODINGS` record, fixing the
/// section frame's CRC-32C so the corruption under test is the record
/// content, not the frame.
fn doctor_encoding_record(pager: &Pager, directory_page: u64, record_bytes: [u8; 4]) {
    let mut directory = pager.read_page(directory_page).unwrap();
    let section = HEADER_LEN + ENTRY_LEN + SECTION_HEADER_LEN + ZONE_MAP_RECORD_LEN;
    let record = section + SECTION_HEADER_LEN;
    directory[record..record + 4].copy_from_slice(&record_bytes);
    directory[section + 4..section + 8].copy_from_slice(&crc32c(&record_bytes).to_le_bytes());
    pager.write_page(directory_page, &directory).unwrap();
}

/// The `p0 = bit_width` duplicate in the directory record must agree with
/// the section header.
#[test]
fn directory_bit_width_mismatch_is_corruption() {
    let directory = tempdir().unwrap();
    let (pager, directory_page) = fixture_group(&directory, "p0.devondb");
    doctor_encoding_record(&pager, directory_page, [3, 3, 0, 0]);

    let error = NodeGroup::read(&pager, directory_page, &[LogicalType::Int64])
        .expect_err("a p0/section bit width mismatch must not decode");
    assert_eq!(
        corrupt_context(&error),
        "bitpack_for directory parameter bit width 3 disagrees with the section bit width 2"
    );
}

#[test]
fn bitpack_for_refuses_inadmissible_types() {
    // Write side: forcing id 3 onto a String column is refused.
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("string.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
    group.push_row(vec![Value::String("x".to_owned())]).unwrap();
    let error = group
        .write_forcing_encodings(&pager, &[(0, 3)])
        .expect_err("bitpack_for must refuse String");
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert_eq!(context, "encoding bitpack_for is not admissible for String");

    // Read side: a directory doctored to declare id 3 on a String column
    // is corruption at section validation.
    let pager = Pager::create(
        directory.path().join("string-read.devondb"),
        PAGE_SIZE,
        DB_ID,
    )
    .unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
    group.push_row(vec![Value::String("x".to_owned())]).unwrap();
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 1)]).unwrap();
    publish_features(&pager);
    doctor_encoding_record(&pager, directory_page, [3, 0, 0, 0]);
    let error = NodeGroup::read(&pager, directory_page, &[LogicalType::String])
        .expect_err("id 3 on a String column must not decode");
    assert_eq!(
        corrupt_context(&error),
        "COLUMN_ENCODINGS column 0 encoding bitpack_for is not admissible for String"
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
    /// frame-valid directory must never panic the bitpack_for decoder.
    /// The fixture's real bit width is 1 (values [7, 8, NULL]), so
    /// noise long enough reaches the block unpack path as well as every
    /// header check.
    #[test]
    fn arbitrary_bitpack_for_payloads_never_panic(
        byte_len in 0_usize..=160,
        noise in collection::vec(any::<u8>(), 0..=160),
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
        group.push_row(vec![Value::Int64(8)]).unwrap();
        group.push_row(vec![Value::Null]).unwrap();
        let directory_page = group.write_forcing_encodings(&pager, &[(0, 3)]).unwrap();
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
        prop_assert!(outcome.is_ok(), "bitpack_for decoder panicked");
    }

    /// A seeded round-trip property over generated value sets and NULL
    /// patterns, through the real write/read path.
    #[test]
    fn generated_columns_round_trip(
        rows in collection::vec(
            (any::<i64>(), any::<bool>()),
            1..=96,
        ),
    ) {
        let rows: Vec<Value> = rows
            .into_iter()
            .map(|(value, valid)| if valid { Value::Int64(value) } else { Value::Null })
            .collect();
        let directory = tempdir().unwrap();
        let (pager, directory_page) = write_bitpack_group(
            &directory,
            "property.devondb",
            LogicalType::Int64,
            rows.clone(),
        );
        let decoded = NodeGroup::read(&pager, directory_page, &[LogicalType::Int64]).unwrap();
        for (row, value) in rows.iter().enumerate() {
            prop_assert_eq!(decoded.value(row, 0), Some(value), "row {} diverged", row);
        }
        let (typed, _) =
            NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::Int64], 0).unwrap();
        prop_assert_eq!(typed, rows);
    }
}
