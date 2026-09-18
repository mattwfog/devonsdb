use crc32c::crc32c;
use devondb_storage::catalog::{
    Catalog, IndexEntry, IndexKind, InterfaceColumn, InterfaceEntry, NodeClassEntry, PinEntry,
    RelClassEntry, RelStorage, TableStorage,
};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{
    READ_SAFE_FLAG_MASK, SUPPORTED_FLAG_MASK, SUPPORTED_FORMAT_VERSION, Superblock, ZONE_MAPS_FLAG,
};
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema, RelTableSchema};
use devondb_types::{DevonError, DevonResult, value::Value as DevonValue};
use proptest::collection;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use serde_json::{Value, json};
use tempfile::tempdir;

const PROPTEST_SEED: u64 = 0x4456_4e53_544f_5230;
const PAGE_SIZE: usize = 4096;
const CATALOG_HEADER_LEN: usize = 8;
const MAX_CATALOG_PAYLOAD: usize = PAGE_SIZE - CATALOG_HEADER_LEN;
const DB_ID: [u8; 16] = *b"decode-fuzz-db!!";
const NODE_DIRECTORY_FLAGS_OFFSET: usize = 12;
const NODE_SECTION_START: usize = 32;
const NODE_SECTION_PAYLOAD_START: usize = NODE_SECTION_START + 8;

fn arbitrary_superblock_page() -> BoxedStrategy<Vec<u8>> {
    let arbitrary = collection::vec(any::<u8>(), 64..=PAGE_SIZE);
    let truncated = collection::vec(any::<u8>(), 0..64);
    let valid_magic_and_arbitrary_fields = (
        any::<u32>(),
        any::<u32>(),
        any::<u64>(),
        any::<u32>(),
        any::<[u8; 16]>(),
        any::<u64>(),
        any::<u64>(),
        collection::vec(any::<u8>(), 0..=192),
    )
        .prop_map(
            |(format, minimum, flags, page_size, db_id, lsn, root, tail)| {
                let mut page = vec![0_u8; 64 + tail.len()];
                page[..8].copy_from_slice(b"DEVONDB\0");
                page[8..12].copy_from_slice(&format.to_le_bytes());
                page[12..16].copy_from_slice(&minimum.to_le_bytes());
                page[16..24].copy_from_slice(&flags.to_le_bytes());
                page[24..28].copy_from_slice(&page_size.to_le_bytes());
                page[28..44].copy_from_slice(&db_id);
                page[44..52].copy_from_slice(&lsn.to_le_bytes());
                page[52..60].copy_from_slice(&root.to_le_bytes());
                let checksum = crc32c(&page[..60]);
                page[60..64].copy_from_slice(&checksum.to_le_bytes());
                page[64..].copy_from_slice(&tail);
                page
            },
        );

    prop_oneof![6 => arbitrary, 2 => valid_magic_and_arbitrary_fields, 2 => truncated].boxed()
}

fn jsonish_payload() -> BoxedStrategy<Vec<u8>> {
    let arbitrary_json_string = collection::vec(
        prop::sample::select(vec!['a', 'Z', '0', ' ', '\n', 'é', '🦀']),
        0..=900,
    )
    .prop_map(|characters| {
        serde_json::to_vec(&characters.into_iter().collect::<String>()).unwrap()
    });
    let huge_string = (0..=MAX_CATALOG_PAYLOAD - 2)
        .prop_map(|length| format!("\"{}\"", "a".repeat(length)).into_bytes());
    let deep_array = (0_usize..=1024)
        .prop_map(|depth| format!("{}0{}", "[".repeat(depth), "]".repeat(depth)).into_bytes());
    let fragments = prop::sample::select(
        [
            b"{".as_slice(),
            b"[".as_slice(),
            b"\"".as_slice(),
            b"null".as_slice(),
            b"{\"node_tables\":".as_slice(),
            b"{\"ontology\":{\"interfaces\":[".as_slice(),
        ]
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>(),
    );
    let catalog_shapes = prop::sample::select(interesting_catalog_payloads());

    prop_oneof![
        3 => arbitrary_json_string,
        2 => huge_string,
        2 => deep_array,
        2 => fragments,
        3 => catalog_shapes,
    ]
    .boxed()
}

fn arbitrary_catalog_payload() -> BoxedStrategy<Vec<u8>> {
    prop_oneof![
        5 => collection::vec(any::<u8>(), 0..=MAX_CATALOG_PAYLOAD),
        5 => jsonish_payload(),
    ]
    .boxed()
}

