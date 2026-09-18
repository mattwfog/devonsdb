use crc32c::crc32c;
use devondb_storage::{
    catalog::Catalog,
    node_group::NodeGroup,
    pager::Pager,
    superblock::ZONE_MAPS_FLAG,
    vector_encoding::{decode_b1, decode_f16, decode_i8, encode_b1, encode_f16, encode_i8},
};
use devondb_types::{
    DevonError,
    logical_type::{B1Rescore, LogicalType, VectorEncoding},
    schema::{Column, NodeTableSchema, RelTableSchema},
    value::Value,
};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"vector-wire-test";
const DIRECTORY_HEADER_LEN: usize = 16;
const DIRECTORY_ENTRY_LEN: usize = 16;
// Mirrors `docs/FORMAT.md` § node groups, matching this file's convention of
// restating format law locally rather than importing private storage consts.
const ZONE_MAP_SECTION_FRAME_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;

#[derive(Clone, Copy)]
struct Entry {
    first_page: u64,
    byte_len: u32,
}

#[test]
fn node_ddl_accepts_encoded_columns_but_relationship_ddl_rejects_them() {
    let mut catalog = Catalog::default();
    let node = NodeTableSchema::new(
        "Item".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("embedding", encoded(3, VectorEncoding::F16), false),
        ],
    )
    .unwrap();
    catalog.add_node_table(node).unwrap();

    let rel = RelTableSchema::new(
        "Similar".to_owned(),
        "Item".to_owned(),
        "Item".to_owned(),
        vec![column("embedding", encoded(3, VectorEncoding::I8), false)],
    )
    .unwrap();
    assert!(matches!(
        catalog.add_rel_table(rel),
        Err(DevonError::InvalidArgument { .. })
    ));
}

#[test]
fn f16_main_run_matches_hand_computed_bytes() {
    let ty = encoded(2, VectorEncoding::F16);
    let (_directory, _path, pager, directory_page) = write_one_column(
        ty,
        vec![
            Value::Vector(vec![1.0, -2.0]),
            Value::Null,
            Value::Vector(vec![0.5, -0.0]),
        ],
    );
    let payload = payload(&pager, entry(&pager, directory_page, 0));

    assert_eq!(
        payload,
        [
            0b0000_0101, // validity
            0x00,
            0x3c,
            0x00,
            0xc0, // 1.0, -2.0
            0x00,
            0x00,
            0x00,
            0x00, // null slot
            0x00,
            0x38,
            0x00,
            0x80, // 0.5, -0.0
        ]
    );
}

#[test]
fn i8_main_run_matches_hand_computed_bytes() {
    let ty = encoded(2, VectorEncoding::I8);
    let (_directory, _path, pager, directory_page) =
        write_one_column(ty, vec![Value::Vector(vec![-1.0, 1.0]), Value::Null]);
    let payload = payload(&pager, entry(&pager, directory_page, 0));
    let mut expected = vec![0b0000_0001];
    expected.extend((1.0_f32 / 127.0).to_le_bytes());
    expected.extend(0.0_f32.to_le_bytes());
    expected.extend([0x81, 0x7f]);
    expected.extend([0_u8; 10]);

    assert_eq!(payload, expected);
}

#[test]
fn b1_main_run_matches_hand_computed_lsb_first_bytes() {
    let ty = encoded(
        1,
        VectorEncoding::B1 {
            rotation_seed: 0,
            rescore: B1Rescore::None,
        },
    );
    let (_directory, _path, pager, directory_page) = write_one_column(
        ty,
        vec![
            Value::Vector(vec![1.0]),
            Value::Null,
            Value::Vector(vec![-1.0]),
        ],
    );
    let payload = payload(&pager, entry(&pager, directory_page, 0));

    // SplitMix64(seed=0)'s first output has its low bit set. The initial
    // diagonal therefore negates the sole coordinate; four one-element
    // rounds leave it unchanged.
    assert_eq!(payload, [0b0000_0101, 0x00, 0x00, 0x01]);
}

