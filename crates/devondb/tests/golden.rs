use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{
    Database, DevonError, Plan, Statement, StatementEnvelope,
    introspect::{
        ClassesSummary, InterfaceColumnSummary, InterfaceSummary, NodeClassSummary, RelClassSummary,
    },
    text::{
        parser::{Parsed, parse},
        printer::print_statement,
    },
};
use devondb_plan::statement::RelRow;
use devondb_storage::{
    catalog::{Catalog, TableStorage},
    node_group::{NodeGroup, ZoneMapValue},
    pager::Pager,
    superblock::{
        FREE_PAGES_FLAG, MULTIPROCESS_COORDINATION_FLAG, SCALAR_TYPES_V2_FLAG, ZONE_MAPS_FLAG,
    },
};
use devondb_types::{
    Decimal128, GeoPoint, logical_type::LogicalType, schema::Column, value::Value,
};

const M1_ANCHOR_FILE: &str = "m1-person.devondb";
const M2_ANCHOR_FILE: &str = "m2-social.devondb";
const M5_ANCHOR_FILE: &str = "m5-pins.devondb";
const ONTOLOGY_ANCHOR_FILE: &str = "ont-classes.devondb";
const GEO_ANCHOR_FILE: &str = "geo-places.devondb";
const SCALAR_V2_ANCHOR_FILE: &str = "scalar-v2.devondb";
const S1_STATS_ANCHOR_FILE: &str = "s1-stats.devondb";
const RELEASE_0_1_0_ANCHOR_FILE: &str = "release-0.1.0.devondb";
const MULTIPROCESS_ANCHOR_FILE: &str = "multiprocess.devondb";
const FREE_PAGES_ANCHOR_FILE: &str = "free-pages.devondb";
const M5_PIN_NAME: &str = "ada friends";
const M5_PIN_SOURCE_TEXT: &str = "who does Ada know";
const M5_PIN_PLAN_JSON: &str = r#"{"plan":{"exprs":[{"as":"name","expr":{"col":"friend.name"}}],"input":{"input":{"binding":"friend","direction":"out","from_binding":"person","input":{"input":{"binding":"person","op":"ScanNodes","table":"Person"},"op":"Filter","predicate":{"eq":[{"col":"person.name"},{"lit":"Ada"}]}},"op":"Expand","rel":"Knows"},"keys":[{"expr":{"col":"friend.name"},"order":"asc"}],"op":"Sort"},"op":"Project"},"v":0}"#;
const PAGE_SIZE: u32 = 4096;
const MIN_READER_VERSION: u32 = 999;
const SUPERBLOCK_HEADER_LEN: usize = 64;
const MIN_READER_VERSION_OFFSET: usize = 12;
const FEATURE_FLAGS_OFFSET: usize = 16;
const CHECKPOINT_LSN_OFFSET: usize = 44;
const CHECKSUM_OFFSET: usize = 60;
const GEO_COLUMNS_FEATURE: u64 = 1 << 4;
const DETERMINISM_DB_ID: [u8; 16] = *b"geo-anchor-test!";
const SCALAR_V2_DB_ID: [u8; 16] = *b"scalar-v2-anchor";
const S1_STATS_DB_ID: [u8; 16] = *b"s1-stats-anchor!";
const RELEASE_0_1_0_DB_ID: [u8; 16] = *b"release-0.1.0!!!";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct ManifestEntry {
    file: String,
    page_size: u32,
    description: String,
    tables: Vec<TableEntry>,
}

#[derive(Debug)]
struct TableEntry {
    name: String,
    row_count: usize,
}

struct ManifestParser<'a> {
    input: &'a [u8],
    cursor: usize,
}

impl<'a> ManifestParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            cursor: 0,
        }
    }

    fn parse(mut self) -> Result<Vec<ManifestEntry>, String> {
        self.expect_byte(b'[')?;
        let mut entries = Vec::new();
        if self.consume_byte(b']') {
            return self.finish(entries);
        }
        loop {
            entries.push(self.parse_entry()?);
            if self.consume_byte(b']') {
                return self.finish(entries);
            }
            self.expect_byte(b',')?;
        }
    }

    fn parse_entry(&mut self) -> Result<ManifestEntry, String> {
        self.expect_byte(b'{')?;
        self.expect_key("file")?;
        let file = self.parse_string()?;
        self.expect_byte(b',')?;
        self.expect_key("page_size")?;
        let page_size = self.parse_u32()?;
        self.expect_byte(b',')?;
        self.expect_key("description")?;
        let description = self.parse_string()?;
        self.expect_byte(b',')?;
        self.expect_key("tables")?;
        let tables = self.parse_tables()?;
        self.expect_byte(b'}')?;
        Ok(ManifestEntry {
            file,
            page_size,
            description,
            tables,
        })
    }

    fn parse_tables(&mut self) -> Result<Vec<TableEntry>, String> {
        self.expect_byte(b'[')?;
        let mut tables = Vec::new();
        if self.consume_byte(b']') {
            return Ok(tables);
        }
        loop {
            tables.push(self.parse_table()?);
            if self.consume_byte(b']') {
                return Ok(tables);
            }
            self.expect_byte(b',')?;
        }
    }

    fn parse_table(&mut self) -> Result<TableEntry, String> {
        self.expect_byte(b'{')?;
        self.expect_key("name")?;
        let name = self.parse_string()?;
        self.expect_byte(b',')?;
        self.expect_key("row_count")?;
        let row_count = self.parse_usize()?;
        self.expect_byte(b'}')?;
        Ok(TableEntry { name, row_count })
    }

    fn expect_key(&mut self, expected: &str) -> Result<(), String> {
        let actual = self.parse_string()?;
        if actual != expected {
            return Err(format!(
                "expected manifest key `{expected}`, found `{actual}`"
            ));
        }
        self.expect_byte(b':')
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect_byte(b'\"')?;
        let start = self.cursor;
        while let Some(byte) = self.input.get(self.cursor).copied() {
            match byte {
                b'\"' => {
                    let value = std::str::from_utf8(&self.input[start..self.cursor])
                        .map_err(|error| format!("manifest string is not UTF-8: {error}"))?;
                    self.cursor += 1;
                    return Ok(value.to_owned());
                }
                b'\\' => return Err("escaped manifest strings are not supported".to_owned()),
                0..=31 => return Err("manifest string contains a control byte".to_owned()),
                _ => self.cursor += 1,
            }
        }
        Err("unterminated manifest string".to_owned())
    }

    fn parse_u32(&mut self) -> Result<u32, String> {
        let number = self.parse_digits()?;
        number
            .parse()
            .map_err(|error| format!("manifest u32 `{number}` is invalid: {error}"))
    }

    fn parse_usize(&mut self) -> Result<usize, String> {
        let number = self.parse_digits()?;
        number
            .parse()
            .map_err(|error| format!("manifest usize `{number}` is invalid: {error}"))
    }

    fn parse_digits(&mut self) -> Result<&str, String> {
        self.skip_whitespace();
        let start = self.cursor;
        while self.input.get(self.cursor).is_some_and(u8::is_ascii_digit) {
            self.cursor += 1;
        }
        if self.cursor == start {
            return Err(format!("expected a number at byte {}", self.cursor));
        }
        std::str::from_utf8(&self.input[start..self.cursor])
            .map_err(|error| format!("manifest number is not UTF-8: {error}"))
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), String> {
        if self.consume_byte(expected) {
            return Ok(());
        }
        Err(format!(
            "expected `{}` at byte {}",
            char::from(expected),
            self.cursor
        ))
    }

    fn consume_byte(&mut self, expected: u8) -> bool {
        self.skip_whitespace();
        if self.input.get(self.cursor) != Some(&expected) {
            return false;
        }
        self.cursor += 1;
        true
    }

    fn skip_whitespace(&mut self) {
        while self
            .input
            .get(self.cursor)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.cursor += 1;
        }
    }

    fn finish<T>(&mut self, value: T) -> Result<T, String> {
        self.skip_whitespace();
        if self.cursor == self.input.len() {
            Ok(value)
        } else {
            Err(format!(
                "unexpected manifest content at byte {}",
                self.cursor
            ))
        }
    }
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-golden-test-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct WalCleanup {
    path: PathBuf,
}

