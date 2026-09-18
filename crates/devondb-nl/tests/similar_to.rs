//! NL_VERSION 8 similar-to corpus (`docs/NL.md` § 18): entity lookup →
//! scalar embedding projection → `KnnScan`, k defaulting to 10.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use devondb::text::parser::{Parsed, parse};
use devondb::text::printer::print_plan;
use devondb::{ColumnSummary, Database, NodeTableSummary, SchemaSummary, Value};
use devondb_nl::{Compiled, DeterministicCompiler, IntentCompiler, NL_VERSION, NoParse};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.to_owned(),
        ty: ty.to_owned(),
        primary_key,
    }
}

fn table(name: &str, columns: Vec<ColumnSummary>) -> NodeTableSummary {
    NodeTableSummary {
        name: name.to_owned(),
        columns,
    }
}

fn person() -> NodeTableSummary {
    table(
        "Person",
        vec![
            column("id", "Int64", true),
            column("name", "String", false),
            column("embedding", "Vector(4)", false),
        ],
    )
}

fn schema_with(tables: Vec<NodeTableSummary>) -> SchemaSummary {
    SchemaSummary {
        node_tables: tables,
        rel_tables: Vec::new(),
        classes: None,
        pins: Vec::new(),
    }
}

fn embedded_schema() -> SchemaSummary {
    schema_with(vec![person()])
}

fn ambiguous_schema() -> SchemaSummary {
    schema_with(vec![table(
        "Person",
        vec![
            column("id", "Int64", true),
            column("name", "String", false),
            column("embedding", "VectorEncoded(4, cosine)", false),
            column("small_embedding", "Vector(2)", false),
        ],
    )])
}

fn cross_table_schema() -> SchemaSummary {
    schema_with(vec![
        person(),
        table(
            "Document",
            vec![
                column("id", "Int64", true),
                column("embedding", "Vector(4)", false),
            ],
        ),
    ])
}

fn compile_plan(question: &str, schema: &SchemaSummary) -> devondb::Plan {
    match DeterministicCompiler.compile(question, schema) {
        Compiled::Plan(plan) => plan,
        Compiled::NoParse(report) => panic!("question refused: {question:?}: {report:?}"),
    }
}

fn canonical(question: &str, schema: &SchemaSummary) -> String {
    print_plan(&compile_plan(question, schema)).unwrap()
}

fn refusal(compiled: Compiled, question: &str) -> NoParse {
    match compiled {
        Compiled::NoParse(report) => report,
        Compiled::Plan(_) => panic!("out-of-scope question compiled: {question:?}"),
    }
}

fn assert_schema_refusal(
    schema: &SchemaSummary,
    question: &str,
    token: &str,
    suggestion: Option<&str>,
) {
    let report = refusal(DeterministicCompiler.compile(question, schema), question);
    assert_eq!(report.unrecognized.len(), 1, "{report:?}");
    assert_eq!(report.unrecognized[0].token, token);
    assert_eq!(
        report.unrecognized[0].suggestion.as_deref(),
        suggestion,
        "{report:?}"
    );
}

#[test]
fn similar_to_templates_remain_live_in_version_nine() {
    assert_eq!(NL_VERSION, 10);
}

#[test]
fn similar_to_default_k_and_list_verb_prefixes_compile_scalar_knn() {
    let schema = embedded_schema();
    for (question, expected) in [
        (
            "people similar to ada",
            "knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = \"ada\" | project anchor.embedding), 10, cosine)",
        ),
        (
            "show people similar to Ada",
            "knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = \"Ada\" | project anchor.embedding), 10, cosine)",
        ),
        (
            "find people similar to ada",
            "knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = \"ada\" | project anchor.embedding), 10, cosine)",
        ),
        (
            "list people similar to ada",
            "knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = \"ada\" | project anchor.embedding), 10, cosine)",
        ),
    ] {
        assert_eq!(canonical(question, &schema), expected, "{question:?}");
    }
}

#[test]
fn explicit_and_top_counts_override_the_default_k() {
    let schema = embedded_schema();
    let expected = "knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = \"ada\" | project anchor.embedding), 3, cosine)";
    assert_eq!(canonical("3 people similar to ada", &schema), expected);
    assert_eq!(canonical("top 3 people similar to ada", &schema), expected);
}

#[test]
fn ascii_folding_grounds_the_same_anchor() {
    let schema = embedded_schema();
    let ada = "knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = \"Ada\" | project anchor.embedding), 10, cosine)";
    assert_eq!(canonical("People Similar To Ada", &schema), ada);
}

#[test]
fn compiled_similar_to_round_trips_through_canonical_text() {
    let schema = embedded_schema();
    let original = compile_plan("people similar to ada", &schema);
    let text = print_plan(&original).unwrap();
    let Parsed::Query(reparsed) = parse(&text).unwrap() else {
        panic!("expected query");
    };
    assert_eq!(original.plan, reparsed.plan);
}

