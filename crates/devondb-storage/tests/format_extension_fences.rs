use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use devondb_storage::csr_group::CsrGroup;
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, RESERVED_READ_SAFE_FLAG, ZONE_MAPS_FLAG};
use devondb_types::{DevonError, logical_type::LogicalType, value::Value};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"format-fence-db!";
const DIRECTORY_HEADER_LEN: usize = 16;
const DIRECTORY_ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;
const ZONE_MAP_DIRECTORY_FLAG: u32 = 1 << 0;
const COLUMN_ENCODINGS_DIRECTORY_FLAG: u32 = 1 << 1;
const RESERVED_READ_SAFE_DIRECTORY_FLAG: u32 = 1 << 2;
const UNREGISTERED_DIRECTORY_FLAG: u32 = 1 << 3;

#[test]
fn unregistered_directory_bit_is_corruption() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("node-flags.devondb");
    let types = node_types();
    let group = node_group();
    let directory_page = write_node_group(&path, &group);

    let flags_offset = directory_page * u64::from(PAGE_SIZE) + 12;
    let original_flags = read_file_bytes(&path, flags_offset, 4);
    assert_eq!(
        u32::from_le_bytes(original_flags.clone().try_into().unwrap()),
        1
    );
    write_file_bytes(
        &path,
        flags_offset,
        &(ZONE_MAP_DIRECTORY_FLAG | UNREGISTERED_DIRECTORY_FLAG).to_le_bytes(),
    );
    assert_node_group_corrupt(
        &path,
        directory_page,
        &types,
        "node-group directory bit 3 is unregistered (unknown bits 0x8)",
    );
    write_file_bytes(&path, flags_offset, &original_flags);
    assert_node_group_reads(&path, directory_page, &types, &group);
}

#[test]
fn registered_directory_bits_require_their_governors() {
    let directory = tempdir().unwrap();
    let types = node_types();
    let group = node_group();
    let cases = [
        (
            ZONE_MAP_DIRECTORY_FLAG,
            ZONE_MAPS_FLAG,
            "node-group directory ZONE_MAP_STATS bit is set but ZONE_MAPS feature is clear",
        ),
        (
            COLUMN_ENCODINGS_DIRECTORY_FLAG,
            COLUMN_ENCODINGS_FLAG,
            "node-group directory COLUMN_ENCODINGS bit is set but COLUMN_ENCODINGS feature is clear",
        ),
        (
            RESERVED_READ_SAFE_DIRECTORY_FLAG,
            RESERVED_READ_SAFE_FLAG,
            "node-group directory bit 2 is set but governing superblock feature bit 12 is clear",
        ),
    ];

    for (directory_bit, (directory_flag, governor, expected)) in cases.into_iter().enumerate() {
        let path = directory
            .path()
            .join(format!("governor-clear-{directory_bit}.devondb"));
        let directory_page = write_node_group(&path, &group);
        set_feature_bits(&path, 0, governor);
        set_directory_flags(
            &path,
            directory_page,
            ZONE_MAP_DIRECTORY_FLAG | directory_flag,
        );
        assert_node_group_corrupt(&path, directory_page, &types, expected);
    }
}

#[test]
fn unsupported_read_safe_directory_section_is_skipped_by_length() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("opaque-section.devondb");
    let types = node_types();
    let group = node_group();
    let directory_page = write_node_group(&path, &group);
    let section_offset = node_sections_end(types.len());
    let mut frame = Vec::from(4_u32.to_le_bytes());
    frame.extend_from_slice(&0xdead_beef_u32.to_le_bytes());
    frame.extend_from_slice(&[0x81, 0x27, 0x44, 0xfa]);
    write_page_bytes(&path, directory_page, section_offset, &frame);
    set_directory_flags(
        &path,
        directory_page,
        ZONE_MAP_DIRECTORY_FLAG | RESERVED_READ_SAFE_DIRECTORY_FLAG,
    );
    set_feature_bits(&path, RESERVED_READ_SAFE_FLAG, 0);

    let pager = Pager::open(&path).unwrap();
    assert!(pager.superblock().requires_read_only());
    assert_eq!(
        NodeGroup::read(&pager, directory_page, &types).unwrap(),
        group
    );
    assert_eq!(
        NodeGroup::read_row_count(&pager, directory_page, types.len()).unwrap(),
        group.row_count()
    );
}