impl WalCleanup {
    fn for_database(path: &Path) -> Self {
        Self {
            path: wal_path(path),
        }
    }
}

impl Drop for WalCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn golden_corpus_opens_and_matches_manifest() {
    let directory = corpus_directory();
    let manifest = load_manifest(&directory);
    let database_paths = corpus_database_paths(&directory);
    assert!(!database_paths.is_empty(), "golden corpus is empty");
    assert_eq!(manifest.len(), database_paths.len());

    for path in database_paths {
        verify_corpus_database(&path, &manifest);
    }
}

#[test]
fn golden_newer_min_reader_version_fails_cleanly() {
    let directory = TestDirectory::new();
    let source = corpus_directory().join(M1_ANCHOR_FILE);
    let copy = directory.join(M1_ANCHOR_FILE);
    fs::copy(source, &copy).unwrap();
    doctor_min_reader_version(&copy, MIN_READER_VERSION);

    let error = match Database::open(&copy) {
        Ok(_) => panic!("doctored database unexpectedly opened"),
        Err(error) => error,
    };
    let supported = match &error {
        DevonError::VersionMismatch {
            min_reader_version: MIN_READER_VERSION,
            supported,
            ..
        } => *supported,
        other => panic!("unexpected error: {other:?}"),
    };
    let message = error.to_string();
    assert!(message.contains("999"), "unexpected message: {message}");
    assert!(
        message.contains(&format!("supports version {supported}")),
        "message omits the reader's own version: {message}"
    );
    assert!(
        message.contains("upgrade devondb"),
        "unexpected message: {message}"
    );
}