fn catalog_header_mutation() -> BoxedStrategy<(Vec<u8>, &'static str)> {
    let oversized_length = ((MAX_CATALOG_PAYLOAD as u32 + 1)..=u32::MAX).prop_map(|length| {
        let mut page = vec![0_u8; PAGE_SIZE];
        page[..4].copy_from_slice(&length.to_le_bytes());
        (page, "catalog payload length exceeds page capacity")
    });
    let wrong_checksum = any::<u32>().prop_map(|candidate| {
        let mut page = catalog_page(&empty_catalog_payload());
        let checksum = u32::from_le_bytes(page[4..8].try_into().unwrap());
        let wrong = if candidate == checksum {
            !candidate
        } else {
            candidate
        };
        page[4..8].copy_from_slice(&wrong.to_le_bytes());
        (page, "catalog payload checksum does not match")
    });
    let payload = empty_catalog_payload();
    let padding_start = CATALOG_HEADER_LEN + payload.len();
    let nonzero_padding =
        (padding_start..PAGE_SIZE, 1_u8..=u8::MAX).prop_map(move |(offset, byte)| {
            let mut page = catalog_page(&payload);
            page[offset] = byte;
            (page, "catalog page padding is not zero")
        });

    prop_oneof![oversized_length, wrong_checksum, nonzero_padding].boxed()
}

fn catalog_page(payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= MAX_CATALOG_PAYLOAD);
    let mut page = vec![0_u8; PAGE_SIZE];
    page[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    page[4..8].copy_from_slice(&crc32c(payload).to_le_bytes());
    page[CATALOG_HEADER_LEN..CATALOG_HEADER_LEN + payload.len()].copy_from_slice(payload);
    page
}

fn decode_catalog_page(page: &[u8]) -> DevonResult<Catalog> {
    assert_eq!(page.len(), PAGE_SIZE);
    let directory = tempdir().unwrap();
    let pager = Pager::create(
        directory.path().join("fuzz.devondb"),
        PAGE_SIZE as u32,
        DB_ID,
    )
    .unwrap();
    let root = pager.allocate_page().unwrap();
    pager.write_page(root, page).unwrap();
    let mut superblock = pager.superblock();
    superblock.catalog_root = root;
    superblock.checkpoint_lsn = 1;
    pager.commit_superblock(superblock).unwrap();
    Catalog::load(&pager)
}

fn valid_node_directory_page() -> Vec<u8> {
    let directory = tempdir().unwrap();
    let pager = Pager::create(
        directory.path().join("node-source.devondb"),
        PAGE_SIZE as u32,
        DB_ID,
    )
    .unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    group.push_row(vec![DevonValue::Int64(7)]).unwrap();
    let directory_page = group.write(&pager).unwrap();
    pager.read_page(directory_page).unwrap()
}

fn decode_node_directory_page(page: &[u8]) -> DevonResult<NodeGroup> {
    assert_eq!(page.len(), PAGE_SIZE);
    let directory = tempdir().unwrap();
    let pager = Pager::create(
        directory.path().join("node-decode.devondb"),
        PAGE_SIZE as u32,
        DB_ID,
    )
    .unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    group.push_row(vec![DevonValue::Int64(7)]).unwrap();
    let directory_page = group.write(&pager).unwrap();
    pager.write_page(directory_page, page).unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn = 1;
    superblock.feature_flags |= ZONE_MAPS_FLAG;
    pager.commit_superblock(superblock).unwrap();
    NodeGroup::read(&pager, directory_page, &[LogicalType::Int64])
}

fn corrupt_context(result: DevonResult<Catalog>) -> String {
    match result {
        Err(DevonError::Corrupt { context }) => context,
        Err(other) => panic!("expected Corrupt, got {other}"),
        Ok(catalog) => panic!("expected Corrupt, decoded {catalog:?}"),
    }
}

fn empty_catalog_payload() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "node_tables": [],
        "rel_tables": [],
        "storage": {}
    }))
    .unwrap()
}

fn base_catalog_value() -> Value {
    json!({
        "node_tables": [{
            "name": "Person",
            "columns": [
                {"name": "id", "ty": "Int64", "primary_key": true},
                {"name": "name", "ty": "String", "primary_key": false},
                {"name": "age", "ty": "Int64", "primary_key": false},
                {"name": "embedding", "ty": {"Vector": {"dim": 4}}, "primary_key": false}
            ]
        }],
        "rel_tables": [{
            "name": "Knows", "from": "Person", "to": "Person", "columns": []
        }],
        "storage": {}
    })
}

fn pins_catalog_value(pins: Value) -> Value {
    json!({
        "node_tables": [],
        "rel_tables": [],
        "storage": {},
        "pins": pins
    })
}

fn pins_catalog_payload(section: &[u8]) -> Vec<u8> {
    let prefix = br#"{"node_tables":[],"rel_tables":[],"storage":{},"pins":"#;
    let mut payload = Vec::with_capacity(prefix.len() + section.len() + 1);
    payload.extend_from_slice(prefix);
    payload.extend_from_slice(section);
    payload.push(b'}');
    assert!(payload.len() <= MAX_CATALOG_PAYLOAD);
    payload
}

fn pins_corrupt_context(pins: Value) -> String {
    let payload = serde_json::to_vec(&pins_catalog_value(pins)).unwrap();
    corrupt_context(decode_catalog_page(&catalog_page(&payload)))
}

