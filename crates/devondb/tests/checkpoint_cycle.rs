//! Public-facade checkpoint/reopen regression gates across DDL/DML shapes.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use devondb::{
    Database, QueryResult, SchemaSummary, Value,
    introspect::{
        ClassesSummary, ColumnSummary, InterfaceColumnSummary, InterfaceSummary, NodeClassSummary,
        NodeTableSummary, RelClassSummary, RelTableSummary,
    },
    text::parser::{Parsed, parse},
};
use tempfile::TempDir;

const PAGE_SIZE: u32 = 4096;

#[derive(Clone, Copy, Debug)]
enum Shape {
    Adaptive,
    Tiny,
}

impl Shape {
    fn label(self) -> &'static str {
        match self {
            Self::Adaptive => "adaptive",
            Self::Tiny => "tiny",
        }
    }

    fn initial_entity_count(self) -> usize {
        match self {
            Self::Adaptive => 32,
            Self::Tiny => 1,
        }
    }

    fn name(self, index: usize, operation: &str) -> String {
        match self {
            Self::Adaptive => format!("{operation}-{index:03}"),
            Self::Tiny => format!("{}{index}", &operation[..1]),
        }
    }

    fn kind(self, index: usize) -> String {
        match self {
            // This full column is constant, so §8.4's cost model must select
            // the non-plain constant encoding by a wide margin.
            Self::Adaptive => "encoded-kind".to_owned(),
            // Every tiny String value differs. With at most five rows, the
            // fixed costs keep every admissible encoding behind plain.
            Self::Tiny => format!("k{index}"),
        }
    }
}

#[derive(Clone, Debug)]
struct EntityRow {
    id: String,
    name: String,
    kind: String,
    rank: i64,
    weight: Option<f64>,
    active: bool,
    embedding: [f32; 2],
}

impl EntityRow {
    fn literal(&self) -> String {
        format!(
            "({}, {}, {}, {}, {}, {}, [{:.1}, {:.1}])",
            string_literal(&self.id),
            string_literal(&self.name),
            string_literal(&self.kind),
            self.rank,
            optional_float(self.weight),
            self.active,
            self.embedding[0],
            self.embedding[1]
        )
    }

    fn values(&self) -> Vec<Value> {
        vec![
            Value::String(self.id.clone()),
            Value::String(self.name.clone()),
            Value::String(self.kind.clone()),
            Value::Int64(self.rank),
            self.weight.map_or(Value::Null, Value::Float64),
            Value::Bool(self.active),
            Value::Vector(self.embedding.to_vec()),
        ]
    }
}

#[derive(Clone, Debug)]
struct DisposableRow {
    id: String,
    note: String,
    sequence: i64,
    active: bool,
}

impl DisposableRow {
    fn literal(&self) -> String {
        format!(
            "({}, {}, {}, {})",
            string_literal(&self.id),
            string_literal(&self.note),
            self.sequence,
            self.active
        )
    }

    fn values(&self) -> Vec<Value> {
        vec![
            Value::String(self.id.clone()),
            Value::String(self.note.clone()),
            Value::Int64(self.sequence),
            Value::Bool(self.active),
        ]
    }
}

#[derive(Default)]
struct ExpectedState {
    entities: BTreeMap<String, EntityRow>,
    disposable: BTreeMap<String, DisposableRow>,
    edges: BTreeSet<(String, String)>,
}

#[test]
fn ddl_dml_shape_matrix_survives_two_checkpoint_reopen_cycles() {
    for shape in [Shape::Adaptive, Shape::Tiny] {
        run_cell(shape);
    }
}

fn run_cell(shape: Shape) {
    let directory = TempDir::new().expect("create checkpoint-cycle directory");
    let path = directory.path().join(format!("{}.devondb", shape.label()));
    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    let mut expected = ExpectedState::default();

    create_schema(&mut database);
    first_dml_round(&mut database, shape, &mut expected);
    database = checkpoint_and_reopen(database, &path, shape, 1);
    assert_read_back(&mut database, &expected, shape, 1);

    second_dml_round(&mut database, shape, &mut expected);
    database = checkpoint_and_reopen(database, &path, shape, 2);
    assert_read_back(&mut database, &expected, shape, 2);
}