#[test]
fn detach_delete_statement_text_and_tagged_json_are_golden() {
    let cases = vec![
        (Value::Null, "null", r#""Null""#),
        (Value::Bool(true), "true", r#"{"Bool":true}"#),
        (Value::Int64(-7), "-7", r#"{"Int64":-7}"#),
        (Value::Float64(1.25), "1.25", r#"{"Float64":1.25}"#),
        (
            Value::String("Ada".to_owned()),
            r#""Ada""#,
            r#"{"String":"Ada"}"#,
        ),
        (
            Value::Vector(vec![1.0, -2.5]),
            "[1, -2.5]",
            r#"{"Vector":[1.0,-2.5]}"#,
        ),
        (
            Value::GeoPoint(GeoPoint::new(45.5, -122.625).unwrap()),
            "geo(45.5, -122.625)",
            r#"{"GeoPoint":{"lat_deg":45.5,"lng_deg":-122.625}}"#,
        ),
        (
            Value::Timestamp(0),
            r#"timestamp("1970-01-01T00:00:00Z")"#,
            r#"{"Timestamp":0}"#,
        ),
        (
            Value::Bytes(vec![0, 0xff]),
            r#"bytes("00ff")"#,
            r#"{"Bytes":[0,255]}"#,
        ),
        (
            Value::Decimal(Decimal128::new(1234, 2).unwrap()),
            r#"decimal("12.34")"#,
            r#"{"Decimal":"12.34"}"#,
        ),
        (
            Value::Json(r#"{"a":1}"#.to_owned()),
            r#"json("{\"a\":1}")"#,
            r#"{"Json":"{\"a\":1}"}"#,
        ),
    ];

    for (key, literal, tagged_json) in cases {
        let envelope = StatementEnvelope {
            v: 0,
            stmt: Statement::DetachDeleteNode {
                table: "Odd Table".to_owned(),
                key_column: "primary key".to_owned(),
                key,
            },
        };
        let text = format!("detach delete from `Odd Table` where `primary key` = {literal}");
        assert_eq!(print_statement(&envelope).unwrap(), text);
        assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope.clone()));

        let expected_json = [
            r#"{"v":0,"stmt":{"stmt":"DetachDeleteNode","table":"Odd Table","key_column":"primary key","key":"#,
            tagged_json,
            "}}",
        ]
        .concat();
        assert_eq!(envelope.to_json().unwrap(), expected_json);
        assert_eq!(
            StatementEnvelope::from_json(&expected_json).unwrap(),
            envelope
        );
    }

    let contextual_identifier = StatementEnvelope {
        v: 0,
        stmt: Statement::DetachDeleteNode {
            table: "detach".to_owned(),
            key_column: "detach".to_owned(),
            key: Value::Int64(7),
        },
    };
    assert_eq!(
        print_statement(&contextual_identifier).unwrap(),
        "detach delete from detach where detach = 7"
    );
}

#[test]
#[ignore = "regenerates the committed M1 golden database"]
fn mint_m1_person_anchor() {
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(M1_ANCHOR_FILE);
    let wal = wal_path(&path);
    remove_file_if_present(&path);
    remove_file_if_present(&wal);

    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database.execute(&person_schema()).unwrap();
    database.execute(&person_rows()).unwrap();
    database.checkpoint().unwrap();
    assert_eq!(fs::metadata(&wal).unwrap().len(), 0);
    drop(database);
    fs::remove_file(wal).unwrap();
}

#[test]
#[ignore = "regenerates the committed M2 social-graph golden database"]
fn mint_m2_social_anchor() {
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(M2_ANCHOR_FILE);
    let wal = wal_path(&path);
    remove_file_if_present(&path);
    remove_file_if_present(&wal);

    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    database.execute(&social_person_schema()).unwrap();
    database.execute(&knows_schema()).unwrap();
    database.execute(&social_person_rows()).unwrap();
    database.execute(&knows_rows()).unwrap();
    database.checkpoint().unwrap();
    assert_eq!(fs::metadata(&wal).unwrap().len(), 0);
    drop(database);
    fs::remove_file(wal).unwrap();
}

#[test]
#[ignore = "regenerates the committed M5 pinned-plan golden database"]
fn mint_m5_pins_anchor() {
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(M5_ANCHOR_FILE);
    let wal = wal_path(&path);
    remove_file_if_present(&path);
    remove_file_if_present(&wal);

    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    seed_m5_anchor(&mut database);
    finish_anchor(database, &wal);
}

fn seed_m5_anchor(database: &mut Database) {
    execute_text(
        database,
        "create node table Person (id Int64 primary key, name String)",
    );
    execute_text(database, "create rel table Knows from Person to Person");
    execute_text(
        database,
        "insert into Person values (1, \"Ada\"), (2, \"Linus\"), (3, \"Grace\")",
    );
    execute_text(database, "insert rel into Knows values (1 -> 2), (1 -> 3)");
    database
        .pin(M5_PIN_NAME, M5_PIN_SOURCE_TEXT, &m5_pin_plan())
        .unwrap();
}

#[test]
#[ignore = "regenerates the committed ontology-class golden database"]
fn mint_ont_classes_anchor() {
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(ONTOLOGY_ANCHOR_FILE);
    let wal = wal_path(&path);
    remove_file_if_present(&path);
    remove_file_if_present(&wal);

    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    seed_ontology_anchor(&mut database);
    finish_anchor(database, &wal);
}

fn seed_ontology_anchor(database: &mut Database) {
    execute_text(
        database,
        "create node table Person (id Int64 primary key, name String, role String)",
    );
    execute_text(
        database,
        "create rel table Knows from Person to Person (since Int64)",
    );
    execute_text(database, "create interface Nameable (name String)");
    execute_text(
        database,
        concat!(
            "create class for Person (plural \"people\", summary (name, role), ",
            "color \"#7aa2ff\", description \"a human\", implements (Nameable))"
        ),
    );
    execute_text(
        database,
        "create class for Knows (verb \"knows\", inverse \"is known by\")",
    );
    execute_text(
        database,
        concat!(
            "insert into Person values (1, \"Ada\", \"mathematician\"), ",
            "(2, \"Grace\", \"programmer\"), (3, \"Linus\", \"engineer\")"
        ),
    );
    execute_text(
        database,
        "insert rel into Knows values (1 -> 2, 1843), (1 -> 3, 1991)",
    );
}

#[test]
#[ignore = "regenerates the committed GeoPoint golden database"]
fn mint_geo_places_anchor() {
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(GEO_ANCHOR_FILE);
    let wal = wal_path(&path);
    remove_file_if_present(&path);
    remove_file_if_present(&wal);

    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    seed_geo_anchor(&mut database);
    finish_anchor(database, &wal);
}

#[test]
fn geo_anchor_mint_is_byte_deterministic() {
    let directory = TestDirectory::new();
    let first = directory.join("geo-first.devondb");
    let second = directory.join("geo-second.devondb");

    mint_geo_anchor_at(&first);
    mint_geo_anchor_at(&second);

    assert_eq!(
        fs::read(&first).unwrap(),
        fs::read(&second).unwrap(),
        "minting identical geo anchors produced different bytes"
    );
}

fn mint_geo_anchor_at(path: &Path) {
    drop(Pager::create(path, PAGE_SIZE, DETERMINISM_DB_ID).unwrap());
    let wal = wal_path(path);
    let mut database = Database::open(path).unwrap();
    seed_geo_anchor(&mut database);
    finish_anchor(database, &wal);
}

fn seed_geo_anchor(database: &mut Database) {
    execute_text(
        database,
        concat!(
            "create node table Place (id Int64 primary key, name String, ",
            "location GeoPoint)"
        ),
    );
    execute_text(
        database,
        concat!(
            "insert into Place values (1, \"North Pole\", geo(90.0, 0.0)), ",
            "(2, \"Date Line East\", geo(-16.5, 179.9999)), ",
            "(3, \"Portland\", geo(45.5152, -122.6784)), ",
            "(4, \"Greenwich\", geo(51.4779, 0.0))"
        ),
    );
}

#[test]
#[ignore = "mints the committed scalar-v2 golden database"]
fn mint_scalar_v2_anchor() {
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(SCALAR_V2_ANCHOR_FILE);
    assert!(
        !path.exists(),
        "golden anchors are permanent; {SCALAR_V2_ANCHOR_FILE} already exists"
    );
    remove_file_if_present(&wal_path(&path));
    mint_scalar_v2_anchor_at(&path);
}

#[test]
fn scalar_v2_anchor_mint_is_byte_deterministic() {
    let directory = TestDirectory::new();
    let first = directory.join("scalar-v2-first.devondb");
    let second = directory.join("scalar-v2-second.devondb");

    mint_scalar_v2_anchor_at(&first);
    mint_scalar_v2_anchor_at(&second);

    assert_eq!(
        fs::read(&first).unwrap(),
        fs::read(&second).unwrap(),
        "minting identical scalar-v2 anchors produced different bytes"
    );
}

fn mint_scalar_v2_anchor_at(path: &Path) {
    drop(Pager::create(path, PAGE_SIZE, SCALAR_V2_DB_ID).unwrap());
    let wal = wal_path(path);
    let mut database = Database::open(path).unwrap();
    seed_scalar_v2_anchor(&mut database);
    finish_anchor(database, &wal);
}

fn seed_scalar_v2_anchor(database: &mut Database) {
    database
        .execute(&Statement::CreateNodeTable {
            name: "Scalar".to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("created_at", LogicalType::Timestamp, false),
                column("payload", LogicalType::Bytes, false),
                column(
                    "amount",
                    LogicalType::Decimal {
                        precision: 38,
                        scale: 0,
                    },
                    false,
                ),
                column("document", LogicalType::Json, false),
            ],
        })
        .unwrap();
    database
        .execute(&Statement::InsertNode {
            table: "Scalar".to_owned(),
            rows: expected_scalar_v2_rows(),
        })
        .unwrap();
}

#[test]
#[ignore = "mints the committed v0.1.0 release golden database"]
fn mint_release_0_1_0_anchor() {
    // docs/RELEASING.md step 4: every release that can write database files
    // mints an anchor with the release candidate. Once committed it is
    // permanent — never overwrite or re-mint it.
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(RELEASE_0_1_0_ANCHOR_FILE);
    assert!(
        !path.exists(),
        "release anchors are permanent; {RELEASE_0_1_0_ANCHOR_FILE} already exists"
    );
    remove_file_if_present(&wal_path(&path));
    mint_release_0_1_0_anchor_at(&path);
}

#[test]
fn release_anchor_mint_is_byte_deterministic() {
    let directory = TestDirectory::new();
    let first = directory.join("release-first.devondb");
    let second = directory.join("release-second.devondb");

    mint_release_0_1_0_anchor_at(&first);
    mint_release_0_1_0_anchor_at(&second);

    assert_eq!(
        fs::read(&first).unwrap(),
        fs::read(&second).unwrap(),
        "minting identical release anchors produced different bytes"
    );
}

fn mint_release_0_1_0_anchor_at(path: &Path) {
    drop(Pager::create(path, PAGE_SIZE, RELEASE_0_1_0_DB_ID).unwrap());
    let wal = wal_path(path);
    let mut database = Database::open(path).unwrap();
    seed_release_anchor(&mut database);
    finish_anchor(database, &wal);
}

fn seed_release_anchor(database: &mut Database) {
    execute_text(
        database,
        concat!(
            "create node table Landmark (id Int64 primary key, name String, ",
            "location GeoPoint, embedding Vector(4))"
        ),
    );
    execute_text(database, "create rel table Links from Landmark to Landmark");
    database
        .execute(&Statement::InsertNode {
            table: "Landmark".to_owned(),
            rows: expected_release_landmarks(),
        })
        .unwrap();
    database
        .execute(&Statement::InsertRel {
            table: "Links".to_owned(),
            rows: [(1, 2), (2, 3)]
                .into_iter()
                .map(|(from, to)| RelRow {
                    from_key: Value::Int64(from),
                    to_key: Value::Int64(to),
                    values: Vec::new(),
                })
                .collect(),
        })
        .unwrap();
}

#[test]
#[ignore = "regenerates the committed S-1 zone-map statistics database"]
fn mint_s1_stats_anchor() {
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(S1_STATS_ANCHOR_FILE);
    remove_file_if_present(&path);
    remove_file_if_present(&wal_path(&path));
    mint_s1_stats_anchor_at(&path);
}

#[test]
#[ignore = "mints the committed FREE_PAGES golden database"]
fn mint_free_pages_anchor() {
    // The FREE_PAGES fixture (docs/FREE_PAGES.md § Format and golden-corpus
    // impact): a database that has retired, reclaimed, AND reused pages,
    // with a ledger spanning at least two pages. A pinned snapshot blocks
    // reuse while churn accumulates >254 live entries (two ledger pages at
    // 4096B); dropping it lets the tail cycles actually reuse.
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(FREE_PAGES_ANCHOR_FILE);
    assert!(
        !path.exists(),
        "golden anchors are permanent; {FREE_PAGES_ANCHOR_FILE} already exists"
    );
    let wal = wal_path(&path);
    remove_file_if_present(&wal);

    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    execute_text(
        &mut database,
        "create node table Keyed (id Int64 primary key, body String)",
    );
    database.checkpoint().unwrap();
    let pin = database.snapshot();
    for cycle in 0_i64..70 {
        let rows = (0..4)
            .map(|row| {
                vec![
                    Value::Int64(cycle * 100 + row),
                    Value::String(format!("anchor-{cycle}-{row}-{}", "x".repeat(96))),
                ]
            })
            .collect();
        database
            .execute(&Statement::InsertNode {
                table: "Keyed".to_owned(),
                rows,
            })
            .unwrap();
        database.checkpoint().unwrap();
    }
    drop(pin);
    for cycle in 70_i64..74 {
        let rows = (0..4)
            .map(|row| {
                vec![
                    Value::Int64(cycle * 100 + row),
                    Value::String(format!("anchor-{cycle}-{row}-{}", "x".repeat(96))),
                ]
            })
            .collect();
        database
            .execute(&Statement::InsertNode {
                table: "Keyed".to_owned(),
                rows,
            })
            .unwrap();
        database.checkpoint().unwrap();
    }
    let stats = database
        .free_pages_stats()
        .unwrap()
        .expect("the anchor must carry a ledger");
    assert!(!stats.degraded);
    assert!(
        stats.session_reused > 0,
        "the anchor must have reused pages"
    );
    assert!(
        stats.retired_total > stats.session_reused,
        "some entries must still be live"
    );
    let live = stats.live_entries;
    assert!(
        live > 254,
        "the live ledger must span at least two 4096-byte pages ({live} entries)"
    );
    finish_anchor(database, &wal);
}

#[test]
#[ignore = "regenerates the committed multiprocess-coordination database"]
fn mint_multiprocess_anchor() {
    let directory = corpus_directory();
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(MULTIPROCESS_ANCHOR_FILE);
    let wal = wal_path(&path);
    remove_file_if_present(&path);
    remove_file_if_present(&wal);

    let mut database = Database::create(&path, PAGE_SIZE).unwrap();
    execute_text(
        &mut database,
        "create node table Worker (id Int64 primary key, name String)",
    );
    execute_text(
        &mut database,
        "insert into Worker values (1, \"writer\"), (2, \"reader\")",
    );
    database.checkpoint().unwrap();
    drop(database);

    Database::activate_multiprocess(&path).unwrap();
    assert_eq!(fs::metadata(&wal).unwrap().len(), 0);
    fs::remove_file(wal).unwrap();
}

fn mint_s1_stats_anchor_at(path: &Path) {
    let pager = Pager::create(path, PAGE_SIZE, S1_STATS_DB_ID).unwrap();
    let schema = s1_stats_schema();
    let types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let mut group = NodeGroup::new(types).unwrap();
    for row in s1_stats_rows() {
        group.push_row(row).unwrap();
    }
    let group_id = group.write(&pager).unwrap();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    catalog
        .set_table_storage(
            "Stats",
            TableStorage {
                groups: vec![group_id],
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();
}

fn verify_corpus_database(path: &Path, manifest: &[ManifestEntry]) {
    let file = path.file_name().and_then(OsStr::to_str).unwrap();
    let entry = manifest
        .iter()
        .find(|entry| entry.file == file)
        .unwrap_or_else(|| panic!("`{file}` is missing from manifest.json"));
    assert!(!entry.description.is_empty());
    assert_eq!(stored_page_size(path), entry.page_size, "file: {file}");
    let wal_cleanup = WalCleanup::for_database(path);
    assert!(
        !wal_cleanup.path.exists(),
        "corpus contains a WAL for {file}"
    );
    let pristine = fs::read(path).unwrap();
    let mut database = Database::open(path).unwrap();

    for table in &entry.tables {
        let result = database.run(&scan_plan(&table.name)).unwrap();
        assert_eq!(result.rows.len(), table.row_count, "{file}: {}", table.name);
        if file == M1_ANCHOR_FILE && table.name == "Person" {
            assert_eq!(
                result.columns,
                ["row.id", "row.name", "row.age", "row.embedding"]
            );
            assert_eq!(result.rows, expected_people());
        }
    }

    if file == M2_ANCHOR_FILE {
        let result = database.run(&m2_expand_plan()).unwrap();
        assert_eq!(result.columns, ["p.name", "f.name"]);
        assert_eq!(result.rows, expected_m2_expand_rows());
    }

    if file == M5_ANCHOR_FILE {
        verify_m5_anchor(&mut database, path);
    }

    if file == ONTOLOGY_ANCHOR_FILE {
        verify_ontology_anchor(&mut database);
    }

    if file == GEO_ANCHOR_FILE {
        verify_geo_anchor(&mut database, path);
    }

    if file == SCALAR_V2_ANCHOR_FILE {
        verify_scalar_v2_anchor(&mut database, path);
    }

    if file == S1_STATS_ANCHOR_FILE {
        verify_s1_stats_anchor(path);
    }

    if file == RELEASE_0_1_0_ANCHOR_FILE {
        verify_release_anchor(&mut database, path);
    }

    if file == MULTIPROCESS_ANCHOR_FILE {
        verify_multiprocess_anchor(path);
    }

    if file == FREE_PAGES_ANCHOR_FILE {
        verify_free_pages_anchor(&mut database, path);
    }

    drop(database);
    drop(wal_cleanup);
    assert_eq!(
        fs::read(path).unwrap(),
        pristine,
        "opening corpus anchor {file} altered its committed bytes"
    );
}

fn verify_m5_anchor(database: &mut Database, path: &Path) {
    let result = database.run_pin(M5_PIN_NAME).unwrap();
    assert_eq!(result.columns, ["name"]);
    assert_eq!(result.rows, expected_m5_pin_rows());
    assert_eq!(
        stored_pin_plan_bytes(path, M5_PIN_NAME).as_slice(),
        M5_PIN_PLAN_JSON.as_bytes()
    );
}

fn verify_ontology_anchor(database: &mut Database) {
    assert_eq!(
        database.schema_summary().classes,
        Some(expected_ontology_classes())
    );
    let result = database.run(&ontology_expand_plan()).unwrap();
    assert_eq!(result.columns, ["p.name", "f.name"]);
    assert_eq!(result.rows, expected_ontology_expand_rows());
}

fn verify_geo_anchor(database: &mut Database, path: &Path) {
    let result = database.run(&scan_plan("Place")).unwrap();
    assert_eq!(result.columns, ["row.id", "row.name", "row.location"]);
    assert_eq!(result.rows, expected_geo_places());
    let flags = raw_authoritative_feature_flags(path);
    assert_eq!(flags & GEO_COLUMNS_FEATURE, GEO_COLUMNS_FEATURE);
}

fn verify_scalar_v2_anchor(database: &mut Database, path: &Path) {
    let result = database.run(&scan_plan("Scalar")).unwrap();
    assert_eq!(
        result.columns,
        [
            "row.id",
            "row.created_at",
            "row.payload",
            "row.amount",
            "row.document"
        ]
    );
    assert_eq!(result.rows, expected_scalar_v2_rows());
    let flags = raw_authoritative_feature_flags(path);
    assert_eq!(
        flags & SCALAR_TYPES_V2_FLAG,
        SCALAR_TYPES_V2_FLAG,
        "scalar-v2 golden must carry feature bit 7"
    );
}

fn verify_release_anchor(database: &mut Database, path: &Path) {
    let result = database.run(&scan_plan("Landmark")).unwrap();
    assert_eq!(
        result.columns,
        ["row.id", "row.name", "row.location", "row.embedding"]
    );
    assert_eq!(result.rows, expected_release_landmarks());

    let result = database.run(&release_expand_plan()).unwrap();
    assert_eq!(result.columns, ["p.name", "f.name"]);
    assert_eq!(result.rows, expected_release_links());

    let flags = raw_authoritative_feature_flags(path);
    assert_eq!(flags & GEO_COLUMNS_FEATURE, GEO_COLUMNS_FEATURE);
    assert_ne!(flags & ZONE_MAPS_FLAG, 0);
}

fn verify_s1_stats_anchor(path: &Path) {
    let pager = Pager::open(path).unwrap();
    assert_ne!(pager.superblock().feature_flags & ZONE_MAPS_FLAG, 0);
    let catalog = Catalog::load(&pager).unwrap();
    let schema = catalog.node_table("Stats").unwrap();
    let types = schema
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect::<Vec<_>>();
    let group_id = catalog.table_storage("Stats").unwrap().groups[0];
    let directory = NodeGroup::read_directory(&pager, group_id, &types).unwrap();
    let stats = directory
        .zone_maps()
        .expect("S-1 anchor must carry ZONE_MAP_STATS below the facade");

    assert_eq!(stats.len(), 4);
    assert_eq!(stats[0].null_count, 0);
    assert_eq!(stats[0].min, Some(ZoneMapValue::Int64(1)));
    assert_eq!(stats[0].max, Some(ZoneMapValue::Int64(3)));
    assert_eq!(stats[1].null_count, 0);
    assert_eq!(stats[1].min, None, "Float64+NaN must omit min/max");
    assert_eq!(stats[1].max, None, "Float64+NaN must omit min/max");
    assert_eq!(stats[2].null_count, 3);
    assert_eq!(stats[2].min, None, "all-NULL Int64 must omit min/max");
    assert_eq!(stats[2].max, None, "all-NULL Int64 must omit min/max");

    let atoms = s1_stats_points()
        .into_iter()
        .map(|point| {
            devondb_geo::grid::atom(point.lat_deg(), point.lng_deg())
                .unwrap()
                .raw()
        })
        .collect::<Vec<_>>();
    assert_eq!(stats[3].null_count, 0);
    assert_eq!(
        stats[3].min,
        atoms.iter().copied().min().map(ZoneMapValue::GeoPoint)
    );
    assert_eq!(
        stats[3].max,
        atoms.iter().copied().max().map(ZoneMapValue::GeoPoint)
    );
}

fn verify_multiprocess_anchor(path: &Path) {
    let flags = raw_authoritative_feature_flags(path);
    assert_eq!(
        flags & MULTIPROCESS_COORDINATION_FLAG,
        MULTIPROCESS_COORDINATION_FLAG,
        "multiprocess golden must carry feature bit 8"
    );
}

fn verify_free_pages_anchor(database: &mut Database, path: &Path) {
    let flags = raw_authoritative_feature_flags(path);
    assert_eq!(
        flags & FREE_PAGES_FLAG,
        FREE_PAGES_FLAG,
        "free-pages golden must carry feature bit 5"
    );
    let stats = database
        .free_pages_stats()
        .unwrap()
        .expect("free-pages golden must carry a ledger");
    assert!(
        !stats.degraded,
        "the committed anchor's extension must validate"
    );
    assert!(
        stats.retired_total > stats.live_entries,
        "the anchor must have reused entries ({} retired, {} live)",
        stats.retired_total,
        stats.live_entries
    );
    let live = stats.live_entries;
    assert!(
        live > 254,
        "the anchor's live ledger must span two or more pages ({live} entries)"
    );
}

fn corpus_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden")
}

fn load_manifest(directory: &Path) -> Vec<ManifestEntry> {
    let manifest = fs::read_to_string(directory.join("manifest.json")).unwrap();
    ManifestParser::new(&manifest).parse().unwrap()
}

fn corpus_database_paths(directory: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension() == Some(OsStr::new("devondb")))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn scan_plan(table: &str) -> Plan {
    Plan::from_json(&format!(
        r#"{{"v":0,"plan":{{"op":"ScanNodes","table":"{table}","binding":"row"}}}}"#
    ))
    .unwrap()
}

fn m2_expand_plan() -> Plan {
    Plan::from_json(
        r#"{"v":0,"plan":{"op":"Project","exprs":[{"expr":{"col":"p.name"},"as":"p.name"},{"expr":{"col":"f.name"},"as":"f.name"}],"input":{"op":"Filter","predicate":{"gt":[{"col":"f.age"},{"lit":30}]},"input":{"op":"Expand","rel":"Knows","direction":"out","from_binding":"p","binding":"f","input":{"op":"ScanNodes","table":"Person","binding":"p"}}}}}"#,
    )
    .unwrap()
}

fn m5_pin_plan() -> Plan {
    Plan::from_json(M5_PIN_PLAN_JSON).unwrap()
}

fn release_expand_plan() -> Plan {
    Plan::from_json(
        r#"{"v":0,"plan":{"op":"Project","exprs":[{"expr":{"col":"p.name"},"as":"p.name"},{"expr":{"col":"f.name"},"as":"f.name"}],"input":{"op":"Expand","rel":"Links","direction":"out","from_binding":"p","binding":"f","input":{"op":"ScanNodes","table":"Landmark","binding":"p"}}}}"#,
    )
    .unwrap()
}

fn ontology_expand_plan() -> Plan {
    Plan::from_json(
        r#"{"v":0,"plan":{"op":"Project","exprs":[{"expr":{"col":"p.name"},"as":"p.name"},{"expr":{"col":"f.name"},"as":"f.name"}],"input":{"op":"Expand","rel":"Knows","direction":"out","from_binding":"p","binding":"f","input":{"op":"ScanNodes","table":"Person","binding":"p"}}}}"#,
    )
    .unwrap()
}

fn stored_pin_plan_bytes(path: &Path, name: &str) -> Vec<u8> {
    let pager = Pager::open(path).unwrap();
    let catalog = Catalog::load(&pager).unwrap();
    let pin = catalog.pins().iter().find(|pin| pin.name == name).unwrap();
    serde_json::to_vec(&pin.plan).unwrap()
}

fn stored_page_size(path: &Path) -> u32 {
    let bytes = fs::read(path).unwrap();
    assert!(
        bytes.len() >= SUPERBLOCK_HEADER_LEN,
        "database is too short to contain a superblock header"
    );
    let slot_one_offset = raw_u32(&bytes, 24) as usize;
    let slot_zero = valid_slot_page_size(&bytes, 0);
    let slot_one = valid_slot_page_size(&bytes, slot_one_offset);

    match (slot_zero, slot_one) {
        (Some(first), Some(second)) => {
            assert_eq!(first, second, "valid superblock page sizes differ");
            first
        }
        (Some(page_size), None) | (None, Some(page_size)) => page_size,
        (None, None) => panic!("no superblock slot has a valid CRC"),
    }
}

fn valid_slot_page_size(bytes: &[u8], offset: usize) -> Option<u32> {
    let header = bytes.get(offset..offset + SUPERBLOCK_HEADER_LEN)?;
    let stored = raw_u32(header, CHECKSUM_OFFSET);
    (crc32c::crc32c(&header[..CHECKSUM_OFFSET]) == stored).then(|| raw_u32(header, 24))
}

fn raw_authoritative_feature_flags(path: &Path) -> u64 {
    let bytes = fs::read(path).unwrap();
    assert!(bytes.len() >= PAGE_SIZE as usize + SUPERBLOCK_HEADER_LEN);
    let authoritative = [0, PAGE_SIZE as usize]
        .into_iter()
        .filter(|&offset| {
            let stored = u32::from_le_bytes(
                bytes[offset + CHECKSUM_OFFSET..offset + SUPERBLOCK_HEADER_LEN]
                    .try_into()
                    .unwrap(),
            );
            crc32c::crc32c(&bytes[offset..offset + CHECKSUM_OFFSET]) == stored
        })
        .max_by_key(|&offset| raw_u64(&bytes, offset + CHECKPOINT_LSN_OFFSET))
        .expect("no superblock slot has a valid CRC");
    raw_u64(&bytes, authoritative + FEATURE_FLAGS_OFFSET)
}

fn raw_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn raw_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn doctor_min_reader_version(path: &Path, version: u32) {
    let page_size = stored_page_size(path);
    let mut bytes = fs::read(path).unwrap();
    for slot_offset in [0, page_size as usize] {
        assert!(bytes.len() >= slot_offset + SUPERBLOCK_HEADER_LEN);
        bytes[slot_offset + MIN_READER_VERSION_OFFSET..slot_offset + 16]
            .copy_from_slice(&version.to_le_bytes());
        let checksum = crc32c::crc32c(&bytes[slot_offset..slot_offset + CHECKSUM_OFFSET]);
        bytes[slot_offset + CHECKSUM_OFFSET..slot_offset + SUPERBLOCK_HEADER_LEN]
            .copy_from_slice(&checksum.to_le_bytes());
    }
    fs::write(path, bytes).unwrap();
}

fn person_schema() -> Statement {
    Statement::CreateNodeTable {
        name: "Person".to_owned(),
        columns: vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
            column("age", LogicalType::Int64, false),
            column("embedding", LogicalType::Vector { dim: 4 }, false),
        ],
    }
}

fn s1_stats_schema() -> devondb_types::schema::NodeTableSchema {
    devondb_types::schema::NodeTableSchema::new(
        "Stats".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("sample", LogicalType::Float64, false),
            column("empty", LogicalType::Int64, false),
            column("location", LogicalType::GeoPoint, false),
        ],
    )
    .unwrap()
}

fn s1_stats_points() -> [GeoPoint; 3] {
    [
        GeoPoint::from_canonical(45.5152, -122.6784).unwrap(),
        GeoPoint::from_canonical(51.4779, 0.0).unwrap(),
        GeoPoint::from_canonical(90.0, 0.0).unwrap(),
    ]
}

fn s1_stats_rows() -> Vec<Vec<Value>> {
    let points = s1_stats_points();
    vec![
        vec![
            Value::Int64(1),
            Value::Float64(f64::NAN),
            Value::Null,
            Value::GeoPoint(points[0]),
        ],
        vec![
            Value::Int64(2),
            Value::Float64(-0.0),
            Value::Null,
            Value::GeoPoint(points[1]),
        ],
        vec![
            Value::Int64(3),
            Value::Float64(7.5),
            Value::Null,
            Value::GeoPoint(points[2]),
        ],
    ]
}

fn person_rows() -> Statement {
    Statement::InsertNode {
        table: "Person".to_owned(),
        rows: expected_people(),
    }
}

fn social_person_schema() -> Statement {
    Statement::CreateNodeTable {
        name: "Person".to_owned(),
        columns: vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
            column("age", LogicalType::Int64, false),
        ],
    }
}