fn small_json_string() -> BoxedStrategy<String> {
    collection::vec(
        prop::sample::select(vec!['a', 'Z', '0', ' ', '\n', 'é', '🦀']),
        0..=12,
    )
    .prop_map(|characters| characters.into_iter().collect())
    .boxed()
}

fn small_json_value() -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|value| Value::Number(value.into())),
        small_json_string().prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            collection::vec(inner.clone(), 0..=4).prop_map(Value::Array),
            collection::btree_map(small_json_string(), inner, 0..=4)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
    .boxed()
}

fn arbitrary_pins_section() -> BoxedStrategy<Vec<u8>> {
    let arbitrary_json = small_json_value().prop_map(|value| serde_json::to_vec(&value).unwrap());
    let pin_like_json = collection::vec(
        (
            small_json_string(),
            small_json_string(),
            small_json_value(),
            any::<u64>(),
        ),
        0..=4,
    )
    .prop_map(|entries| {
        let pins = entries
            .into_iter()
            .map(|(name, text, plan, created_lsn)| {
                json!({
                    "name": name,
                    "text": text,
                    "plan": plan,
                    "created_lsn": created_lsn
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_vec(&pins).unwrap()
    });

    prop_oneof![
        4 => collection::vec(any::<u8>(), 0..=768),
        3 => arbitrary_json,
        3 => pin_like_json,
    ]
    .boxed()
}

fn with_ontology(ontology: Value) -> Value {
    let mut catalog = base_catalog_value();
    catalog["ontology"] = ontology;
    catalog
}

fn interesting_catalog_values() -> Vec<Value> {
    let mut valid_ontology = base_catalog_value();
    valid_ontology["ontology"] = json!({
        "interfaces": [{
            "name": "Nameable", "columns": [{"name": "name", "type": "String"}]
        }],
        "node_classes": [{
            "table": "pErSoN", "label": "NAME", "summary": ["age"],
            "implements": ["nameable"]
        }],
        "rel_classes": [{"table": "KNOWS", "verb": "knows"}]
    });

    vec![
        base_catalog_value(),
        valid_ontology,
        json!({
            "node_tables": [
                {"name": "Person", "columns": [{"name": "id", "ty": "Int64", "primary_key": true}]},
                {"name": "person", "columns": [{"name": "id", "ty": "Int64", "primary_key": true}]}
            ],
            "rel_tables": [], "storage": {}
        }),
        json!({
            "node_tables": [{"name": "Person", "columns": [
                {"name": "Name", "ty": "Int64", "primary_key": true},
                {"name": "name", "ty": "String", "primary_key": false}
            ]}],
            "rel_tables": [], "storage": {}
        }),
        json!({
            "node_tables": [{"name": "Person", "columns": [
                {"name": "id", "ty": "Int64", "primary_key": true}
            ]}],
            "rel_tables": [],
            "storage": {"Person": {"groups": []}, "person": {"groups": []}}
        }),
        {
            let mut value = base_catalog_value();
            value["indexes"] = json!([{
                "name": "embedding", "kind": "hnsw", "table": "Person",
                "column": "embedding", "root": 7, "unknown": true
            }]);
            value
        },
        with_ontology(json!({})),
        with_ontology(json!({"node_classes": [{"table": "Ghost"}]})),
        with_ontology(json!({"interfaces": [{
            "name": "Nameable", "columns": [{"name": "name", "type": "String", "extra": 1}]
        }]})),
    ]
}

fn interesting_catalog_payloads() -> Vec<Vec<u8>> {
    interesting_catalog_values()
        .into_iter()
        .map(|value| serde_json::to_vec(&value).unwrap())
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_superblocks_never_panic(page in arbitrary_superblock_page()) {
        let _outcome = Superblock::decode(&page);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED ^ 0x5a4f_4e45_464c_4147),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_unknown_node_directory_flags_are_corrupt_without_panicking(flags in any::<u32>()) {
        let mut page = valid_node_directory_page();
        let flags = flags | (1 << 1);
        page[NODE_DIRECTORY_FLAGS_OFFSET..NODE_DIRECTORY_FLAGS_OFFSET + 4]
            .copy_from_slice(&flags.to_le_bytes());
        let outcome = std::panic::catch_unwind(|| decode_node_directory_page(&page));
        prop_assert!(outcome.is_ok(), "node directory flags decoder panicked");
        let is_corrupt = matches!(outcome.unwrap(), Err(DevonError::Corrupt { .. }));
        prop_assert!(is_corrupt);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED ^ 0x5a4f_4e45_4652_414d),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_node_section_frames_are_corrupt_without_panicking(
        section_len in any::<u32>(),
        noise in collection::vec(any::<u8>(), 0..=96),
    ) {
        let mut page = valid_node_directory_page();
        page[NODE_SECTION_START..NODE_SECTION_START + 4]
            .copy_from_slice(&section_len.to_le_bytes());
        let noise_end = (NODE_SECTION_PAYLOAD_START + noise.len()).min(page.len());
        page[NODE_SECTION_PAYLOAD_START..noise_end]
            .copy_from_slice(&noise[..noise_end - NODE_SECTION_PAYLOAD_START]);
        if let Some(section_end) = NODE_SECTION_PAYLOAD_START.checked_add(section_len as usize)
            && section_end <= page.len()
        {
            let wrong_checksum = crc32c(&page[NODE_SECTION_PAYLOAD_START..section_end]) ^ 1;
            page[NODE_SECTION_START + 4..NODE_SECTION_START + 8]
                .copy_from_slice(&wrong_checksum.to_le_bytes());
        }
        let outcome = std::panic::catch_unwind(|| decode_node_directory_page(&page));
        prop_assert!(outcome.is_ok(), "node section decoder panicked");
        let is_corrupt = matches!(outcome.unwrap(), Err(DevonError::Corrupt { .. }));
        prop_assert!(is_corrupt);
    }
}

#[test]
fn every_low_feature_flag_combination_has_the_documented_outcome() {
    for flags in 0_u64..16 {
        let expected_read_only = flags & READ_SAFE_FLAG_MASK & !SUPPORTED_FLAG_MASK != 0;
        let superblock = Superblock {
            format_version: SUPPORTED_FORMAT_VERSION,
            min_reader_version: SUPPORTED_FORMAT_VERSION,
            feature_flags: flags,
            page_size: PAGE_SIZE as u32,
            db_id: DB_ID,
            checkpoint_lsn: 11,
            catalog_root: 23,
        };
        let mut page = [0_u8; 64];
        superblock.encode(&mut page).unwrap();
        let decoded = Superblock::decode(&page);

        if flags & !READ_SAFE_FLAG_MASK != 0 {
            assert!(
                matches!(decoded, Err(DevonError::VersionMismatch { .. })),
                "flags {flags:#06b} should be a version mismatch: {decoded:?}"
            );
        } else {
            let decoded = decoded.unwrap();
            assert_eq!(decoded.feature_flags, flags);
            assert_eq!(
                decoded.requires_read_only(),
                expected_read_only,
                "wrong read-only result for flags {flags:#06b}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 192,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED ^ 0x4341_5441_4c4f_4700),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_catalog_payloads_under_valid_headers_never_panic(
        payload in arbitrary_catalog_payload()
    ) {
        let page = catalog_page(&payload);
        let _outcome = decode_catalog_page(&page);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 192,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED ^ 0x5049_4e53_4a53_4f4e),
        ..ProptestConfig::default()
    })]

    #[test]
    fn pins_arbitrary_bytes_and_json_never_panic(section in arbitrary_pins_section()) {
        let payload = pins_catalog_payload(&section);
        let _outcome = decode_catalog_page(&catalog_page(&payload));
    }
}

#[test]
fn pins_unknown_fields_are_corrupt() {
    let context = pins_corrupt_context(json!([{
        "name": "friends",
        "text": "who does ada know",
        "plan": {"v": 0, "plan": {}},
        "created_lsn": 7,
        "surprise": true
    }]));

    assert!(context.contains("unknown field `surprise`"), "{context}");
}

#[test]
fn pins_missing_required_fields_are_corrupt() {
    let cases = [
        (
            "name",
            json!({
                "text": "query", "plan": {"v": 0, "plan": {}}, "created_lsn": 1
            }),
        ),
        (
            "text",
            json!({
                "name": "pin", "plan": {"v": 0, "plan": {}}, "created_lsn": 1
            }),
        ),
        (
            "plan",
            json!({"name": "pin", "text": "query", "created_lsn": 1}),
        ),
        (
            "created_lsn",
            json!({
                "name": "pin", "text": "query", "plan": {"v": 0, "plan": {}}
            }),
        ),
    ];

    for (field, entry) in cases {
        let context = pins_corrupt_context(json!([entry]));
        assert!(
            context.contains(&format!("missing field `{field}`")),
            "missing {field}: {context}"
        );
    }
}

#[test]
fn pins_wrong_json_types_are_corrupt() {
    let cases = [
        ("pins object", json!({})),
        ("non-object entry", json!([true])),
        (
            "numeric name",
            json!([{
                "name": 7, "text": "query", "plan": {"v": 0, "plan": {}},
                "created_lsn": 1
            }]),
        ),
        (
            "boolean text",
            json!([{
                "name": "pin", "text": false, "plan": {"v": 0, "plan": {}},
                "created_lsn": 1
            }]),
        ),
        (
            "string LSN",
            json!([{
                "name": "pin", "text": "query", "plan": {"v": 0, "plan": {}},
                "created_lsn": "1"
            }]),
        ),
    ];

    for (label, pins) in cases {
        let context = pins_corrupt_context(pins);
        assert!(
            context.contains("catalog payload is not valid JSON"),
            "{label}: {context}"
        );
    }
}

#[test]
fn pins_duplicate_names_are_corrupt() {
    let cases = [
        ("exact", "Friends", "Friends"),
        ("ASCII fold-equal", "Friends", "fRIENDS"),
    ];

    for (label, first, second) in cases {
        let context = pins_corrupt_context(json!([
            {
                "name": first, "text": "first", "plan": {"v": 0, "plan": {}},
                "created_lsn": 1
            },
            {
                "name": second, "text": "second", "plan": {"v": 0, "plan": {}},
                "created_lsn": 2
            }
        ]));
        assert!(
            context.contains("fold-equal duplicate"),
            "{label}: {context}"
        );
    }
}

#[test]
fn pins_malformed_stored_plan_payloads_are_corrupt() {
    let cases = [
        ("non-object envelope", json!([]), "not a JSON object"),
        ("missing version", json!({"plan": {}}), "no numeric `v`"),
        (
            "non-numeric version",
            json!({"v": "zero", "plan": {}}),
            "no numeric `v`",
        ),
        ("missing plan", json!({"v": 0}), "no object `plan`"),
        (
            "non-object plan",
            json!({"v": 0, "plan": []}),
            "no object `plan`",
        ),
    ];

    for (label, plan, expected) in cases {
        let context = pins_corrupt_context(json!([{
            "name": "pin", "text": "query", "plan": plan, "created_lsn": 1
        }]));
        assert!(context.contains(expected), "{label}: {context}");
    }
}

#[test]
fn pins_truncated_sections_are_corrupt() {
    let payload = serde_json::to_vec(&pins_catalog_value(json!([{
        "name": "friends",
        "text": "who does ada know",
        "plan": {
            "v": 0,
            "plan": {"op": "ScanNodes", "table": "Person", "binding": "person"}
        },
        "created_lsn": 7
    }])))
    .unwrap();
    let pins_start = payload
        .windows(b"\"pins\"".len())
        .position(|window| window == b"\"pins\"")
        .unwrap();

    for cut in pins_start..payload.len() {
        let context = corrupt_context(decode_catalog_page(&catalog_page(&payload[..cut])));
        assert!(
            context.contains("catalog payload is not valid JSON"),
            "cut at {cut}: {context}"
        );
    }
}

#[test]
fn pins_oversized_entries_have_documented_error_classes() {
    let plan_json = r#"{"v":0,"plan":{}}"#;
    // Over one page but under the multipage directory bound: since the
    // multi-page catalog (feature bit 9) this SAVES, spilling into
    // continuation pages, and round-trips.
    let huge_text = "p".repeat(MAX_CATALOG_PAYLOAD);
    let entry =
        PinEntry::from_plan_json("oversized".to_owned(), huge_text.clone(), plan_json, 1).unwrap();
    let mut catalog = Catalog::default();
    catalog.pin(entry).unwrap();
    let directory = tempdir().unwrap();
    let pager = Pager::create(
        directory.path().join("oversized-pin.devondb"),
        PAGE_SIZE as u32,
        DB_ID,
    )
    .unwrap();

    catalog.save(&pager, 1).unwrap();
    assert_eq!(Catalog::load(&pager).unwrap(), catalog);

    // Past the directory page's id capacity the save is refused before
    // any page allocation — the surviving save-side error class.
    let over_directory_bound = "p".repeat((PAGE_SIZE - CATALOG_HEADER_LEN) / 8 * PAGE_SIZE + 1);
    let unbounded = PinEntry::from_plan_json(
        "over-directory-bound".to_owned(),
        over_directory_bound,
        plan_json,
        2,
    )
    .unwrap();
    let mut oversized = Catalog::default();
    oversized.pin(unbounded).unwrap();
    let save_error = oversized.save(&pager, 2).unwrap_err();
    assert!(
        matches!(
            save_error,
            DevonError::InvalidArgument { ref context }
                if context.contains("catalog payload") && context.contains("continuation pages")
        ),
        "{save_error}"
    );

    let oversized_section = serde_json::to_vec(&json!([{
        "name": "oversized",
        "text": huge_text,
        "plan": {"v": 0, "plan": {}},
        "created_lsn": 1
    }]))
    .unwrap();
    let prefix = br#"{"node_tables":[],"rel_tables":[],"storage":{},"pins":"#;
    let payload_len = prefix.len() + oversized_section.len() + 1;
    assert!(payload_len > MAX_CATALOG_PAYLOAD);
    let mut page = vec![0_u8; PAGE_SIZE];
    page[..4].copy_from_slice(&(payload_len as u32).to_le_bytes());
    page[CATALOG_HEADER_LEN..CATALOG_HEADER_LEN + prefix.len()].copy_from_slice(prefix);

    assert_eq!(
        corrupt_context(decode_catalog_page(&page)),
        "catalog payload length exceeds page capacity"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED ^ 0x4845_4144_4552_0000),
        ..ProptestConfig::default()
    })]

    #[test]
    fn catalog_header_math_reports_exact_corruption(
        (page, expected) in catalog_header_mutation()
    ) {
        prop_assert_eq!(corrupt_context(decode_catalog_page(&page)), expected);
    }
}