fn create_schema(database: &mut Database) {
    command(
        database,
        concat!(
            "create node table Entity (id String primary key, name String, kind String, ",
            "rank Int64, weight Float64, active Bool, embedding Vector(2))"
        ),
    );
    command(
        database,
        "create node table Disposable (id String primary key, note String, sequence Int64, active Bool)",
    );
    command(
        database,
        "create rel table Connects from Entity to Entity (since Int64, tag String)",
    );
    command(
        database,
        "create interface Nameable (name String, kind String)",
    );
    command(
        database,
        concat!(
            "create class for Entity (display \"Checkpoint entity\", plural \"checkpoint entities\", ",
            "label name, summary (name, kind, rank), color \"#336699\", ",
            "description \"checkpoint-cycle fixture\", implements (Nameable))"
        ),
    );
    command(
        database,
        "create class for Connects (verb \"connects\", inverse \"is connected by\")",
    );
}

fn first_dml_round(database: &mut Database, shape: Shape, expected: &mut ExpectedState) {
    let initial = (0..shape.initial_entity_count())
        .map(|index| initial_entity(shape, index))
        .collect::<Vec<_>>();
    insert_entities(database, &initial);
    store_entities(expected, initial);

    let upserts = vec![
        replacement_entity(shape, 0, "first-overwrite", 700),
        replacement_entity(shape, shape.initial_entity_count(), "first-absent", 701),
    ];
    upsert_entities(database, &upserts);
    store_entities(expected, upserts);
    let updated_name = shape.name(1, "first-update");
    update_entity(database, expected, 1, &updated_name, 801, None, true);

    first_disposable_dml(database, expected);
    insert_edge(database, expected, 0, 1, 1843, "first-forward");
    insert_edge(database, expected, 1, 0, 1952, "first-reverse");
}

fn second_dml_round(database: &mut Database, shape: Shape, expected: &mut ExpectedState) {
    let first_new = shape.initial_entity_count() + 1;
    let inserted = replacement_entity(shape, first_new, "second-insert", 900);
    insert_entities(database, std::slice::from_ref(&inserted));
    store_entities(expected, vec![inserted]);

    let upserts = vec![
        replacement_entity(shape, 0, "second-overwrite", 901),
        replacement_entity(shape, first_new + 1, "second-absent", 902),
    ];
    upsert_entities(database, &upserts);
    store_entities(expected, upserts);
    let updated_name = shape.name(1, "second-update");
    update_entity(
        database,
        expected,
        1,
        &updated_name,
        903,
        Some(18.25),
        false,
    );

    second_disposable_dml(database, expected);
    insert_edge(
        database,
        expected,
        first_new,
        first_new + 1,
        2001,
        "second-new",
    );
    insert_edge(database, expected, 0, first_new, 2002, "second-from-old");
}

fn first_disposable_dml(database: &mut Database, expected: &mut ExpectedState) {
    let inserted = vec![
        disposable(0, "insert-first", 0, true),
        disposable(1, "insert-first", 1, false),
        disposable(2, "insert-first", 2, true),
    ];
    insert_disposable(database, &inserted);
    store_disposable(expected, inserted);
    let upserts = vec![
        disposable(0, "upsert-first", 100, false),
        disposable(3, "upsert-first", 103, false),
    ];
    upsert_disposable(database, &upserts);
    store_disposable(expected, upserts);
    update_disposable(database, expected, 1, "update-first", 101, true);
    delete_disposable(database, expected, 2);
}

fn second_disposable_dml(database: &mut Database, expected: &mut ExpectedState) {
    let inserted = vec![
        disposable(4, "insert-second", 204, true),
        disposable(5, "insert-second", 205, false),
    ];
    insert_disposable(database, &inserted);
    store_disposable(expected, inserted);
    let upserts = vec![
        disposable(0, "upsert-second", 200, false),
        disposable(6, "upsert-second", 206, true),
    ];
    upsert_disposable(database, &upserts);
    store_disposable(expected, upserts);
    update_disposable(database, expected, 1, "update-second", 201, true);
    delete_disposable(database, expected, 3);
}