fn knows_schema() -> Statement {
    Statement::CreateRelTable {
        name: "Knows".to_owned(),
        from: "Person".to_owned(),
        to: "Person".to_owned(),
        columns: Vec::new(),
    }
}

fn social_person_rows() -> Statement {
    Statement::InsertNode {
        table: "Person".to_owned(),
        rows: social_people(),
    }
}

fn knows_rows() -> Statement {
    Statement::InsertRel {
        table: "Knows".to_owned(),
        rows: [
            (1, 2),
            (1, 3),
            (2, 4),
            (2, 5),
            (3, 4),
            (4, 1),
            (4, 5),
            (5, 2),
        ]
        .into_iter()
        .map(|(from, to)| RelRow {
            from_key: Value::Int64(from),
            to_key: Value::Int64(to),
            values: Vec::new(),
        })
        .collect(),
    }
}

fn execute_text(database: &mut Database, input: &str) {
    let Parsed::Statement(envelope) = parse(input).unwrap() else {
        panic!("expected statement: {input}");
    };
    database.execute(&envelope.stmt).unwrap();
}

fn finish_anchor(mut database: Database, wal: &Path) {
    database.checkpoint().unwrap();
    assert_eq!(fs::metadata(wal).unwrap().len(), 0);
    drop(database);
    fs::remove_file(wal).unwrap();
}

