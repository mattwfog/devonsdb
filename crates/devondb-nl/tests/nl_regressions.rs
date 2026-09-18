use std::collections::BTreeSet;

use devondb::text::parser::{Parsed, parse};
use devondb::text::printer::print_plan;
use devondb::{ColumnSummary, NodeTableSummary, SchemaSummary};
use devondb_nl::{
    Compiled, CompiledStatement, DeterministicCompiler, IntentCompiler, NL_VERSION, NoParse,
};

// White-box imports exercise private lexical helpers without widening
// devondb-nl's public API.
#[allow(dead_code)]
mod normalize {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum TokenKind {
        Word,
        Number,
        Quoted,
        Geo,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Token {
        pub(crate) original: String,
        pub(crate) folded: String,
        pub(crate) kind: TokenKind,
    }
}

#[allow(dead_code)]
#[path = "../src/syntax.rs"]
mod syntax_under_test;
#[allow(dead_code)]
#[path = "../src/vocabulary.rs"]
mod vocabulary_under_test;

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.to_owned(),
        ty: ty.to_owned(),
        primary_key,
    }
}

fn schema_with_columns(columns: Vec<ColumnSummary>) -> SchemaSummary {
    SchemaSummary {
        node_tables: vec![NodeTableSummary {
            name: "Person".to_owned(),
            columns,
        }],
        rel_tables: Vec::new(),
        classes: None,
        pins: Vec::new(),
    }
}

fn people_schema() -> SchemaSummary {
    schema_with_columns(vec![
        column("id", "Int64", true),
        column("name", "String", false),
        column("age", "Int64", false),
    ])
}

fn compile_plan(question: &str, schema: &SchemaSummary) -> devondb::Plan {
    match DeterministicCompiler.compile(question, schema) {
        Compiled::Plan(plan) => plan,
        Compiled::NoParse(report) => panic!("question refused: {question:?}: {report:?}"),
    }
}

fn refusal(question: &str, schema: &SchemaSummary) -> NoParse {
    match DeterministicCompiler.compile(question, schema) {
        Compiled::NoParse(report) => report,
        Compiled::Plan(plan) => panic!(
            "question unexpectedly compiled: {question:?}: {}",
            print_plan(&plan).unwrap()
        ),
    }
}

#[test]
fn nl_version_nine_covers_task_278_and_review_corrections() {
    assert_eq!(NL_VERSION, 10);
}

#[test]
fn quoted_tokens_are_never_comparator_or_vocabulary_words() {
    use normalize::{Token, TokenKind};

    let quoted_not = Token {
        original: "not".to_owned(),
        folded: "not".to_owned(),
        kind: TokenKind::Quoted,
    };
    assert_eq!(
        vocabulary_under_test::comparator(std::slice::from_ref(&quoted_not), 0),
        None
    );
    assert!(!vocabulary_under_test::is_vocabulary(&quoted_not));

    let plan = compile_plan("people whose name is \"not\"", &people_schema());
    assert_eq!(
        print_plan(&plan).unwrap(),
        "nodes(Person) as person | filter person.name = \"not\""
    );
}

#[test]
fn folded_reserved_identifiers_are_backtick_quoted_and_parseable() {
    assert_eq!(syntax_under_test::identifier("Count"), "`Count`");
    assert_eq!(syntax_under_test::identifier("Where"), "`Where`");

    let schema = schema_with_columns(vec![
        column("id", "Int64", true),
        column("name", "String", false),
        column("Count", "Int64", false),
    ]);
    let compiled = compile_plan("people with Count over 1", &schema);
    let expected_text = "nodes(Person) as person | filter person.`Count` > 1";
    let Parsed::Query(expected) = parse(expected_text).unwrap() else {
        panic!("expected query parsed as a statement");
    };
    assert_eq!(compiled, expected);
    assert_eq!(parse(expected_text).unwrap(), Parsed::Query(compiled));
}

#[test]
fn c0_string_controls_use_the_plan_printer_escape() {
    assert_eq!(syntax_under_test::quote_string("A\u{1}da"), "\"A\\u{1}da\"");

    let expected_text = "nodes(Person) as person | filter person.name = \"A\\u{1}da\"";
    let Parsed::Query(expected) = parse(expected_text).unwrap() else {
        panic!("expected query parsed as a statement");
    };
    for question in [
        "people whose name is A\u{1}da",
        "people whose name is \"A\u{1}da\"",
    ] {
        let compiled = compile_plan(question, &people_schema());
        assert_eq!(compiled, expected, "question: {question:?}");
        assert_eq!(print_plan(&compiled).unwrap(), expected_text);
    }
}

#[test]
fn vocabulary_words_are_unique_and_with_has_a_stable_suggestion() {
    let words = vocabulary_under_test::all_words().collect::<Vec<_>>();
    assert_eq!(words.iter().filter(|word| **word == "with").count(), 1);
    assert_eq!(
        words.iter().copied().collect::<BTreeSet<_>>().len(),
        words.len()
    );

    let report = refusal("people wit age over 30", &people_schema());
    let typo = report
        .unrecognized
        .iter()
        .find(|item| item.token == "wit")
        .expect("the connector typo must be reported");
    assert_eq!(typo.suggestion.as_deref(), Some("with"));
}

#[test]
fn nearest_templates_rank_by_grounded_slots_with_family_markers_as_one_slot() {
    // NL.md § 7 / § 19: a recognized family marker (`similar to`, a head
    // verb, `within`, …) grounds the matching template's marker slot and
    // counts as ONE grounded slot; among equal counts the marker-matched
    // template ranks first, then declaration order. No separate tier.
    let report = refusal("people similar to Ada", &people_schema());
    assert_eq!(
        report
            .nearest
            .iter()
            .map(|hint| hint.example.as_str())
            .collect::<Vec<_>>(),
        vec![
            "top <n> people similar to Ada",
            "people similar to Ada",
            "top <n> people with <column> over Ada by <sort-column>",
        ]
    );

    // Statement refusals rank the same way: the `delete` head verb grounds
    // the delete family's verb slot, so it outranks the update family
    // (equal grounded slots otherwise); declaration order breaks the rest.
    let CompiledStatement::NoParse(report) =
        DeterministicCompiler.compile_statement("delete all people over 40", &people_schema())
    else {
        panic!("bulk delete unexpectedly compiled");
    };
    assert_eq!(
        report
            .nearest
            .iter()
            .map(|hint| hint.example.as_str())
            .collect::<Vec<_>>(),
        vec![
            "delete people 40",
            "remove people 40",
            "set people 40 <column> to <value>",
        ]
    );
}

#[test]
fn punctuation_boundaries_do_not_retarget_values() {
    let schema = people_schema();
    for (question, expected) in [
        (
            "who is ada!",
            "nodes(Person) as person | filter person.name = \"ada\"",
        ),
        ("people;", "nodes(Person) as person"),
        (
            "who is (ada)",
            "nodes(Person) as person | filter person.name = \"ada\"",
        ),
        ("show: people", "nodes(Person) as person"),
    ] {
        let plan = compile_plan(question, &schema);
        assert_eq!(
            print_plan(&plan).unwrap(),
            expected,
            "question: {question:?}"
        );
    }

    let dot_led = refusal("people with age over .5", &schema);
    assert!(dot_led.unrecognized.iter().any(|token| token.token == ".5"));
}