fn checkpoint_and_reopen(
    mut database: Database,
    path: &Path,
    shape: Shape,
    cycle: usize,
) -> Database {
    let wal = wal_path(path);
    let before = file_len(&wal);
    assert!(
        before > 0,
        "{} cycle {cycle}: fixture must have pre-checkpoint WAL transactions",
        shape.label()
    );
    database.checkpoint().unwrap_or_else(|error| {
        panic!(
            "{} cycle {cycle}: checkpoint failed: {error}",
            shape.label()
        )
    });

    // This gate uses the documented `<database>-wal` file as the public
    // compaction signal: it must shrink from committed bytes to zero, and a
    // fresh open must not restore any of those pre-checkpoint transactions.
    assert_eq!(file_len(&wal), 0, "{} cycle {cycle}", shape.label());
    drop(database);
    let reopened = Database::open(path)
        .unwrap_or_else(|error| panic!("{} cycle {cycle}: reopen failed: {error}", shape.label()));
    assert_eq!(
        file_len(&wal),
        0,
        "{} cycle {cycle}: reopen repopulated checkpointed WAL records",
        shape.label()
    );
    reopened
}

fn assert_read_back(database: &mut Database, expected: &ExpectedState, shape: Shape, cycle: usize) {
    let context = format!("{} cycle {cycle}", shape.label());
    assert_eq!(
        database.schema_summary(),
        expected_schema(),
        "{context}: schema"
    );
    assert_eq!(
        query(database, entity_query()),
        expected_entities(expected),
        "{context}: node rows"
    );
    assert_eq!(
        query(database, disposable_query()),
        expected_disposable(expected),
        "{context}: deleted/updated rows"
    );
    assert_eq!(
        query(database, outgoing_query()),
        expected_outgoing(expected),
        "{context}: outgoing traversal"
    );
    assert_eq!(
        query(database, incoming_query()),
        expected_incoming(expected),
        "{context}: incoming traversal"
    );
}

fn initial_entity(shape: Shape, index: usize) -> EntityRow {
    EntityRow {
        id: entity_id(index),
        name: shape.name(index, "initial"),
        kind: shape.kind(index),
        rank: (index as i64) * 10,
        weight: Some(index as f64 + 0.25),
        active: index.is_multiple_of(2),
        embedding: [index as f32, index as f32 + 0.5],
    }
}

fn replacement_entity(shape: Shape, index: usize, operation: &str, rank: i64) -> EntityRow {
    EntityRow {
        id: entity_id(index),
        name: shape.name(index, operation),
        kind: shape.kind(index),
        rank,
        weight: Some(rank as f64 + 0.25),
        active: rank % 2 == 0,
        embedding: [rank as f32, index as f32 + 0.5],
    }
}

fn disposable(index: usize, operation: &str, sequence: i64, active: bool) -> DisposableRow {
    DisposableRow {
        id: disposable_id(index),
        note: format!("{}{index}", &operation[..1]),
        sequence,
        active,
    }
}

fn update_entity(
    database: &mut Database,
    expected: &mut ExpectedState,
    index: usize,
    name: &str,
    rank: i64,
    weight: Option<f64>,
    active: bool,
) {
    let weight_text = optional_float(weight);
    command(
        database,
        &format!(
            "update Entity set name = {}, rank = {rank}, weight = {weight_text}, active = {active}, embedding = [{rank}.0, {index}.0] where id = {}",
            string_literal(name),
            string_literal(&entity_id(index))
        ),
    );
    let row = expected
        .entities
        .get_mut(&entity_id(index))
        .expect("updated entity exists in expected state");
    row.name = name.to_owned();
    row.rank = rank;
    row.weight = weight;
    row.active = active;
    row.embedding = [rank as f32, index as f32];
}

fn update_disposable(
    database: &mut Database,
    expected: &mut ExpectedState,
    index: usize,
    operation: &str,
    sequence: i64,
    active: bool,
) {
    let note = format!("{}{index}", &operation[..1]);
    command(
        database,
        &format!(
            "update Disposable set note = {}, sequence = {sequence}, active = {active} where id = {}",
            string_literal(&note),
            string_literal(&disposable_id(index))
        ),
    );
    let row = expected
        .disposable
        .get_mut(&disposable_id(index))
        .expect("updated disposable row exists in expected state");
    row.note = note;
    row.sequence = sequence;
    row.active = active;
}

