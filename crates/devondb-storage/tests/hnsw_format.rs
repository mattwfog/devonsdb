use crc32c::crc32c;
use devondb_storage::hnsw::{
    format::{HnswRoot, LayerDirectory},
    types::{HnswConfig, HnswMetric, NavigationEncoding},
};
use devondb_types::DevonError;

const PAGE_SIZE: usize = 4096;
const DIRECTORY_CRC32C: u32 = 0xdcf3_697c;

const EMPTY_ROOT_HEADER: [u8; 64] = [
    0x48, 0x4e, 0x53, 0x57, 0x01, 0x00, 0x00, 0x00, // magic through encoding
    0x10, 0x00, 0x20, 0x00, 0xc8, 0x00, 0x00, 0x00, // M, M0, ef_construction
    0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // level_seed
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // covered_rows
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // no entry node
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // levels, reserved, groups
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // directory first page
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // directory length and CRC
];

const THREE_LAYER_TWO_GROUP_DIRECTORY: [u8; 48] = [
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // layer 0, group 0
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // layer 0, group 1: empty
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // layer 1, group 0
    0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // layer 1, group 1
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // layer 2, group 0: empty
    0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // layer 2, group 1
];

fn empty_root() -> HnswRoot {
    HnswRoot {
        config: HnswConfig::with_defaults(
            0x0102_0304_0506_0708,
            HnswMetric::L2,
            NavigationEncoding::F32,
        ),
        covered_rows: 0,
        entry_node: None,
        entry_level: 0,
        layer_count: 0,
        group_count: 0,
        layer_dir_first_page: 0,
        layer_dir_byte_len: 0,
        layer_dir_crc32c: 0,
    }
}

fn populated_root() -> HnswRoot {
    HnswRoot {
        config: HnswConfig::with_defaults(
            0x8877_6655_4433_2211,
            HnswMetric::Cosine,
            NavigationEncoding::F16,
        ),
        covered_rows: 2,
        entry_node: Some(1),
        entry_level: 2,
        layer_count: 3,
        group_count: 2,
        layer_dir_first_page: 9,
        layer_dir_byte_len: 48,
        layer_dir_crc32c: DIRECTORY_CRC32C,
    }
}

fn encoded_root(root: &HnswRoot) -> Vec<u8> {
    let mut page = vec![0xa5; PAGE_SIZE];
    root.encode(&mut page).unwrap();
    page
}

fn assert_corrupt(page: &[u8]) {
    assert!(matches!(
        HnswRoot::decode(page),
        Err(DevonError::Corrupt { .. })
    ));
}

fn assert_mutation_rejected(page: &[u8], mutate: impl FnOnce(&mut [u8])) {
    let mut corrupt = page.to_vec();
    mutate(&mut corrupt);
    assert_corrupt(&corrupt);
}

#[test]
fn empty_root_bytes_match_hand_computed_golden() {
    let actual = encoded_root(&empty_root());
    let mut expected = EMPTY_ROOT_HEADER.to_vec();
    expected.resize(PAGE_SIZE, 0);
    assert_eq!(actual, expected);
    assert_eq!(HnswRoot::decode(&actual).unwrap(), empty_root());
}

#[test]
fn three_layer_two_group_directory_matches_hand_computed_golden() {
    let directory = LayerDirectory::new(3, 2, vec![2, 0, 3, 4, 0, 5]).unwrap();
    assert_eq!(directory.encode(), THREE_LAYER_TWO_GROUP_DIRECTORY);
    assert_eq!(directory.byte_len(), 48);
    assert_eq!(directory.crc32c(), DIRECTORY_CRC32C);

    let decoded =
        LayerDirectory::decode(&THREE_LAYER_TWO_GROUP_DIRECTORY, &populated_root()).unwrap();
    assert_eq!(decoded, directory);
    assert_eq!(decoded.page_id(0, 1).unwrap(), None);
    assert_eq!(decoded.page_id(1, 1).unwrap(), Some(4));
}

#[test]
fn root_rejects_fixed_header_and_configuration_corruption() {
    let page = encoded_root(&populated_root());
    assert_mutation_rejected(&page, |bytes| bytes[0] = b'X');
    assert_mutation_rejected(&page, |bytes| {
        bytes[4..6].copy_from_slice(&2_u16.to_le_bytes())
    });
    assert_mutation_rejected(&page, |bytes| bytes[6] = 2);
    assert_mutation_rejected(&page, |bytes| bytes[7] = 4);
    assert_mutation_rejected(&page, |bytes| {
        bytes[8..10].copy_from_slice(&3_u16.to_le_bytes())
    });
    assert_mutation_rejected(&page, |bytes| {
        bytes[8..10].copy_from_slice(&65_u16.to_le_bytes())
    });
    assert_mutation_rejected(&page, |bytes| {
        bytes[10..12].copy_from_slice(&31_u16.to_le_bytes());
    });
    assert_mutation_rejected(&page, |bytes| {
        bytes[12..16].copy_from_slice(&31_u32.to_le_bytes());
    });
    assert_mutation_rejected(&page, |bytes| {
        bytes[12..16].copy_from_slice(&4097_u32.to_le_bytes());
    });
}