fn expected_people() -> Vec<Vec<Value>> {
    vec![
        person(1, "ada", Value::Int64(36), vector(&[0.1, 0.2, 0.3, 0.4])),
        person(2, "grace", Value::Int64(45), vector(&[0.5, 0.6, 0.7, 0.8])),
        person(3, "alan", Value::Int64(41), Value::Null),
        person(4, "", Value::Null, vector(&[0.0, 0.0, 0.0, 0.0])),
        person(
            5,
            "édith",
            Value::Int64(30),
            vector(&[1.0, -1.0, 0.5, -0.5]),
        ),
    ]
}

fn social_people() -> Vec<Vec<Value>> {
    vec![
        social_person(1, "Ada", 36),
        social_person(2, "Grace", 50),
        social_person(3, "Linus", 30),
        social_person(4, "Barbara", 45),
        social_person(5, "Alan", 41),
        social_person(6, "Edsger", 28),
    ]
}

fn expected_m2_expand_rows() -> Vec<Vec<Value>> {
    [
        ("Ada", "Grace"),
        ("Grace", "Barbara"),
        ("Grace", "Alan"),
        ("Linus", "Barbara"),
        ("Barbara", "Ada"),
        ("Barbara", "Alan"),
        ("Alan", "Grace"),
    ]
    .into_iter()
    .map(|(person, friend)| {
        vec![
            Value::String(person.to_owned()),
            Value::String(friend.to_owned()),
        ]
    })
    .collect()
}

