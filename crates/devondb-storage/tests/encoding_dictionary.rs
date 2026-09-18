//! Dictionary encoding (id 4) tests: frozen byte golden, round trips
//! over deterministic value sets including every NULL pattern, single-row
//! and 2048-row, the corruption matrix with exact messages, a seeded
//! never-panics fuzz, plain-equivalence of a dictionary-read group, and
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
const DB_ID: [u8; 16] = *b"encoding-dict!!!";
const GOLDEN_DB_ID: [u8; 16] = *b"enc-dictionary!!";
const ANCHOR_FILE: &str = "encoding-dictionary.devondb";
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;
const PROPTEST_SEED: u64 = 0x4449_4354_2430_3030;

/// The frozen values-section bytes of the committed golden's `tag` column:
/// rows pear · apple · NULL · apple
/// · cherry · pear → validity `0b111011`; dictionary apple · cherry · pear
/// (bytewise ascending), heap "applecherrypear", codes 2 0 0 0 1 2.
const GOLDEN_TAG_PAYLOAD: [u8; 60] = [
    0b111011, // validity
    3, 0, 0, 0, // dict_count 3
    0, 0, 0, 0, 5, 0, 0, 0, 11, 0, 0, 0, 15, 0, 0, 0, // offsets 0 5 11 15
    b'a', b'p', b'p', b'l', b'e', b'c', b'h', b'e', b'r', b'r', b'y', b'p', b'e', b'a', b'r', 2, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, // codes
];
/// The golden directory's `COLUMN_ENCODINGS` section: id plain (params
/// zero), then dictionary (id 4, `p0` = code width 4).
const GOLDEN_SECTION: [u8; 8] = [0, 0, 0, 0, 4, 4, 0, 0];

fn golden_rows() -> Vec<Vec<Value>> {
    vec![
        vec![Value::Int64(1), Value::String("pear".to_owned())],
        vec![Value::Int64(2), Value::String("apple".to_owned())],
        vec![Value::Int64(3), Value::Null],
        vec![Value::Int64(4), Value::String("apple".to_owned())],
        vec![Value::Int64(5), Value::String("cherry".to_owned())],
        vec![Value::Int64(6), Value::String("pear".to_owned())],
    ]
}

fn golden_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Dictionary".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
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
fn mint_encoding_dictionary_anchor_at(path: &Path) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, GOLDEN_DB_ID).unwrap();
    let schema = golden_schema();
    let types = golden_types();
    let mut group = NodeGroup::new(types).unwrap();
    for row in golden_rows() {
        group.push_row(row).unwrap();
    }
    let group_id = group.write_forcing_encodings(&pager, &[(1, 4)]).unwrap();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    catalog
        .set_table_storage(
            "Dictionary",
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

/// Writes a single-column String group forced to dictionary and publishes
/// the feature bits.
fn write_dictionary_group(directory: &TempDir, name: &str, rows: Vec<Value>) -> (Pager, u64) {
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
    for row in rows {
        group.push_row(vec![row]).unwrap();
    }
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 4)]).unwrap();
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
#[ignore = "mints the committed encoding-dictionary golden database"]
fn mint_encoding_dictionary_anchor() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(ANCHOR_FILE);
    assert!(
        !path.exists(),
        "golden anchors are permanent; {ANCHOR_FILE} already exists"
    );
    mint_encoding_dictionary_anchor_at(&path);
}