#[test]
fn ontology_validation_covers_every_documented_corruption_class() {
    let cases = vec![
        (
            "empty ontology",
            with_ontology(json!({})),
            "catalog ontology section is present but declares nothing",
        ),
        (
            "fold-equal interfaces",
            with_ontology(json!({"interfaces": [
                {"name": "Named", "columns": [{"name": "name", "type": "String"}]},
                {"name": "named", "columns": [{"name": "name", "type": "String"}]}
            ]})),
            "fold-equal duplicate",
        ),
        (
            "empty interface name",
            with_ontology(json!({"interfaces": [{
                "name": "", "columns": [{"name": "name", "type": "String"}]
            }]})),
            "interface has an empty name",
        ),
        (
            "empty interface columns",
            with_ontology(json!({"interfaces": [{"name": "Named", "columns": []}]})),
            "declares no columns",
        ),
        (
            "empty interface column name",
            with_ontology(json!({"interfaces": [{
                "name": "Named", "columns": [{"name": "", "type": "String"}]
            }]})),
            "column with an empty name",
        ),
        (
            "fold-equal interface columns",
            with_ontology(json!({"interfaces": [{"name": "Named", "columns": [
                {"name": "Name", "type": "String"},
                {"name": "name", "type": "String"}
            ]}]})),
            "fold-equal duplicate",
        ),
        (
            "fold-equal node classes",
            with_ontology(json!({"node_classes": [
                {"table": "Person"}, {"table": "person"}
            ]})),
            "fold-equal duplicate",
        ),
        (
            "unknown node table",
            with_ontology(json!({"node_classes": [{"table": "Ghost"}]})),
            "unknown node table",
        ),
        (
            "unknown label column",
            with_ontology(json!({"node_classes": [{
                "table": "Person", "label": "ghost"
            }]})),
            "unknown column",
        ),
        (
            "unknown summary column",
            with_ontology(json!({"node_classes": [{
                "table": "Person", "summary": ["ghost"]
            }]})),
            "unknown column",
        ),
        (
            "unknown implemented interface",
            with_ontology(json!({"node_classes": [{
                "table": "Person", "implements": ["Ghostly"]
            }]})),
            "implements unknown interface",
        ),
        (
            "interface type mismatch",
            with_ontology(json!({
                "interfaces": [{
                    "name": "Aged", "columns": [{"name": "age", "type": "String"}]
                }],
                "node_classes": [{"table": "Person", "implements": ["Aged"]}]
            })),
            "does not satisfy interface",
        ),
        (
            "fold-equal relationship classes",
            with_ontology(json!({"rel_classes": [
                {"table": "Knows"}, {"table": "knows"}
            ]})),
            "fold-equal duplicate",
        ),
        (
            "unknown relationship table",
            with_ontology(json!({"rel_classes": [{"table": "Ghost"}]})),
            "unknown relationship table",
        ),
        (
            "unknown ontology field",
            with_ontology(json!({"surprise": true})),
            "unknown field",
        ),
        (
            "unknown interface field",
            with_ontology(json!({"interfaces": [{
                "name": "Named", "columns": [{"name": "name", "type": "String"}],
                "surprise": true
            }]})),
            "unknown field",
        ),
        (
            "unknown interface-column field",
            with_ontology(json!({"interfaces": [{
                "name": "Named", "columns": [{
                    "name": "name", "type": "String", "surprise": true
                }]
            }]})),
            "unknown field",
        ),
        (
            "unknown node-class field",
            with_ontology(json!({"node_classes": [{
                "table": "Person", "surprise": true
            }]})),
            "unknown field",
        ),
        (
            "unknown relationship-class field",
            with_ontology(json!({"rel_classes": [{
                "table": "Knows", "surprise": true
            }]})),
            "unknown field",
        ),
    ];

    for (label, value, expected) in cases {
        let payload = serde_json::to_vec(&value).unwrap();
        let context = corrupt_context(decode_catalog_page(&catalog_page(&payload)));
        assert!(
            context.contains(expected),
            "{label}: expected `{expected}` in `{context}`"
        );
    }
}

