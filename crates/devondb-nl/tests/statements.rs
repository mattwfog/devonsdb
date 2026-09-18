use devondb::text::parser::{Parsed, parse};
use devondb::{ColumnSummary, NodeTableSummary, SchemaSummary};
use devondb_nl::{
    Compiled, CompiledStatement, DeterministicCompiler, IntentCompiler, NoParse, Ungrounded,
};

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.to_owned(),
        ty: ty.to_owned(),
        primary_key,
    }
}

fn schema(key_type: &str) -> SchemaSummary {
    SchemaSummary {
        node_tables: vec![NodeTableSummary {
            name: "Person".to_owned(),
            columns: vec![
                column("id", key_type, true),
                column("name", "String", false),
                column("age", "Int64", false),
                column("score", "Float64", false),
            ],
        }],
        rel_tables: Vec::new(),
        classes: None,
        pins: Vec::new(),
    }
}

fn expected_statement(text: &str) -> devondb::Statement {
    let Parsed::Statement(envelope) = parse(text).unwrap() else {
        panic!("expected a statement: {text}");
    };
    envelope.stmt
}

fn compile_statement(input: &str, schema: &SchemaSummary) -> devondb::Statement {
    match DeterministicCompiler.compile_statement(input, schema) {
        CompiledStatement::Statement(statement) => statement,
        CompiledStatement::NoParse(report) => {
            panic!("statement refused: {input:?}: {report:?}")
        }
    }
}

fn refusal(input: &str, schema: &SchemaSummary) -> NoParse {
    match DeterministicCompiler.compile_statement(input, schema) {
        CompiledStatement::NoParse(report) => report,
        CompiledStatement::Statement(statement) => {
            panic!("refused input compiled: {input:?}: {statement:?}")
        }
    }
}

#[test]
fn golden_statement_corpus_compiles_to_exact_statements() {
    let integer_key = schema("Int64");
    let string_key = schema("String");
    for (input, schema, expected) in [
        (
            "delete person 42",
            &integer_key,
            "delete from Person where id = 42",
        ),
        (
            "remove person ada",
            &string_key,
            "delete from Person where id = \"ada\"",
        ),
        (
            "set person 42 age to 39",
            &integer_key,
            "update Person set age = 39 where id = 42",
        ),
        (
            "change person 42 name to Ada",
            &integer_key,
            "update Person set name = \"Ada\" where id = 42",
        ),
    ] {
        assert_eq!(
            compile_statement(input, schema),
            expected_statement(expected),
            "input: {input:?}"
        );
    }
}

#[test]
fn statement_grounding_uses_catalog_folding_and_exact_literal_types() {
    let schema = schema("Int64");
    assert_eq!(
        compile_statement("SET PERSON 42 AGE TO 39", &schema),
        expected_statement("update Person set age = 39 where id = 42")
    );
    assert_eq!(
        compile_statement("change person 42 name to ada", &schema),
        expected_statement("update Person set name = \"ada\" where id = 42")
    );
    assert!(matches!(
        DeterministicCompiler.compile_statement("set person 42 score to 39.0", &schema),
        CompiledStatement::Statement(_)
    ));
}

#[test]
fn refusal_corpus_covers_bulk_unknowns_type_mismatches_and_extra_tokens() {
    let schema = schema("Int64");
    let cases = [
        "delete all people over 40",
        "delete persno 42",
        "set person 42 agge to 39",
        "delete person ada",
        "set person 42 age to Ada",
        "delete person 42 now",
    ];
    for input in cases {
        let report = refusal(input, &schema);
        assert!(!report.nearest.is_empty(), "input: {input:?}");
        assert!(
            report.nearest.iter().any(|hint| {
                hint.example.contains("<pk-value>")
                    || hint.example.contains("42")
                    || hint.example.contains("40")
            }),
            "PK-addressed hint missing for {input:?}: {:?}",
            report.nearest
        );
    }

    let bulk = refusal("delete all people over 40", &schema);
    assert_eq!(
        bulk.nearest
            .iter()
            .map(|hint| hint.example.as_str())
            .collect::<Vec<_>>(),
        vec![
            "delete people 40",
            "remove people 40",
            "set people 40 <column> to <value>",
        ]
    );

    let unknown_table = refusal("delete persno 42", &schema);
    assert_eq!(
        unknown_table.unrecognized,
        vec![Ungrounded {
            token: "persno".to_owned(),
            suggestion: Some("Person".to_owned()),
        }]
    );

    let unknown_column = refusal("set person 42 agge to 39", &schema);
    assert_eq!(
        unknown_column.unrecognized,
        vec![Ungrounded {
            token: "agge".to_owned(),
            suggestion: Some("age".to_owned()),
        }]
    );
    assert_eq!(
        unknown_column
            .nearest
            .iter()
            .map(|hint| hint.example.as_str())
            .collect::<Vec<_>>(),
        vec![
            "set person 42 <column> to 39",
            "change person 42 <column> to 39",
            "delete person 42",
        ]
    );
}

#[test]
fn statement_outcomes_are_deterministic_for_successes_and_refusals() {
    let schema = schema("Int64");
    for input in [
        "delete person 42",
        "set person 42 age to 39",
        "delete all people over 40",
        "delete person wrong",
    ] {
        assert_eq!(
            DeterministicCompiler.compile_statement(input, &schema),
            DeterministicCompiler.compile_statement(input, &schema),
            "input: {input:?}"
        );
    }
}

struct QueryOnlyCompiler;

impl IntentCompiler for QueryOnlyCompiler {
    fn compile(&self, _question: &str, _schema: &SchemaSummary) -> Compiled {
        Compiled::NoParse(NoParse::default())
    }
}

#[test]
fn default_statement_method_is_an_all_ungrounded_opt_out() {
    let CompiledStatement::NoParse(report) =
        QueryOnlyCompiler.compile_statement("delete person 42", &schema("Int64"))
    else {
        panic!("default statement compiler did not opt out");
    };
    assert!(report.recognized.is_empty());
    assert_eq!(
        report.unrecognized,
        vec![
            Ungrounded {
                token: "delete".to_owned(),
                suggestion: None,
            },
            Ungrounded {
                token: "person".to_owned(),
                suggestion: None,
            },
            Ungrounded {
                token: "42".to_owned(),
                suggestion: None,
            },
        ]
    );
    assert!(!report.nearest.is_empty());
}
