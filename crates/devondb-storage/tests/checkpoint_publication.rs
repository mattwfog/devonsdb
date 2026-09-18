//! Regression coverage for the checkpoint publication window: adaptive
//! node-group writes may declare `COLUMN_ENCODINGS` before catalog save
//! publishes the governing superblock feature bit.

use std::path::PathBuf;

use devondb_storage::{
    catalog::Catalog, node_group::group_page_inventory, node_table::NodeTable, pager::Pager,
    superblock::COLUMN_ENCODINGS_FLAG, wal::WalWriter,
};
use devondb_types::{
    DevonResult,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
    value::Value,
};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"checkpoint-pub!!";
const TABLE: &str = "Person";
const COLUMN_ENCODINGS_DIRECTORY_FLAG: u32 = 1 << 1;

struct Fixture {
    directory: TempDir,
    path: PathBuf,
    pager: Pager,
    catalog: Catalog,
    table: NodeTable,
    wal: WalWriter,
    publish_lsn: u64,
}

impl Fixture {
    fn new(columns: Vec<Column>) -> Self {
        let directory = tempdir().unwrap();
        let path = directory.path().join("checkpoint.devondb");
        let wal_path = directory.path().join("checkpoint.devondb-wal");
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let schema = NodeTableSchema::new(TABLE.to_owned(), columns).unwrap();
        let table = NodeTable::new(schema.clone());
        let mut catalog = Catalog::default();
        catalog.add_node_table(schema).unwrap();
        catalog.save(&pager, 1).unwrap();
        let wal = WalWriter::open(wal_path, 2).unwrap();
        Self {
            directory,
            path,
            pager,
            catalog,
            table,
            wal,
            publish_lsn: 2,
        }
    }

    fn insert(&mut self, row: Vec<Value>) {
        self.table.insert(&mut self.wal, row).unwrap();
    }

    fn recover_row(&mut self, row: Vec<Value>) {
        self.table.recover_row(row).unwrap();
    }

    fn checkpoint(&mut self) -> DevonResult<()> {
        self.table.checkpoint(&self.pager, &mut self.catalog)?;

        // Catalog publication can validate a freshly written directory
        // before its superblock flip (superseded-page inventory is one such
        // reader). Exercise that window rather than only the post-save path.
        let schema = self.catalog.node_table(TABLE).unwrap();
        let types: Vec<LogicalType> = schema.columns().iter().map(|column| column.ty).collect();
        if let Some(storage) = self.catalog.table_storage(TABLE) {
            for group in &storage.groups {
                group_page_inventory(&self.pager, *group, &types)?;
            }
        }

        self.catalog.save(&self.pager, self.publish_lsn)?;
        self.publish_lsn += 1;
        Ok(())
    }

    fn reachable_group_ids(&self) -> &[u64] {
        self.catalog
            .table_storage(TABLE)
            .map_or(&[], |storage| storage.groups.as_slice())
    }

    fn reachable_non_plain_group(&self) -> bool {
        self.reachable_group_ids()
            .iter()
            .any(|group| directory_declares_column_encodings(&self.pager, *group))
    }

    fn assert_feature_matches_reachable_groups(&self) {
        assert_eq!(
            self.pager.superblock().feature_flags & COLUMN_ENCODINGS_FLAG != 0,
            self.reachable_non_plain_group(),
            "feature bit 13 must describe the published reachable groups"
        );
    }

    fn reopen_rows(self) -> Vec<Vec<Value>> {
        let Self {
            directory,
            path,
            pager,
            catalog: _,
            table: _,
            wal,
            publish_lsn: _,
        } = self;
        drop(wal);
        drop(pager);
        let pager = Pager::open(path).unwrap();
        let catalog = Catalog::load(&pager).unwrap();
        let schema = catalog.node_table(TABLE).unwrap().clone();
        let rows = NodeTable::new(schema).scan(&pager, &catalog).unwrap();
        drop(pager);
        drop(directory);
        rows
    }
}

fn person_columns() -> Vec<Column> {
    vec![
        Column {
            name: "id".to_owned(),
            ty: LogicalType::String,
            primary_key: true,
        },
        Column {
            name: "name".to_owned(),
            ty: LogicalType::String,
            primary_key: false,
        },
    ]
}

fn string_primary_key_column() -> Vec<Column> {
    vec![Column {
        name: "id".to_owned(),
        ty: LogicalType::String,
        primary_key: true,
    }]
}

fn person(id: &str, name: &str) -> Vec<Value> {
    vec![Value::String(id.to_owned()), Value::String(name.to_owned())]
}

fn directory_declares_column_encodings(pager: &Pager, directory_page: u64) -> bool {
    let page = pager.read_page(directory_page).unwrap();
    let flags = u32::from_le_bytes(page[12..16].try_into().unwrap());
    flags & COLUMN_ENCODINGS_DIRECTORY_FLAG != 0
}

#[test]
fn create_insert_checkpoint_reopens_exact_values() {
    let expected = vec![person("a", "x")];
    let mut fixture = Fixture::new(person_columns());
    fixture.insert(expected[0].clone());

    fixture.checkpoint().unwrap();

    fixture.assert_feature_matches_reachable_groups();
    assert_eq!(fixture.reopen_rows(), expected);
}

#[test]
fn repeated_checkpoint_uses_the_already_published_feature_bit() {
    let mut fixture = Fixture::new(person_columns());
    fixture.insert(person("a", "same"));
    fixture.checkpoint().unwrap();
    assert_ne!(
        fixture.pager.superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        0
    );

    fixture.insert(person("b", "same"));
    fixture.checkpoint().unwrap();

    fixture.assert_feature_matches_reachable_groups();
    assert_eq!(
        fixture.reopen_rows(),
        [person("a", "same"), person("b", "same")]
    );
}

#[test]
fn adaptive_string_encoding_survives_two_checkpoint_cycles() {
    let mut fixture = Fixture::new(person_columns());
    let mut expected = Vec::new();
    for row in 0..512 {
        let value = person(
            &format!("id-{row:04}"),
            &format!("https://edge.example.com/sensors/{row:04}/temperature"),
        );
        fixture.recover_row(value.clone());
        expected.push(value);
    }

    fixture.checkpoint().unwrap();
    assert!(
        fixture.reachable_non_plain_group(),
        "the string-heavy dataset must select a non-plain encoding"
    );

    let extra = person(
        "id-extra",
        "https://edge.example.com/sensors/extra/temperature",
    );
    fixture.insert(extra.clone());
    expected.push(extra);
    fixture.checkpoint().unwrap();

    fixture.assert_feature_matches_reachable_groups();
    assert_eq!(fixture.reopen_rows(), expected);
}

#[test]
fn feature_bit_is_published_iff_a_reachable_group_is_non_plain() {
    let mut encoded = Fixture::new(person_columns());
    encoded.insert(person("a", "same"));
    encoded.checkpoint().unwrap();
    assert!(encoded.reachable_non_plain_group());
    encoded.assert_feature_matches_reachable_groups();

    let mut plain = Fixture::new(string_primary_key_column());
    let mut state = 0x243F_6A88_85A3_08D3_u64;
    for _ in 0..2048 {
        let mut text = String::with_capacity(32);
        for _ in 0..32 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            text.push(char::from((state >> 33) as u8 % 94 + 33));
        }
        plain.recover_row(vec![Value::String(text)]);
    }
    plain.checkpoint().unwrap();
    assert!(
        !plain.reachable_non_plain_group(),
        "the high-entropy string group must remain plain"
    );
    plain.assert_feature_matches_reachable_groups();
}