#[test]
fn valid_ontology_and_non_ontology_collision_fixtures_are_exercised() {
    let values = interesting_catalog_values();
    assert!(decode_catalog_page(&catalog_page(&serde_json::to_vec(&values[0]).unwrap())).is_ok());
    assert!(decode_catalog_page(&catalog_page(&serde_json::to_vec(&values[1]).unwrap())).is_ok());
    for value in &values[2..] {
        assert!(
            matches!(
                decode_catalog_page(&catalog_page(&serde_json::to_vec(value).unwrap())),
                Err(DevonError::Corrupt { .. })
            ),
            "accepted corrupt catalog fixture: {value}"
        );
    }
}

#[derive(Clone, Debug)]
struct ValidCatalogCase {
    company: bool,
    relationship: bool,
    person_storage: Option<Vec<u64>>,
    company_storage: Option<Vec<u64>>,
    rel_storage: Option<(Vec<u64>, Vec<u64>)>,
    index_mask: u8,
    ontology_mode: u8,
    vector_dim: u32,
    string_primary_key: bool,
    metadata: u8,
}

fn valid_catalog_case() -> BoxedStrategy<ValidCatalogCase> {
    (
        any::<bool>(),
        any::<bool>(),
        prop::option::of(collection::vec(2_u64..=200, 0..=4)),
        prop::option::of(collection::vec(2_u64..=200, 0..=4)),
        prop::option::of((
            collection::vec(0_u64..=200, 0..=4),
            collection::vec(0_u64..=200, 0..=4),
        )),
        0_u8..=3,
        0_u8..=4,
        1_u32..=8,
        any::<bool>(),
        0_u8..=3,
    )
        .prop_map(
            |(
                company,
                relationship,
                person_storage,
                company_storage,
                rel_storage,
                index_mask,
                ontology_mode,
                vector_dim,
                string_primary_key,
                metadata,
            )| ValidCatalogCase {
                company,
                relationship,
                person_storage,
                company_storage,
                rel_storage,
                index_mask,
                ontology_mode,
                vector_dim,
                string_primary_key,
                metadata,
            },
        )
        .boxed()
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn node_schema(name: &str, dim: u32, string_primary_key: bool) -> NodeTableSchema {
    let primary_key_type = if string_primary_key {
        LogicalType::String
    } else {
        LogicalType::Int64
    };
    NodeTableSchema::new(
        name.to_owned(),
        vec![
            column("id", primary_key_type, true),
            column("name", LogicalType::String, false),
            column("age", LogicalType::Int64, false),
            column("embedding", LogicalType::Vector { dim }, false),
        ],
    )
    .unwrap()
}

fn build_valid_catalog(case: &ValidCatalogCase) -> Catalog {
    let mut catalog = Catalog::default();
    catalog
        .add_node_table(node_schema(
            "Person",
            case.vector_dim,
            case.string_primary_key,
        ))
        .unwrap();
    if case.company {
        catalog
            .add_node_table(node_schema("Company", case.vector_dim, false))
            .unwrap();
    }
    if case.relationship {
        let to = if case.company { "Company" } else { "Person" };
        catalog
            .add_rel_table(
                RelTableSchema::new(
                    "Knows".to_owned(),
                    "Person".to_owned(),
                    to.to_owned(),
                    vec![column("since", LogicalType::Int64, false)],
                )
                .unwrap(),
            )
            .unwrap();
    }
    if let Some(groups) = &case.person_storage {
        catalog
            .set_table_storage(
                "pErSoN",
                TableStorage {
                    groups: groups.clone(),
                },
            )
            .unwrap();
    }
    if case.company
        && let Some(groups) = &case.company_storage
    {
        catalog
            .set_table_storage(
                "COMPANY",
                TableStorage {
                    groups: groups.clone(),
                },
            )
            .unwrap();
    }
    if case.relationship
        && let Some((fwd, bwd)) = &case.rel_storage
    {
        catalog
            .set_rel_storage(
                "knows",
                RelStorage {
                    fwd: fwd.clone(),
                    bwd: bwd.clone(),
                },
            )
            .unwrap();
    }
    add_valid_indexes(&mut catalog, case);
    add_valid_ontology(&mut catalog, case);
    catalog
}

fn add_valid_indexes(catalog: &mut Catalog, case: &ValidCatalogCase) {
    if case.index_mask & 1 != 0 {
        catalog
            .add_index(IndexEntry {
                name: "person_embedding".to_owned(),
                kind: IndexKind::Hnsw,
                table: "PERSON".to_owned(),
                column: "EMBEDDING".to_owned(),
                root: 301,
            })
            .unwrap();
    }
    if case.company && case.index_mask & 2 != 0 {
        catalog
            .add_index(IndexEntry {
                name: "company_embedding".to_owned(),
                kind: IndexKind::Hnsw,
                table: "Company".to_owned(),
                column: "embedding".to_owned(),
                root: 302,
            })
            .unwrap();
    }
}

fn add_valid_ontology(catalog: &mut Catalog, case: &ValidCatalogCase) {
    let has_interface = matches!(case.ontology_mode, 1 | 3 | 4);
    if has_interface {
        catalog
            .declare_interface(InterfaceEntry {
                name: "Nameable".to_owned(),
                columns: vec![InterfaceColumn {
                    name: "name".to_owned(),
                    ty: LogicalType::String,
                }],
            })
            .unwrap();
    }
    if case.ontology_mode >= 2 {
        let display = [None, Some("Person"), Some("Human"), Some("🦀 Person")]
            [case.metadata as usize]
            .map(str::to_owned);
        catalog
            .declare_node_class(NodeClassEntry {
                table: "PERSON".to_owned(),
                display,
                plural: Some("people".to_owned()),
                label: Some("NAME".to_owned()),
                summary: vec!["name".to_owned(), "age".to_owned()],
                color: Some("#7aa2ff".to_owned()),
                description: Some("generated class".to_owned()),
                implements: if has_interface {
                    vec!["nameable".to_owned()]
                } else {
                    Vec::new()
                },
            })
            .unwrap();
    }
    if case.ontology_mode == 4 && case.company {
        catalog
            .declare_node_class(NodeClassEntry {
                table: "Company".to_owned(),
                label: Some("name".to_owned()),
                implements: vec!["Nameable".to_owned()],
                ..NodeClassEntry::default()
            })
            .unwrap();
    }
    if case.ontology_mode == 4 && case.relationship {
        catalog
            .declare_rel_class(RelClassEntry {
                table: "KNOWS".to_owned(),
                verb: Some("knows".to_owned()),
                inverse: Some("is known by".to_owned()),
            })
            .unwrap();
    }
}

#[derive(Clone, Debug)]
struct ValidPinCase {
    name_suffix: String,
    text: String,
    version: u64,
    plan_shape: u8,
    opaque: Value,
    created_lsn: u64,
}

fn valid_pin_cases() -> BoxedStrategy<Vec<ValidPinCase>> {
    collection::vec(
        (
            small_json_string(),
            small_json_string(),
            0_u64..=4,
            0_u8..=3,
            small_json_value(),
            1_u64..=64,
        )
            .prop_map(
                |(name_suffix, text, version, plan_shape, opaque, created_lsn)| ValidPinCase {
                    name_suffix,
                    text,
                    version,
                    plan_shape,
                    opaque,
                    created_lsn,
                },
            ),
        1..=4,
    )
    .boxed()
}

fn stored_plan_for_case(case: &ValidPinCase) -> Value {
    let plan = match case.plan_shape {
        0 => json!({}),
        1 => json!({
            "op": "ScanNodes", "table": "Person", "binding": "person",
            "opaque": case.opaque
        }),
        2 => json!({"op": "PinnedOpaque", "payload": case.opaque}),
        _ => json!({
            "op": "Project",
            "exprs": [{"expr": {"lit": case.opaque}, "as": "value"}],
            "input": {}
        }),
    };
    json!({"v": case.version, "plan": plan})
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED ^ 0x524f_554e_4454_5249),
        ..ProptestConfig::default()
    })]

    #[test]
    fn public_catalog_mutations_round_trip_through_real_pager(case in valid_catalog_case()) {
        let expected = build_valid_catalog(&case);
        let directory = tempdir().unwrap();
        let path = directory.path().join("round-trip.devondb");
        let pager = Pager::create(&path, PAGE_SIZE as u32, DB_ID).unwrap();
        expected.save(&pager, 1).unwrap();
        drop(pager);

        let reopened = Pager::open(&path).unwrap();
        let actual = Catalog::load(&reopened).unwrap();
        prop_assert_eq!(actual, expected);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED ^ 0x5049_4e53_5254_5249),
        ..ProptestConfig::default()
    })]

    #[test]
    fn pins_public_mutations_round_trip_stored_plan_bytes(cases in valid_pin_cases()) {
        let mut expected = Catalog::default();
        let mut expected_plan_bytes = Vec::with_capacity(cases.len());
        for (index, case) in cases.iter().enumerate() {
            let name = format!("pin-{index}-{}", case.name_suffix);
            let plan_bytes = serde_json::to_vec(&stored_plan_for_case(case)).unwrap();
            let plan_json = String::from_utf8(plan_bytes.clone()).unwrap();
            let entry = PinEntry::from_plan_json(
                name,
                case.text.clone(),
                &plan_json,
                case.created_lsn,
            )
            .unwrap();
            let entry_plan_bytes = entry.plan_json().unwrap().into_bytes();
            prop_assert_eq!(entry_plan_bytes.as_slice(), plan_bytes.as_slice());
            expected.pin(entry).unwrap();
            expected_plan_bytes.push(plan_bytes);
        }

        let directory = tempdir().unwrap();
        let path = directory.path().join("pins-round-trip.devondb");
        let pager = Pager::create(&path, PAGE_SIZE as u32, DB_ID).unwrap();
        expected.save(&pager, 100).unwrap();
        drop(pager);

        let reopened = Pager::open(&path).unwrap();
        let actual = Catalog::load(&reopened).unwrap();
        prop_assert_eq!(&actual, &expected);
        prop_assert_eq!(actual.pins().len(), expected_plan_bytes.len());
        for (pin, expected_bytes) in actual.pins().iter().zip(&expected_plan_bytes) {
            prop_assert_eq!(pin.plan_json().unwrap().into_bytes(), expected_bytes.clone());
        }
    }
}