#[test]
fn unsupported_read_safe_directory_section_still_checks_frame_bounds() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("opaque-section-overrun.devondb");
    let types = node_types();
    let group = node_group();
    let directory_page = write_node_group(&path, &group);
    let section_offset = node_sections_end(types.len());
    let mut frame = Vec::from(PAGE_SIZE.to_le_bytes());
    frame.extend_from_slice(&0_u32.to_le_bytes());
    write_page_bytes(&path, directory_page, section_offset, &frame);
    set_directory_flags(
        &path,
        directory_page,
        ZONE_MAP_DIRECTORY_FLAG | RESERVED_READ_SAFE_DIRECTORY_FLAG,
    );
    set_feature_bits(&path, RESERVED_READ_SAFE_FLAG, 0);

    let pager = Pager::open(&path).unwrap();
    assert_corrupt_context(
        NodeGroup::read(&pager, directory_page, &types),
        "node-group directory section bit 2 length overruns the page",
    );
}

#[test]
fn pending_directory_permission_does_not_leak_to_a_fresh_handle() {
    let directory = tempdir().unwrap();
    let group = node_group();
    let writer_path = directory.path().join("writer.devondb");
    let writer = Pager::create(&writer_path, PAGE_SIZE, DB_ID).unwrap();
    group.write_forcing_encodings(&writer, &[]).unwrap();

    let foreign_path = directory.path().join("foreign.devondb");
    let directory_page = write_node_group(&foreign_path, &group);
    set_feature_bits(&foreign_path, 0, ZONE_MAPS_FLAG);
    let fresh = Pager::open(&foreign_path).unwrap();
    assert_corrupt_context(
        NodeGroup::read(&fresh, directory_page, &node_types()),
        "node-group directory ZONE_MAP_STATS bit is set but ZONE_MAPS feature is clear",
    );
    drop(writer);
}

#[test]
fn node_group_padding_rejection_is_format_extension_law() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("node-padding.devondb");
    let types = node_types();
    let group = node_group();
    let directory_page = write_node_group(&path, &group);
    let entries_end = DIRECTORY_HEADER_LEN + DIRECTORY_ENTRY_LEN * types.len();
    let sections_end = entries_end + SECTION_HEADER_LEN + ZONE_MAP_RECORD_LEN * types.len();

    for page_offset in padding_positions(sections_end) {
        let original = replace_page_byte(&path, directory_page, page_offset, 0x5a);
        assert_eq!(original, 0);

        let pager = Pager::open(&path).unwrap();
        assert_corrupt_context(
            NodeGroup::read(&pager, directory_page, &types),
            "node-group directory padding is not zero",
        );
        drop(pager);

        replace_page_byte(&path, directory_page, page_offset, original);
        assert_node_group_reads(&path, directory_page, &types, &group);
    }
}

#[test]
fn csr_directory_padding_rejection_is_format_extension_law() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("csr-padding.devondb");
    let types = csr_types();
    let group = csr_group();
    let directory_page = write_csr_group(&path, &group);
    let entry_count = 2 + types.len();
    let entries_end = DIRECTORY_HEADER_LEN + DIRECTORY_ENTRY_LEN * entry_count;

    for page_offset in padding_positions(entries_end) {
        let original = replace_page_byte(&path, directory_page, page_offset, 0xc3);
        assert_eq!(original, 0);

        let pager = Pager::open(&path).unwrap();
        assert_corrupt_context(
            CsrGroup::read(&pager, directory_page, &types),
            "CSR directory padding is not zero",
        );
        drop(pager);

        replace_page_byte(&path, directory_page, page_offset, original);
        let pager = Pager::open(&path).unwrap();
        assert_eq!(
            CsrGroup::read(&pager, directory_page, &types).unwrap(),
            group
        );
    }
}

fn node_types() -> Vec<LogicalType> {
    vec![LogicalType::String, LogicalType::Int64]
}

fn node_group() -> NodeGroup {
    let mut group = NodeGroup::new(node_types()).unwrap();
    group
        .push_row(vec![Value::String("alpha".to_owned()), Value::Int64(7)])
        .unwrap();
    group
        .push_row(vec![Value::String("beta".to_owned()), Value::Null])
        .unwrap();
    group
}