#[test]
fn b1_rescore_entries_follow_every_main_entry_in_declaration_order() {
    let types = vec![
        LogicalType::Int64,
        b1_type(2, 11, B1Rescore::F16),
        encoded(2, VectorEncoding::F16),
        b1_type(2, 22, B1Rescore::None),
        b1_type(2, 33, B1Rescore::F32),
    ];
    let values = vec![
        Value::Int64(7),
        Value::Vector(vec![1.0, -2.0]),
        Value::Vector(vec![0.5, 4.0]),
        Value::Vector(vec![-1.0, 3.0]),
        Value::Vector(vec![0.25, -0.5]),
    ];
    let (_directory, _path, pager, directory_page) = write_group(types.clone(), vec![values]);
    let directory = pager.read_page(directory_page).unwrap();

    assert_eq!(read_u32(&directory[8..12]), types.len() as u32);
    let entries = (0..7)
        .map(|index| entry_from_page(&directory, index))
        .collect::<Vec<_>>();
    assert!(
        entries
            .windows(2)
            .all(|pair| pair[1].first_page > pair[0].first_page)
    );
    // The enforced-zero tail now begins after the `ZONE_MAP_STATS` section that
    // follows the entry array (SCALE S-1): an 8-byte section frame plus one
    // 24-byte record per MAIN column — the three derived b1 rescore entries
    // carry no stats (`docs/FORMAT.md` § node groups).
    let stats_section_len = ZONE_MAP_SECTION_FRAME_LEN + ZONE_MAP_RECORD_LEN * types.len();
    assert!(
        directory[DIRECTORY_HEADER_LEN + 7 * DIRECTORY_ENTRY_LEN + stats_section_len..]
            .iter()
            .all(|byte| *byte == 0)
    );

    let mut f16_rescore = vec![1];
    f16_rescore.extend([0x00, 0x3c, 0x00, 0xc0]);
    assert_eq!(payload(&pager, entries[5]), f16_rescore);

    let mut f32_rescore = vec![1];
    f32_rescore.extend(0.25_f32.to_le_bytes());
    f32_rescore.extend((-0.5_f32).to_le_bytes());
    assert_eq!(payload(&pager, entries[6]), f32_rescore);
}

#[test]
fn every_encoded_main_and_rescore_null_slot_is_zero() {
    let types = vec![
        encoded(3, VectorEncoding::F16),
        encoded(3, VectorEncoding::I8),
        b1_type(3, 1, B1Rescore::None),
        b1_type(3, 2, B1Rescore::F32),
    ];
    let (_directory, _path, pager, directory_page) = write_group(
        types,
        vec![vec![Value::Null, Value::Null, Value::Null, Value::Null]],
    );

    for index in 0..5 {
        let bytes = payload(&pager, entry(&pager, directory_page, index));
        assert!(bytes.iter().all(|byte| *byte == 0), "entry {index}");
    }
}

#[test]
fn decoded_scan_values_are_stable_across_reopen() {
    let source = vec![0.25, -1.5, 3.75];
    let types = vec![
        encoded(3, VectorEncoding::F16),
        encoded(3, VectorEncoding::I8),
        b1_type(3, 101, B1Rescore::None),
        b1_type(3, 202, B1Rescore::F16),
        b1_type(3, 303, B1Rescore::I8),
        b1_type(3, 404, B1Rescore::F32),
    ];
    let row = vec![Value::Vector(source.clone()); types.len()];
    let (directory, path, pager, directory_page) =
        write_group(types.clone(), vec![row, vec![Value::Null; types.len()]]);
    let before = NodeGroup::read(&pager, directory_page, &types).unwrap();
    drop(pager);

    let reopened = Pager::open(&path).unwrap();
    let after = NodeGroup::read(&reopened, directory_page, &types).unwrap();
    assert_eq!(after, before);
    assert_eq!(after.value(0, 0), Some(&f16_value(&source)));
    assert_eq!(after.value(0, 1), Some(&i8_value(&source)));
    assert_eq!(after.value(0, 2), Some(&b1_value(&source, 101)));
    assert_eq!(after.value(0, 3), Some(&f16_value(&source)));
    assert_eq!(after.value(0, 4), Some(&i8_value(&source)));
    assert_eq!(after.value(0, 5), Some(&Value::Vector(source)));
    for column in 0..types.len() {
        assert_eq!(after.value(1, column), Some(&Value::Null));
    }
    drop(directory);
}