fn expected_m5_pin_rows() -> Vec<Vec<Value>> {
    ["Grace", "Linus"]
        .into_iter()
        .map(|name| vec![Value::String(name.to_owned())])
        .collect()
}

fn expected_ontology_classes() -> ClassesSummary {
    ClassesSummary {
        interfaces: vec![InterfaceSummary {
            name: "Nameable".to_owned(),
            columns: vec![InterfaceColumnSummary {
                name: "name".to_owned(),
                ty: "String".to_owned(),
            }],
        }],
        node_classes: vec![NodeClassSummary {
            table: "Person".to_owned(),
            display: "Person".to_owned(),
            plural: Some("people".to_owned()),
            label: Some("name".to_owned()),
            summary: vec!["name".to_owned(), "role".to_owned()],
            color: Some("#7aa2ff".to_owned()),
            description: Some("a human".to_owned()),
            implements: vec!["Nameable".to_owned()],
        }],
        rel_classes: vec![RelClassSummary {
            table: "Knows".to_owned(),
            verb: Some("knows".to_owned()),
            inverse: Some("is known by".to_owned()),
        }],
    }
}

fn expected_ontology_expand_rows() -> Vec<Vec<Value>> {
    ["Grace", "Linus"]
        .into_iter()
        .map(|friend| {
            vec![
                Value::String("Ada".to_owned()),
                Value::String(friend.to_owned()),
            ]
        })
        .collect()
}