fn csr_types() -> Vec<LogicalType> {
    vec![LogicalType::String, LogicalType::Int64]
}

fn csr_group() -> CsrGroup {
    let mut group = CsrGroup::new(3, csr_types()).unwrap();
    group
        .push_edge(
            0,
            4,
            vec![Value::String("outgoing".to_owned()), Value::Int64(11)],
        )
        .unwrap();
    group
        .push_edge(
            2,
            1,
            vec![Value::String("incoming".to_owned()), Value::Null],
        )
        .unwrap();
    group
}

fn write_node_group(path: &Path, group: &NodeGroup) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = group.write_forcing_encodings(&pager, &[]).unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn = 1;
    superblock.feature_flags |= ZONE_MAPS_FLAG;
    pager.commit_superblock(superblock).unwrap();
    directory_page
}

fn write_csr_group(path: &Path, group: &CsrGroup) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
    group.write(&pager).unwrap()
}

fn padding_positions(entries_end: usize) -> [usize; 3] {
    let final_byte = PAGE_SIZE as usize - 1;
    let middle_byte = entries_end + (final_byte - entries_end) / 2;
    [entries_end, middle_byte, final_byte]
}

fn node_sections_end(column_count: usize) -> usize {
    DIRECTORY_HEADER_LEN
        + DIRECTORY_ENTRY_LEN * column_count
        + SECTION_HEADER_LEN
        + ZONE_MAP_RECORD_LEN * column_count
}

fn set_directory_flags(path: &Path, directory_page: u64, flags: u32) {
    write_page_bytes(path, directory_page, 12, &flags.to_le_bytes());
}

fn write_page_bytes(path: &Path, page_id: u64, page_offset: usize, bytes: &[u8]) {
    let file_offset = page_id * u64::from(PAGE_SIZE) + page_offset as u64;
    write_file_bytes(path, file_offset, bytes);
}

fn replace_page_byte(path: &Path, page_id: u64, page_offset: usize, value: u8) -> u8 {
    let file_offset = page_id * u64::from(PAGE_SIZE) + page_offset as u64;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(file_offset)).unwrap();
    let mut original = [0_u8; 1];
    file.read_exact(&mut original).unwrap();
    file.seek(SeekFrom::Start(file_offset)).unwrap();
    file.write_all(&[value]).unwrap();
    file.sync_all().unwrap();
    original[0]
}

fn read_file_bytes(path: &Path, offset: u64, len: usize) -> Vec<u8> {
    let mut file = OpenOptions::new().read(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut bytes = vec![0_u8; len];
    file.read_exact(&mut bytes).unwrap();
    bytes
}

fn write_file_bytes(path: &Path, offset: u64, bytes: &[u8]) {
    let mut file = OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

fn set_feature_bits(path: &Path, set: u64, clear: u64) {
    let pager = Pager::open(path).unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn += 1;
    superblock.feature_flags |= set;
    superblock.feature_flags &= !clear;
    pager.commit_superblock(superblock).unwrap();
}

fn assert_node_group_corrupt(
    path: &Path,
    directory_page: u64,
    types: &[LogicalType],
    expected: &str,
) {
    let pager = Pager::open(path).unwrap();
    assert_corrupt_context(NodeGroup::read(&pager, directory_page, types), expected);
    assert_corrupt_context(
        NodeGroup::read_row_count(&pager, directory_page, types.len()),
        expected,
    );
}

fn assert_node_group_reads(
    path: &Path,
    directory_page: u64,
    types: &[LogicalType],
    expected: &NodeGroup,
) {
    let pager = Pager::open(path).unwrap();
    assert_eq!(
        NodeGroup::read(&pager, directory_page, types).unwrap(),
        *expected
    );
    assert_eq!(
        NodeGroup::read_row_count(&pager, directory_page, types.len()).unwrap(),
        expected.row_count()
    );
}

fn assert_corrupt_context<T>(result: Result<T, DevonError>, expected: &str) {
    let Err(DevonError::Corrupt { context }) = result else {
        panic!("expected DevonError::Corrupt");
    };
    assert_eq!(context, expected);
}