#[test]
fn reserved_i8_code_is_corruption() {
    assert_main_payload_corrupt(
        encoded(1, VectorEncoding::I8),
        Value::Vector(vec![-1.0]),
        |payload| payload[9] = 0x80,
        "encoded vector",
    );
}

#[test]
fn zero_i8_scale_with_nonzero_code_is_corruption() {
    assert_main_payload_corrupt(
        encoded(1, VectorEncoding::I8),
        Value::Vector(vec![4.0]),
        |payload| payload[9] = 1,
        "encoded vector",
    );
}

#[test]
fn negative_and_non_finite_i8_scales_are_corruption() {
    for scale in [-1.0_f32, f32::INFINITY, f32::NAN] {
        assert_main_payload_corrupt(
            encoded(1, VectorEncoding::I8),
            Value::Vector(vec![4.0]),
            move |payload| payload[1..5].copy_from_slice(&scale.to_le_bytes()),
            "encoded vector",
        );
    }
}

#[test]
fn non_finite_i8_offset_is_corruption() {
    assert_main_payload_corrupt(
        encoded(1, VectorEncoding::I8),
        Value::Vector(vec![4.0]),
        |payload| payload[5..9].copy_from_slice(&f32::NEG_INFINITY.to_le_bytes()),
        "encoded vector",
    );
}

#[test]
fn nonzero_b1_padding_bits_are_corruption() {
    assert_main_payload_corrupt(
        b1_type(1, 9, B1Rescore::None),
        Value::Vector(vec![1.0]),
        |payload| payload[1] |= 0x80,
        "padding",
    );
}

#[test]
fn nonzero_null_slots_are_corruption_for_every_main_encoding() {
    for ty in [
        encoded(1, VectorEncoding::F16),
        encoded(1, VectorEncoding::I8),
        b1_type(1, 9, B1Rescore::None),
    ] {
        assert_main_payload_corrupt(ty, Value::Null, |payload| payload[1] = 1, "null value slot");
    }
}

#[test]
fn b1_main_and_rescore_null_positions_must_match() {
    let ty = b1_type(2, 7, B1Rescore::F16);
    let (_directory, _path, pager, directory_page) = write_one_column(ty, vec![Value::Null]);
    let rescore_entry = entry(&pager, directory_page, 1);
    let mut rescore = payload(&pager, rescore_entry);
    rescore[0] = 1;
    replace_payload(&pager, directory_page, 1, rescore_entry, &rescore);

    assert_corrupt_mentions(
        NodeGroup::read(&pager, directory_page, &[ty]),
        "null positions",
    );
}

#[test]
fn b1_rescore_run_must_have_the_directory_row_count() {
    let ty = b1_type(2, 7, B1Rescore::F16);
    let (_directory, _path, pager, directory_page) =
        write_one_column(ty, vec![Value::Vector(vec![1.0, -2.0])]);
    let mut directory = pager.read_page(directory_page).unwrap();
    let byte_len_offset = DIRECTORY_HEADER_LEN + DIRECTORY_ENTRY_LEN + 8;
    let byte_len = read_u32(&directory[byte_len_offset..byte_len_offset + 4]);
    directory[byte_len_offset..byte_len_offset + 4].copy_from_slice(&(byte_len + 1).to_le_bytes());
    pager.write_page(directory_page, &directory).unwrap();

    assert_corrupt_mentions(
        NodeGroup::read(&pager, directory_page, &[ty]),
        "expected exactly",
    );
}

fn encoded(dim: u32, encoding: VectorEncoding) -> LogicalType {
    LogicalType::VectorEncoded { dim, encoding }
}