#[test]
fn encoding_dictionary_anchor_mint_is_byte_deterministic() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("dictionary-first.devondb");
    let second = directory.path().join("dictionary-second.devondb");

    mint_encoding_dictionary_anchor_at(&first);
    mint_encoding_dictionary_anchor_at(&second);

    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "minting identical encoding-dictionary anchors produced different bytes"
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
    let group_id = catalog.table_storage("Dictionary").unwrap().groups[0];
    let directory = pager.read_page(group_id).unwrap();

    assert_eq!(
        u32::from_le_bytes(directory[12..16].try_into().unwrap()),
        0b11,
        "zone maps + encodings directory flags"
    );
    let section_start = HEADER_LEN + 2 * ENTRY_LEN + SECTION_HEADER_LEN + 2 * ZONE_MAP_RECORD_LEN;
    assert_eq!(
        u32::from_le_bytes(
            directory[section_start..section_start + 4]
                .try_into()
                .unwrap()
        ),
        8
    );
    assert_eq!(
        &directory[section_start + SECTION_HEADER_LEN..section_start + SECTION_HEADER_LEN + 8],
        &GOLDEN_SECTION,
        "the directory section drifted from the frozen bytes"
    );

    assert_eq!(
        column_payload(&pager, &directory, 1),
        GOLDEN_TAG_PAYLOAD,
        "the dictionary payload drifted from the frozen bytes"
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/encoding-dictionary.devondb")
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
    let mut cycling = Vec::new();
    for index in 0..2048 {
        cycling.push(match index % 7 {
            0 => Value::Null,
            n => Value::String(format!("tag-{:04}", n * 13 % 32)),
        });
    }
    let cases: Vec<Vec<Value>> = vec![
        vec![Value::String("b".to_owned()), Value::String("a".to_owned())],
        vec![Value::Null; 4],
        vec![Value::Null],
        vec![Value::String("only".to_owned())],
        // Boundary strings: empty, NUL, astral-plane max, long.
        vec![
            Value::String(String::new()),
            Value::Null,
            Value::String(String::new()),
        ],
        vec![
            Value::String("\u{0}".to_owned()),
            Value::String("\u{10FFFF}".to_owned()),
            Value::Null,
            Value::String("édith 🦀".to_owned()),
        ],
        vec![Value::String("x".repeat(1000)), Value::Null],
        vec![Value::String("same".to_owned()); 5],
        cycling,
    ];
    for (index, rows) in cases.into_iter().enumerate() {
        let directory = tempdir().unwrap();
        let (pager, directory_page) =
            write_dictionary_group(&directory, &format!("case-{index}.devondb"), rows.clone());
        let decoded = NodeGroup::read(&pager, directory_page, &[LogicalType::String]).unwrap();
        for (row, value) in rows.iter().enumerate() {
            assert_eq!(
                decoded.value(row, 0),
                Some(value),
                "case {index} row {row} diverged"
            );
        }
        // The typed scan path (§6.5) decodes the same column identically.
        let (typed, row_count) =
            NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::String], 0)
                .unwrap();
        assert_eq!(row_count, rows.len());
        assert_eq!(typed, rows, "case {index} typed decode diverged");
    }
}

#[test]
fn dictionary_group_reads_back_identical_to_plain() {
    let directory = tempdir().unwrap();
    let rows = vec![
        Value::String("pear".to_owned()),
        Value::Null,
        Value::String("apple".to_owned()),
        Value::String("pear".to_owned()),
    ];
    let (dictionary_pager, dictionary_page) =
        write_dictionary_group(&directory, "dictionary.devondb", rows.clone());

    let plain_pager =
        Pager::create(directory.path().join("plain.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut plain = NodeGroup::new(vec![LogicalType::String]).unwrap();
    for row in &rows {
        plain.push_row(vec![row.clone()]).unwrap();
    }
    let plain_page = plain.write(&plain_pager).unwrap();
    publish_features(&plain_pager);

    let boxed_dictionary =
        NodeGroup::read(&dictionary_pager, dictionary_page, &[LogicalType::String]).unwrap();
    let boxed_plain = NodeGroup::read(&plain_pager, plain_page, &[LogicalType::String]).unwrap();
    for row in 0..rows.len() {
        assert_eq!(boxed_dictionary.value(row, 0), boxed_plain.value(row, 0));
    }

    let (typed_dictionary, _) = NodeGroup::read_column_typed(
        &dictionary_pager,
        dictionary_page,
        &[LogicalType::String],
        0,
    )
    .unwrap();
    let (typed_plain, _) =
        NodeGroup::read_column_typed(&plain_pager, plain_page, &[LogicalType::String], 0).unwrap();
    assert_eq!(typed_dictionary, typed_plain);

    // The whole-group typed scan path agrees too.
    let directory_dictionary =
        NodeGroup::read_directory(&dictionary_pager, dictionary_page, &[LogicalType::String])
            .unwrap();
    let columns = NodeGroup::read_columns_typed_from_directory(
        &dictionary_pager,
        directory_dictionary,
        &[LogicalType::String],
    )
    .unwrap();
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0], typed_plain);
}