#[test]
fn root_rejects_entry_and_layer_corruption() {
    let page = encoded_root(&populated_root());
    assert_mutation_rejected(&page, |bytes| {
        bytes[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    });
    assert_mutation_rejected(&page, |bytes| {
        bytes[32..40].copy_from_slice(&2_u64.to_le_bytes());
    });
    assert_mutation_rejected(&page, |bytes| bytes[40] = 64);
    assert_mutation_rejected(&page, |bytes| bytes[41] = 2);

    let empty = encoded_root(&empty_root());
    assert_mutation_rejected(&empty, |bytes| bytes[40] = 1);
    assert_mutation_rejected(&empty, |bytes| bytes[41] = 1);
}

#[test]
fn root_rejects_coverage_and_directory_reference_corruption() {
    let page = encoded_root(&populated_root());
    assert_mutation_rejected(&page, |bytes| {
        bytes[24..32].copy_from_slice(&1_u64.to_le_bytes());
    });
    assert_mutation_rejected(&page, |bytes| {
        bytes[44..48].copy_from_slice(&0_u32.to_le_bytes());
    });
    assert_mutation_rejected(&page, |bytes| {
        bytes[48..56].copy_from_slice(&0_u64.to_le_bytes());
    });
    assert_mutation_rejected(&page, |bytes| {
        bytes[48..56].copy_from_slice(&1_u64.to_le_bytes());
    });
    assert_mutation_rejected(&page, |bytes| {
        bytes[56..60].copy_from_slice(&47_u32.to_le_bytes());
    });

    let empty = encoded_root(&empty_root());
    assert_mutation_rejected(&empty, |bytes| {
        bytes[48..56].copy_from_slice(&2_u64.to_le_bytes());
    });
    assert_mutation_rejected(&empty, |bytes| {
        bytes[60..64].copy_from_slice(&1_u32.to_le_bytes());
    });
}

#[test]
fn root_rejects_each_reserved_byte_nonzero_tail_and_truncation() {
    let page = encoded_root(&populated_root());
    assert_mutation_rejected(&page, |bytes| bytes[42] = 1);
    assert_mutation_rejected(&page, |bytes| bytes[43] = 1);
    assert_mutation_rejected(&page, |bytes| bytes[64] = 1);
    assert_corrupt(&page[..63]);
}

#[test]
fn directory_rejects_exact_length_and_dimension_mismatches() {
    let payload = THREE_LAYER_TWO_GROUP_DIRECTORY;
    let root = populated_root();
    assert!(matches!(
        LayerDirectory::decode(&payload[..47], &root),
        Err(DevonError::Corrupt { .. })
    ));

    let mut extended = payload.to_vec();
    extended.push(0);
    assert!(matches!(
        LayerDirectory::decode(&extended, &root),
        Err(DevonError::Corrupt { .. })
    ));
    assert!(LayerDirectory::new(3, 2, vec![2, 0, 3, 4, 0]).is_err());

    let mut dimension_root = root;
    dimension_root.covered_rows = 3;
    dimension_root.group_count = 3;
    dimension_root.layer_dir_byte_len = 72;
    assert!(matches!(
        LayerDirectory::decode(&payload, &dimension_root),
        Err(DevonError::Corrupt { .. })
    ));
}

#[test]
fn directory_rejects_crc_corruption() {
    let root = populated_root();
    let mut payload = THREE_LAYER_TWO_GROUP_DIRECTORY;
    payload[0] ^= 1;
    assert!(matches!(
        LayerDirectory::decode(&payload, &root),
        Err(DevonError::Corrupt { .. })
    ));

    let mut bad_checksum = root;
    bad_checksum.layer_dir_crc32c ^= 1;
    assert!(matches!(
        LayerDirectory::decode(&THREE_LAYER_TWO_GROUP_DIRECTORY, &bad_checksum),
        Err(DevonError::Corrupt { .. })
    ));
}

#[test]
fn directory_rejects_reserved_page_ids_but_accepts_zero_empty_cells() {
    let mut payload = THREE_LAYER_TWO_GROUP_DIRECTORY;
    payload[..8].copy_from_slice(&1_u64.to_le_bytes());
    let mut root = populated_root();
    root.layer_dir_crc32c = crc32c(&payload);
    assert!(matches!(
        LayerDirectory::decode(&payload, &root),
        Err(DevonError::Corrupt { .. })
    ));

    let all_null_root = HnswRoot {
        covered_rows: 2,
        group_count: 2,
        ..empty_root()
    };
    let empty = LayerDirectory::decode(&[], &all_null_root).unwrap();
    assert_eq!(empty.layer_count(), 0);
    assert_eq!(empty.group_count(), 2);
    assert!(empty.page_ids().is_empty());
}