fn delete_disposable(database: &mut Database, expected: &mut ExpectedState, index: usize) {
    command(
        database,
        &format!(
            "delete from Disposable where id = {}",
            string_literal(&disposable_id(index))
        ),
    );
    assert!(expected.disposable.remove(&disposable_id(index)).is_some());
}

fn insert_edge(
    database: &mut Database,
    expected: &mut ExpectedState,
    from: usize,
    to: usize,
    since: i64,
    tag: &str,
) {
    let from = entity_id(from);
    let to = entity_id(to);
    command(
        database,
        &format!(
            "insert rel into Connects values ({} -> {}, {since}, {})",
            string_literal(&from),
            string_literal(&to),
            string_literal(tag)
        ),
    );
    assert!(expected.edges.insert((from, to)));
}

fn insert_entities(database: &mut Database, rows: &[EntityRow]) {
    command(
        database,
        &format!(
            "insert into Entity values {}",
            rows.iter()
                .map(EntityRow::literal)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
}

fn upsert_entities(database: &mut Database, rows: &[EntityRow]) {
    command(
        database,
        &format!(
            "upsert Entity values {}",
            rows.iter()
                .map(EntityRow::literal)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
}

fn insert_disposable(database: &mut Database, rows: &[DisposableRow]) {
    command(
        database,
        &format!(
            "insert into Disposable values {}",
            rows.iter()
                .map(DisposableRow::literal)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
}

fn upsert_disposable(database: &mut Database, rows: &[DisposableRow]) {
    command(
        database,
        &format!(
            "upsert Disposable values {}",
            rows.iter()
                .map(DisposableRow::literal)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
}

fn store_entities(expected: &mut ExpectedState, rows: Vec<EntityRow>) {
    for row in rows {
        expected.entities.insert(row.id.clone(), row);
    }
}

fn store_disposable(expected: &mut ExpectedState, rows: Vec<DisposableRow>) {
    for row in rows {
        expected.disposable.insert(row.id.clone(), row);
    }
}

fn expected_entities(expected: &ExpectedState) -> QueryResult {
    QueryResult {
        columns: vec![
            "id",
            "name",
            "kind",
            "rank",
            "weight",
            "active",
            "embedding",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        rows: expected.entities.values().map(EntityRow::values).collect(),
    }
}

fn expected_disposable(expected: &ExpectedState) -> QueryResult {
    QueryResult {
        columns: vec!["id", "note", "sequence", "active"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        rows: expected
            .disposable
            .values()
            .map(DisposableRow::values)
            .collect(),
    }
}

fn expected_outgoing(expected: &ExpectedState) -> QueryResult {
    let rows = expected
        .edges
        .iter()
        .map(|(source, target)| {
            let target_name = &expected.entities[target].name;
            vec![
                Value::String(source.clone()),
                Value::String(target.clone()),
                Value::String(target_name.clone()),
            ]
        })
        .collect();
    QueryResult {
        columns: vec!["source".into(), "target".into(), "target_name".into()],
        rows,
    }
}

fn expected_incoming(expected: &ExpectedState) -> QueryResult {
    let mut edges = expected.edges.iter().collect::<Vec<_>>();
    edges.sort_by(|left, right| (&left.1, &left.0).cmp(&(&right.1, &right.0)));
    let rows = edges
        .into_iter()
        .map(|(source, target)| {
            let source_name = &expected.entities[source].name;
            vec![
                Value::String(target.clone()),
                Value::String(source.clone()),
                Value::String(source_name.clone()),
            ]
        })
        .collect();
    QueryResult {
        columns: vec!["target".into(), "source".into(), "source_name".into()],
        rows,
    }
}

fn expected_schema() -> SchemaSummary {
    SchemaSummary {
        node_tables: vec![entity_schema(), disposable_schema()],
        rel_tables: vec![RelTableSummary {
            name: "Connects".into(),
            from: "Entity".into(),
            to: "Entity".into(),
            columns: vec![
                column("since", "Int64", false),
                column("tag", "String", false),
            ],
        }],
        classes: Some(expected_classes()),
        pins: Vec::new(),
    }
}

fn entity_schema() -> NodeTableSummary {
    NodeTableSummary {
        name: "Entity".into(),
        columns: vec![
            column("id", "String", true),
            column("name", "String", false),
            column("kind", "String", false),
            column("rank", "Int64", false),
            column("weight", "Float64", false),
            column("active", "Bool", false),
            column("embedding", "Vector(2)", false),
        ],
    }
}

fn disposable_schema() -> NodeTableSummary {
    NodeTableSummary {
        name: "Disposable".into(),
        columns: vec![
            column("id", "String", true),
            column("note", "String", false),
            column("sequence", "Int64", false),
            column("active", "Bool", false),
        ],
    }
}

fn expected_classes() -> ClassesSummary {
    ClassesSummary {
        interfaces: vec![InterfaceSummary {
            name: "Nameable".into(),
            columns: vec![
                InterfaceColumnSummary {
                    name: "name".into(),
                    ty: "String".into(),
                },
                InterfaceColumnSummary {
                    name: "kind".into(),
                    ty: "String".into(),
                },
            ],
        }],
        node_classes: vec![entity_class(), disposable_class()],
        rel_classes: vec![RelClassSummary {
            table: "Connects".into(),
            verb: Some("connects".into()),
            inverse: Some("is connected by".into()),
        }],
    }
}

fn entity_class() -> NodeClassSummary {
    NodeClassSummary {
        table: "Entity".into(),
        display: "Checkpoint entity".into(),
        plural: Some("checkpoint entities".into()),
        label: Some("name".into()),
        summary: vec!["name".into(), "kind".into(), "rank".into()],
        color: Some("#336699".into()),
        description: Some("checkpoint-cycle fixture".into()),
        implements: vec!["Nameable".into()],
    }
}

fn disposable_class() -> NodeClassSummary {
    NodeClassSummary {
        table: "Disposable".into(),
        display: "Disposable".into(),
        plural: Some("disposables".into()),
        label: None,
        summary: Vec::new(),
        color: None,
        description: None,
        implements: Vec::new(),
    }
}

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.into(),
        ty: ty.into(),
        primary_key,
    }
}

fn entity_query() -> &'static str {
    concat!(
        "nodes(Entity) as entity | sort entity.id | project entity.id as id, ",
        "entity.name as name, entity.kind as kind, entity.rank as rank, ",
        "entity.weight as weight, entity.active as active, entity.embedding as embedding"
    )
}

fn disposable_query() -> &'static str {
    concat!(
        "nodes(Disposable) as row | sort row.id | project row.id as id, row.note as note, ",
        "row.sequence as sequence, row.active as active"
    )
}

fn outgoing_query() -> &'static str {
    concat!(
        "nodes(Entity) as source | expand Connects out as target | sort source.id, target.id | ",
        "project source.id as source, target.id as target, target.name as target_name"
    )
}

fn incoming_query() -> &'static str {
    concat!(
        "nodes(Entity) as target | expand Connects in as source | sort target.id, source.id | ",
        "project target.id as target, source.id as source, source.name as source_name"
    )
}

fn execute_text(database: &mut Database, input: &str) -> Option<QueryResult> {
    match parse(input).unwrap_or_else(|error| panic!("parse {input:?}: {error}")) {
        Parsed::Statement(envelope) => {
            database
                .execute(&envelope.stmt)
                .unwrap_or_else(|error| panic!("execute {input:?}: {error}"));
            None
        }
        Parsed::Query(plan) => Some(
            database
                .run(&plan)
                .unwrap_or_else(|error| panic!("query {input:?}: {error}")),
        ),
    }
}

fn command(database: &mut Database, input: &str) {
    assert!(
        execute_text(database, input).is_none(),
        "expected statement: {input}"
    );
}

fn query(database: &mut Database, input: &str) -> QueryResult {
    execute_text(database, input).unwrap_or_else(|| panic!("expected query: {input}"))
}

fn entity_id(index: usize) -> String {
    format!("e{index:03}")
}

fn disposable_id(index: usize) -> String {
    format!("d{index:03}")
}

fn string_literal(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn optional_float(value: Option<f64>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| format!("{value:.2}"))
}

fn wal_path(path: &Path) -> PathBuf {
    let mut with_suffix = OsString::from(path.as_os_str());
    with_suffix.push("-wal");
    PathBuf::from(with_suffix)
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path)
        .unwrap_or_else(|error| panic!("metadata for {}: {error}", path.display()))
        .len()
}