#[test]
fn dictionary_refuses_inadmissible_types() {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("int64.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    group.push_row(vec![Value::Int64(1)]).unwrap();
    let error = group
        .write_forcing_encodings(&pager, &[(0, 4)])
        .expect_err("dictionary on Int64 must be refused");
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert_eq!(context, "encoding dictionary is not admissible for Int64");
}

/// A hand-built dictionary values section (no validity bitmap).
fn section(dict_count: u32, offsets: &[u32], heap: &[u8], codes: &[u32]) -> Vec<u8> {
    let mut bytes = dict_count.to_le_bytes().to_vec();
    for offset in offsets {
        bytes.extend_from_slice(&offset.to_le_bytes());
    }
    bytes.extend_from_slice(heap);
    for code in codes {
        bytes.extend_from_slice(&code.to_le_bytes());
    }
    bytes
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
    NodeGroup::read(pager, directory_page, &[LogicalType::String])
        .expect_err("doctored payload must not decode")
}

fn page_with(payload: &[u8]) -> Vec<u8> {
    let mut page = vec![0_u8; PAGE_SIZE as usize];
    page[..payload.len()].copy_from_slice(payload);
    page
}

/// Four all-valid rows forced to dictionary, ready for payload doctoring.
fn doctorable_group(directory: &TempDir, name: &str) -> (Pager, u64) {
    write_dictionary_group(
        directory,
        name,
        vec![
            Value::String("apple".to_owned()),
            Value::String("cherry".to_owned()),
            Value::String("pear".to_owned()),
            Value::String("apple".to_owned()),
        ],
    )
}

#[test]
fn dictionary_corruption_matrix_reports_exact_messages() {
    let directory = tempdir().unwrap();
    let (pager, directory_page) = doctorable_group(&directory, "matrix.devondb");
    // Four valid rows → validity byte 0b1111; the doctoring prepends it.
    let payload = |values: &[u8]| {
        let mut payload = vec![0b1111_u8];
        payload.extend_from_slice(values);
        payload
    };

    // Section shorter than the u32 dict_count.
    let error = doctor_payload(&pager, directory_page, &[0b1111, 1, 2, 3]);
    assert_eq!(
        corrupt_context(&error),
        "dictionary section is 3 bytes, smaller than the u32 dict_count 4"
    );

    // dict_count 2 needs 3 offsets + 4 codes; only 2 offsets present.
    let error = doctor_payload(
        &pager,
        directory_page,
        &payload(&section(2, &[0, 1], &[], &[])),
    );
    assert_eq!(
        corrupt_context(&error),
        "dictionary section is 12 bytes, too short for dict_count 2 and 4 codes (32 minimum)"
    );

    // offsets[0] nonzero.
    let error = doctor_payload(
        &pager,
        directory_page,
        &payload(&section(1, &[1, 1], &[], &[0, 0, 0, 0])),
    );
    assert_eq!(
        corrupt_context(&error),
        "dictionary offsets start at 1, expected 0"
    );

    // Decreasing offsets.
    let error = doctor_payload(
        &pager,
        directory_page,
        &payload(&section(2, &[0, 3, 2], b"abc", &[0, 0, 0, 0])),
    );
    assert_eq!(
        corrupt_context(&error),
        "dictionary offsets are not non-decreasing"
    );

    // Offsets end disagrees with the heap length.
    let error = doctor_payload(
        &pager,
        directory_page,
        &payload(&section(1, &[0, 5], b"ab", &[0, 0, 0, 0])),
    );
    assert_eq!(
        corrupt_context(&error),
        "dictionary offsets end at 5 but the heap is 2 bytes"
    );

    // Invalid UTF-8 in an entry.
    let error = doctor_payload(
        &pager,
        directory_page,
        &payload(&section(1, &[0, 1], &[0xff], &[0, 0, 0, 0])),
    );
    assert!(
        corrupt_context(&error).starts_with("dictionary entry 0 is not valid UTF-8: "),
        "{}",
        corrupt_context(&error)
    );

    // Entries out of bytewise order ("b" before "a").
    let error = doctor_payload(
        &pager,
        directory_page,
        &payload(&section(2, &[0, 1, 2], b"ba", &[0, 0, 0, 0])),
    );
    assert_eq!(
        corrupt_context(&error),
        "dictionary entries are not unique and sorted bytewise ascending at entry 1"
    );

    // Duplicate entries ("a" twice).
    let error = doctor_payload(
        &pager,
        directory_page,
        &payload(&section(2, &[0, 1, 2], b"aa", &[0, 0, 0, 0])),
    );
    assert_eq!(
        corrupt_context(&error),
        "dictionary entries are not unique and sorted bytewise ascending at entry 1"
    );

    // Code past the dictionary.
    let error = doctor_payload(
        &pager,
        directory_page,
        &payload(&section(1, &[0, 1], b"a", &[0, 1, 0, 0])),
    );
    assert_eq!(
        corrupt_context(&error),
        "dictionary code 1 at row 1 is out of range for dict_count 1"
    );
}

#[test]
fn dictionary_null_rows_must_carry_code_zero() {
    let directory = tempdir().unwrap();
    let (pager, directory_page) = doctorable_group(&directory, "null-code.devondb");
    // Row 1 NULL → validity 0b1101; its code slot holds 5.
    let mut payload = vec![0b1101_u8];
    payload.extend_from_slice(&section(1, &[0, 1], b"a", &[0, 5, 0, 0]));
    let error = doctor_payload(&pager, directory_page, &payload);
    assert_eq!(
        corrupt_context(&error),
        "dictionary code at NULL row 1 is 5, expected 0"
    );
}

#[test]
fn dictionary_parameters_outside_v1_are_corruption() {
    let directory = tempdir().unwrap();
    let (pager, directory_page) = doctorable_group(&directory, "params.devondb");

    // Rewrite the COLUMN_ENCODINGS section payload [4, 4, 0, 0] with a
    // narrower (reserved) code width [4, 2, 0, 0], fixing the section CRC.
    let mut directory = pager.read_page(directory_page).unwrap();
    let section_start = HEADER_LEN + ENTRY_LEN + SECTION_HEADER_LEN + ZONE_MAP_RECORD_LEN;
    let payload_start = section_start + SECTION_HEADER_LEN;
    let payload = [4, 2, 0, 0];
    assert_eq!(
        u32::from_le_bytes(
            directory[section_start..section_start + 4]
                .try_into()
                .unwrap()
        ),
        4,
        "single-column encodings section is 4 bytes"
    );
    directory[payload_start..payload_start + 4].copy_from_slice(&payload);
    directory[section_start + 4..section_start + 8]
        .copy_from_slice(&crc32c(&payload).to_le_bytes());
    pager.write_page(directory_page, &directory).unwrap();

    let error = NodeGroup::read(&pager, directory_page, &[LogicalType::String])
        .expect_err("code width 2 must be corruption in v1");
    assert_eq!(
        corrupt_context(&error),
        "encoding dictionary parameters are not the v1 spelling [4, 0, 0]"
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
    /// frame-valid directory must never panic the dictionary decoder.
    #[test]
    fn arbitrary_dictionary_payloads_never_panic(
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
        let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
        group
            .push_row(vec![Value::String("ab".to_owned())])
            .unwrap();
        group.push_row(vec![Value::Null]).unwrap();
        let directory_page = group.write_forcing_encodings(&pager, &[(0, 4)]).unwrap();
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
        prop_assert!(outcome.is_ok(), "dictionary decoder panicked");
    }
}