fn b1_type(dim: u32, rotation_seed: u64, rescore: B1Rescore) -> LogicalType {
    encoded(
        dim,
        VectorEncoding::B1 {
            rotation_seed,
            rescore,
        },
    )
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn write_one_column(
    ty: LogicalType,
    values: Vec<Value>,
) -> (TempDir, std::path::PathBuf, Pager, u64) {
    let rows = values.into_iter().map(|value| vec![value]).collect();
    write_group(vec![ty], rows)
}

fn write_group(
    types: Vec<LogicalType>,
    rows: Vec<Vec<Value>>,
) -> (TempDir, std::path::PathBuf, Pager, u64) {
    let directory = tempdir().unwrap();
    let path = directory.path().join("vectors.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(types).unwrap();
    for row in rows {
        group.push_row(row).unwrap();
    }
    let directory_page = group.write(&pager).unwrap();

    // These fixtures write groups straight through the pager and never publish
    // a catalog, so `Catalog::save` — the only code that claims `ZONE_MAPS` —
    // never runs. Since SCALE S-1 the group above carries a `ZONE_MAP_STATS`
    // directory section, and a set section bit whose governing feature bit is
    // clear is corruption by format law (`docs/FORMAT.md` § node groups).
    // Publish it the way a real checkpoint does; the pager enforces
    // checkpoint-LSN monotonicity on every commit.
    let mut superblock = pager.superblock();
    superblock.feature_flags |= ZONE_MAPS_FLAG;
    superblock.checkpoint_lsn += 1;
    pager.commit_superblock(superblock).unwrap();

    (directory, path, pager, directory_page)
}

fn entry(pager: &Pager, directory_page: u64, index: usize) -> Entry {
    entry_from_page(&pager.read_page(directory_page).unwrap(), index)
}

fn entry_from_page(directory: &[u8], index: usize) -> Entry {
    let offset = DIRECTORY_HEADER_LEN + index * DIRECTORY_ENTRY_LEN;
    Entry {
        first_page: read_u64(&directory[offset..offset + 8]),
        byte_len: read_u32(&directory[offset + 8..offset + 12]),
    }
}

fn payload(pager: &Pager, entry: Entry) -> Vec<u8> {
    let byte_len = entry.byte_len as usize;
    let mut payload = Vec::with_capacity(byte_len);
    for page_offset in 0..byte_len.div_ceil(PAGE_SIZE as usize) {
        let page = pager
            .read_page(entry.first_page + page_offset as u64)
            .unwrap();
        let take = (byte_len - payload.len()).min(PAGE_SIZE as usize);
        payload.extend_from_slice(&page[..take]);
    }
    payload
}

fn replace_payload(
    pager: &Pager,
    directory_page: u64,
    entry_index: usize,
    entry: Entry,
    replacement: &[u8],
) {
    assert_eq!(replacement.len(), entry.byte_len as usize);
    for (page_offset, chunk) in replacement.chunks(PAGE_SIZE as usize).enumerate() {
        let page_id = entry.first_page + page_offset as u64;
        let mut page = pager.read_page(page_id).unwrap();
        page[..chunk.len()].copy_from_slice(chunk);
        pager.write_page(page_id, &page).unwrap();
    }
    let mut directory = pager.read_page(directory_page).unwrap();
    let checksum_offset = DIRECTORY_HEADER_LEN + entry_index * DIRECTORY_ENTRY_LEN + 12;
    directory[checksum_offset..checksum_offset + 4]
        .copy_from_slice(&crc32c(replacement).to_le_bytes());
    pager.write_page(directory_page, &directory).unwrap();
}

fn assert_main_payload_corrupt(
    ty: LogicalType,
    value: Value,
    mutate: impl FnOnce(&mut Vec<u8>),
    expected: &str,
) {
    let (_directory, _path, pager, directory_page) = write_one_column(ty, vec![value]);
    let main_entry = entry(&pager, directory_page, 0);
    let mut bytes = payload(&pager, main_entry);
    mutate(&mut bytes);
    replace_payload(&pager, directory_page, 0, main_entry, &bytes);
    assert_corrupt_mentions(NodeGroup::read(&pager, directory_page, &[ty]), expected);
}

fn assert_corrupt_mentions<T>(result: Result<T, DevonError>, expected: &str) {
    let error = match result {
        Ok(_) => panic!("expected Corrupt"),
        Err(error) => error,
    };
    let DevonError::Corrupt { context } = error else {
        panic!("expected Corrupt");
    };
    assert!(
        context.contains(expected),
        "unexpected corruption: {context}"
    );
}

fn f16_value(source: &[f32]) -> Value {
    Value::Vector(decode_f16(&encode_f16(source)).unwrap())
}

fn i8_value(source: &[f32]) -> Value {
    Value::Vector(decode_i8(&encode_i8(source)).unwrap())
}

fn b1_value(source: &[f32], seed: u64) -> Value {
    Value::Vector(decode_b1(&encode_b1(source, seed), source.len(), seed).unwrap())
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().unwrap())
}