#[test]
fn determinism_is_byte_identical_across_one_hundred_compiles() {
    let schema = embedded_schema();
    let first = canonical("top 7 people similar to Ada", &schema);
    for _ in 0..100 {
        assert_eq!(canonical("top 7 people similar to Ada", &schema), first);
    }
}

#[test]
fn a_table_without_a_vector_column_refuses_naming_the_gap() {
    let schema = schema_with(vec![table(
        "Person",
        vec![column("id", "Int64", true), column("name", "String", false)],
    )]);
    assert_schema_refusal(
        &schema,
        "people similar to ada",
        "people",
        Some("`Person` has no vector column to compare by"),
    );
}

#[test]
fn two_vector_columns_refuse_naming_every_candidate_in_schema_order() {
    assert_schema_refusal(
        &ambiguous_schema(),
        "people similar to ada",
        "people",
        Some(
            "ambiguous embedding on `Person`: `embedding`, `small_embedding` — v2.1's explicit `embedding` declaration is the escape hatch",
        ),
    );
}

#[test]
fn cross_table_similar_to_refuses_naming_the_v2_1_boundary() {
    assert_schema_refusal(
        &cross_table_schema(),
        "documents similar to ada",
        "ada",
        Some("cross-table similar-to is v2.1; the anchor must be in `Document`"),
    );
}

#[test]
fn the_like_synonym_and_number_words_refuse_with_the_nearest_template() {
    let schema = embedded_schema();
    assert_schema_refusal(
        &schema,
        "people like ada",
        "like",
        Some("`like` is not a synonym for `similar to`"),
    );
    let report = refusal(
        DeterministicCompiler.compile("five people similar to ada", &schema),
        "five people similar to ada",
    );
    assert_eq!(
        report
            .unrecognized
            .iter()
            .map(|issue| (issue.token.as_str(), issue.suggestion.as_deref()))
            .collect::<Vec<_>>(),
        [("five", None)],
        "{report:?}"
    );
    assert_eq!(
        report
            .nearest
            .iter()
            .map(|hint| hint.example.as_str())
            .collect::<Vec<_>>(),
        [
            "top <n> people similar to ada",
            "people similar to ada",
            "top <n> people with <column> over <value> by <sort-column>",
        ],
        "{report:?}"
    );
}

struct TestDatabase {
    database: Option<Database>,
    directory: PathBuf,
}

impl TestDatabase {
    fn new(label: &str, ddl: &[String]) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "devondb-nl-similar-{label}-{sequence}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let mut database = Database::create(directory.join("similar.devondb"), 4096).unwrap();
        for statement in ddl {
            let Parsed::Statement(envelope) = parse(statement).unwrap() else {
                panic!("expected statement: {statement}");
            };
            database.execute(&envelope.stmt).unwrap();
        }
        Self {
            database: Some(database),
            directory,
        }
    }

    fn database(&mut self) -> &mut Database {
        self.database.as_mut().unwrap()
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        drop(self.database.take());
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Five people whose Vector(4) embeddings are one-hot, so the anchor is the
/// unique cosine nearest neighbor of itself.
fn embedded_database() -> TestDatabase {
    let names = ["Ada", "Grace", "Linus", "Bob", "Edsger"];
    let mut ddl = vec![
        "create node table Person (id Int64 primary key, name String, embedding Vector(4))"
            .to_owned(),
    ];
    for (index, name) in names.iter().enumerate() {
        let components = (0..4)
            .map(|axis| if axis == index % 4 { "1" } else { "0" })
            .collect::<Vec<_>>()
            .join(", ");
        ddl.push(format!(
            "insert into Person values ({}, \"{name}\", [{components}])",
            index + 1
        ));
    }
    TestDatabase::new("embedded", &ddl)
}

fn assert_database_refusal(
    database: &mut Database,
    question: &str,
    token: &str,
    suggestion: Option<&str>,
) {
    let report = refusal(
        DeterministicCompiler.compile_with_database(question, database),
        question,
    );
    assert_eq!(report.unrecognized.len(), 1, "{report:?}");
    assert_eq!(report.unrecognized[0].token, token);
    assert_eq!(
        report.unrecognized[0].suggestion.as_deref(),
        suggestion,
        "{report:?}"
    );
}

#[test]
fn an_unknown_anchor_refuses_with_the_nearest_stored_spelling() {
    let mut fixture = embedded_database();
    assert_database_refusal(
        fixture.database(),
        "people similar to adaa",
        "adaa",
        Some("Ada"),
    );
}

#[test]
fn facade_runs_ranked_rows_or_the_exact_task_192_refusal() {
    let mut fixture = embedded_database();
    let database = fixture.database();
    let plan = match DeterministicCompiler.compile_with_database("people similar to ada", database)
    {
        Compiled::Plan(plan) => plan,
        Compiled::NoParse(report) => panic!("similar-to refused: {report:?}"),
    };
    match database.run(&plan) {
        Ok(result) => {
            assert_eq!(result.rows.len(), 5);
            assert_eq!(result.rows[0][1], Value::String("Ada".into()));
            assert_eq!(result.rows[0][3], Value::Float64(0.0));
        }
        Err(error) => panic!("scalar-anchored knn should execute: {error}"),
    }
}