fn expected_geo_places() -> Vec<Vec<Value>> {
    vec![
        geo_place(1, "North Pole", 90.0, 0.0),
        geo_place(2, "Date Line East", -16.5, 179.9999),
        geo_place(3, "Portland", 45.5152, -122.6784),
        geo_place(4, "Greenwich", 51.4779, 0.0),
    ]
}

fn expected_scalar_v2_rows() -> Vec<Vec<Value>> {
    let extreme = 10_i128.pow(38) - 1;
    vec![
        vec![
            Value::Int64(1),
            Value::Timestamp(0),
            Value::Bytes(Vec::new()),
            decimal(extreme),
            Value::Json(r#"{"outer":{"items":[1,true,null],"empty":{}}}"#.to_owned()),
        ],
        vec![
            Value::Int64(2),
            Value::Timestamp(i64::MIN),
            Value::Bytes(vec![0, 0xff, 0x80, b'd', b'b']),
            decimal(-extreme),
            Value::Json(r#"["nested",{"depth":2}]"#.to_owned()),
        ],
        vec![
            Value::Int64(3),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
    ]
}

fn decimal(digits: i128) -> Value {
    Value::Decimal(Decimal128::new(digits, 0).unwrap())
}

fn expected_release_landmarks() -> Vec<Vec<Value>> {
    vec![
        release_landmark(
            1,
            "Pioneer Courthouse",
            45.5189,
            -122.6793,
            vector(&[0.1, 0.2, 0.3, 0.4]),
        ),
        release_landmark(
            2,
            "Powells Books",
            45.5231,
            -122.6812,
            vector(&[-0.5, 0.5, -0.5, 0.5]),
        ),
        release_landmark(3, "St Johns Bridge", 45.5851, -122.7646, Value::Null),
    ]
}

fn release_landmark(
    id: i64,
    name: &str,
    lat_deg: f64,
    lng_deg: f64,
    embedding: Value,
) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::String(name.to_owned()),
        Value::GeoPoint(GeoPoint::from_canonical(lat_deg, lng_deg).unwrap()),
        embedding,
    ]
}

fn expected_release_links() -> Vec<Vec<Value>> {
    [
        ("Pioneer Courthouse", "Powells Books"),
        ("Powells Books", "St Johns Bridge"),
    ]
    .into_iter()
    .map(|(from, to)| vec![Value::String(from.to_owned()), Value::String(to.to_owned())])
    .collect()
}

fn geo_place(id: i64, name: &str, lat_deg: f64, lng_deg: f64) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::String(name.to_owned()),
        Value::GeoPoint(GeoPoint::from_canonical(lat_deg, lng_deg).unwrap()),
    ]
}

fn person(id: i64, name: &str, age: Value, embedding: Value) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::String(name.to_owned()),
        age,
        embedding,
    ]
}

fn social_person(id: i64, name: &str, age: i64) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::String(name.to_owned()),
        Value::Int64(age),
    ]
}

fn vector(elements: &[f32]) -> Value {
    Value::Vector(elements.to_vec())
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn wal_path(database: &Path) -> PathBuf {
    let mut path = OsString::from(database.as_os_str());
    path.push("-wal");
    PathBuf::from(path)
}

fn remove_file_if_present(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", path.display()),
    }
}
