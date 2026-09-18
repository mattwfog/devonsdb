use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use devondb::introspect::{ClassesSummary, NodeClassSummary, RelClassSummary};
use devondb::text::parser::{Parsed, parse};
use devondb::text::printer::print_plan;
use devondb::{ColumnSummary, Database, NodeTableSummary, RelTableSummary, SchemaSummary};
use devondb_nl::{
    Compiled, DeterministicCompiler, Grounded, IntentCompiler, NL_VERSION, NoParse, TemplateHint,
    Ungrounded,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum Catalog {
    People,
    Places,
}

struct GoldenCase {
    question: &'static str,
    catalog: Catalog,
    expected: &'static str,
}

struct WithinGoldenCase {
    question: &'static str,
    expected: &'static str,
    expected_columns: &'static [&'static str],
    expected_first_values: &'static [&'static str],
}

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.to_owned(),
        ty: ty.to_owned(),
        primary_key,
    }
}

fn person() -> NodeTableSummary {
    NodeTableSummary {
        name: "Person".to_owned(),
        columns: vec![
            column("id", "Int64", true),
            column("name", "String", false),
            column("age", "Int64", false),
            column("score", "Float64", false),
        ],
    }
}

fn people_schema() -> SchemaSummary {
    SchemaSummary {
        node_tables: vec![person()],
        rel_tables: vec![RelTableSummary {
            name: "Knows".to_owned(),
            from: "Person".to_owned(),
            to: "Person".to_owned(),
            columns: Vec::new(),
        }],
        classes: None,
        pins: Vec::new(),
    }
}

fn multi_hop_schema() -> SchemaSummary {
    let mut schema = people_schema();
    schema.classes = Some(ClassesSummary {
        interfaces: Vec::new(),
        node_classes: Vec::new(),
        rel_classes: vec![RelClassSummary {
            table: "Knows".to_owned(),
            verb: Some("friends".to_owned()),
            inverse: None,
        }],
    });
    schema
}

fn places_schema() -> SchemaSummary {
    SchemaSummary {
        node_tables: vec![
            person(),
            NodeTableSummary {
                name: "City".to_owned(),
                columns: vec![
                    column("id", "Int64", true),
                    column("name", "String", false),
                    column("population", "Int64", false),
                ],
            },
        ],
        rel_tables: vec![RelTableSummary {
            name: "Lives_In".to_owned(),
            from: "Person".to_owned(),
            to: "City".to_owned(),
            columns: Vec::new(),
        }],
        classes: None,
        pins: Vec::new(),
    }
}

fn ontology_schema() -> SchemaSummary {
    SchemaSummary {
        node_tables: vec![NodeTableSummary {
            name: "Person".to_owned(),
            columns: vec![
                column("id", "Int64", true),
                column("name", "String", false),
                column("handle", "String", false),
            ],
        }],
        rel_tables: vec![RelTableSummary {
            name: "Tracks".to_owned(),
            from: "Person".to_owned(),
            to: "Person".to_owned(),
            columns: Vec::new(),
        }],
        classes: Some(ClassesSummary {
            interfaces: Vec::new(),
            node_classes: vec![NodeClassSummary {
                table: "Person".to_owned(),
                display: "Human".to_owned(),
                plural: Some("humans".to_owned()),
                label: Some("handle".to_owned()),
                summary: Vec::new(),
                color: None,
                description: None,
                implements: Vec::new(),
            }],
            rel_classes: vec![RelClassSummary {
                table: "Tracks".to_owned(),
                verb: Some("follows".to_owned()),
                inverse: Some("get followed by".to_owned()),
            }],
        }),
        pins: Vec::new(),
    }
}

fn schema(catalog: Catalog) -> SchemaSummary {
    match catalog {
        Catalog::People => people_schema(),
        Catalog::Places => places_schema(),
    }
}

fn golden_cases() -> Vec<GoldenCase> {
    vec![
        GoldenCase {
            question: "show all people",
            catalog: Catalog::People,
            expected: "nodes(Person) as person",
        },
        GoldenCase {
            question: "people with age over 30",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.age > 30",
        },
        GoldenCase {
            question: "who is Ada",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.name = \"Ada\"",
        },
        GoldenCase {
            question: "show ada",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.name = \"ada\"",
        },
        GoldenCase {
            question: "who does ada know",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.name = \"ada\" | expand Knows out as other | project other.name",
        },
        GoldenCase {
            question: "how many people have age over 30",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.age > 30 | aggregate count(person.id) as `count`",
        },
        GoldenCase {
            question: "top 5 people by age",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | sort person.age desc | limit 5",
        },
        GoldenCase {
            question: "display people with age at least 21",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.age >= 21",
        },
        GoldenCase {
            question: "people where age over 30 and score below 100.5",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.age > 30 and person.score < 100.5",
        },
        GoldenCase {
            question: "number of people",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | aggregate count(person.id) as `count`",
        },
        GoldenCase {
            question: "count people whose name equals Ada",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.name = \"Ada\" | aggregate count(person.id) as `count`",
        },
        GoldenCase {
            question: "who knows Ada",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.name = \"Ada\" | expand Knows in as other | project other.name",
        },
        GoldenCase {
            question: "who does Ada know with age under 50",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.name = \"Ada\" | expand Knows out as other | filter other.age < 50 | project other.name",
        },
        GoldenCase {
            question: "who knows Ada having score above 1.5",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.name = \"Ada\" | expand Knows in as other | filter other.score > 1.5 | project other.name",
        },
        GoldenCase {
            question: "top 3 people with age over 30 by score",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.age > 30 | sort person.score desc | limit 3",
        },
        GoldenCase {
            question: "bottom 2 people with age at least 18 by age",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.age >= 18 | sort person.age | limit 2",
        },
        GoldenCase {
            question: "how many people with age over 30 and score at most 99.5",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.age > 30 and person.score <= 99.5 | aggregate count(person.id) as `count`",
        },
        GoldenCase {
            question: "who lives in Boston",
            catalog: Catalog::Places,
            expected: "nodes(City) as city | filter city.name = \"Boston\" | expand Lives_In in as other | project other.name",
        },
        GoldenCase {
            question: "who does Ada live in",
            catalog: Catalog::Places,
            expected: "nodes(Person) as person | filter person.name = \"Ada\" | expand Lives_In out as other | project other.name",
        },
        GoldenCase {
            question: "total score of people",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | aggregate sum(person.score) as total",
        },
        GoldenCase {
            question: "highest age of people",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | aggregate max(person.age) as highest",
        },
        GoldenCase {
            question: "top 4 people having age after 20 and score at least 2.5 by score",
            catalog: Catalog::People,
            expected: "nodes(Person) as person | filter person.age > 20 and person.score >= 2.5 | sort person.score desc | limit 4",
        },
    ]
}

fn within_golden_cases() -> [WithinGoldenCase; 5] {
    const CAFE: &[&str] = &["Cafe.id", "Cafe.name", "Cafe.location", "Cafe.rating"];
    const COUNT: &[&str] = &["count"];
    [
        WithinGoldenCase {
            question: "cafes within 1 mile of Portland",
            expected: "within(Cafe.location, geo(45.5152, -122.6784), 1609.344)",
            expected_columns: CAFE,
            expected_first_values: &["1", "2"],
        },
        WithinGoldenCase {
            question: "cafes within 1 mile of portland",
            expected: "within(Cafe.location, geo(45.5152, -122.6784), 1609.344)",
            expected_columns: CAFE,
            expected_first_values: &["1", "2"],
        },
        WithinGoldenCase {
            question: "show cafes within 2 kilometers of geo(45.5152, -122.6784)",
            expected: "within(Cafe.location, geo(45.5152, -122.6784), 2000.0)",
            expected_columns: CAFE,
            expected_first_values: &["1", "2"],
        },
        WithinGoldenCase {
            question: "how many cafes within 500 feet of Portland",
            expected: "within(Cafe.location, geo(45.5152, -122.6784), 152.4) | aggregate count(Cafe.id) as `count`",
            expected_columns: COUNT,
            expected_first_values: &["2"],
        },
        WithinGoldenCase {
            question: "cafes with rating over 4 within 100 meters of Portland",
            expected: "within(Cafe.location, geo(45.5152, -122.6784), 100.0) | filter Cafe.rating > 4",
            expected_columns: CAFE,
            expected_first_values: &["1"],
        },
    ]
}

fn expected_execution(question: &str) -> (&'static [&'static str], &'static str) {
    const PERSON: &[&str] = &["person.id", "person.name", "person.age", "person.score"];
    const COUNT: &[&str] = &["count"];
    const OTHER_NAME: &[&str] = &["other.name"];
    match question {
        "show all people" => (
            PERSON,
            "[[Int64(1), String(\"Ada\"), Int64(36), Float64(9.5)], [Int64(2), String(\"Grace\"), Int64(29), Float64(7.0)], [Int64(3), String(\"Linus\"), Int64(45), Float64(2.0)], [Int64(4), String(\"Bob\"), Int64(18), Float64(100.0)]]",
        ),
        "people with age over 30" | "people where age over 30 and score below 100.5" => (
            PERSON,
            "[[Int64(1), String(\"Ada\"), Int64(36), Float64(9.5)], [Int64(3), String(\"Linus\"), Int64(45), Float64(2.0)]]",
        ),
        "who is Ada" => (
            PERSON,
            "[[Int64(1), String(\"Ada\"), Int64(36), Float64(9.5)]]",
        ),
        "show ada" | "who does ada know" => {
            let columns = if question == "show ada" {
                PERSON
            } else {
                OTHER_NAME
            };
            (columns, "[]")
        }
        "how many people have age over 30"
        | "how many people with age over 30 and score at most 99.5" => (COUNT, "[[Int64(2)]]"),
        "top 5 people by age" => (
            PERSON,
            "[[Int64(3), String(\"Linus\"), Int64(45), Float64(2.0)], [Int64(1), String(\"Ada\"), Int64(36), Float64(9.5)], [Int64(2), String(\"Grace\"), Int64(29), Float64(7.0)], [Int64(4), String(\"Bob\"), Int64(18), Float64(100.0)]]",
        ),
        "display people with age at least 21" => (
            PERSON,
            "[[Int64(1), String(\"Ada\"), Int64(36), Float64(9.5)], [Int64(2), String(\"Grace\"), Int64(29), Float64(7.0)], [Int64(3), String(\"Linus\"), Int64(45), Float64(2.0)]]",
        ),
        "number of people" => (COUNT, "[[Int64(4)]]"),
        "count people whose name equals Ada" => (COUNT, "[[Int64(1)]]"),
        "who knows Ada" | "who knows Ada having score above 1.5" => {
            (OTHER_NAME, "[[String(\"Grace\")]]")
        }
        "who does Ada know with age under 50" => {
            (OTHER_NAME, "[[String(\"Grace\")], [String(\"Linus\")]]")
        }
        "top 3 people with age over 30 by score" => (
            PERSON,
            "[[Int64(1), String(\"Ada\"), Int64(36), Float64(9.5)], [Int64(3), String(\"Linus\"), Int64(45), Float64(2.0)]]",
        ),
        "bottom 2 people with age at least 18 by age" => (
            PERSON,
            "[[Int64(4), String(\"Bob\"), Int64(18), Float64(100.0)], [Int64(2), String(\"Grace\"), Int64(29), Float64(7.0)]]",
        ),
        "who lives in Boston" => (OTHER_NAME, "[[String(\"Ada\")]]"),
        "who does Ada live in" => (OTHER_NAME, "[[String(\"Boston\")]]"),
        "total score of people" => (&["total"], "[[Float64(118.5)]]"),
        "highest age of people" => (&["highest"], "[[Int64(45)]]"),
        "top 4 people having age after 20 and score at least 2.5 by score" => (
            PERSON,
            "[[Int64(1), String(\"Ada\"), Int64(36), Float64(9.5)], [Int64(2), String(\"Grace\"), Int64(29), Float64(7.0)]]",
        ),
        _ => panic!("missing execution expectation for {question:?}"),
    }
}

fn compile_plan(question: &str, schema: &SchemaSummary) -> devondb::Plan {
    match DeterministicCompiler.compile(question, schema) {
        Compiled::Plan(plan) => plan,
        Compiled::NoParse(report) => panic!("question refused: {question:?}: {report:?}"),
    }
}

fn compile_database_plan(question: &str, database: &mut Database) -> devondb::Plan {
    match DeterministicCompiler.compile_with_database(question, database) {
        Compiled::Plan(plan) => plan,
        Compiled::NoParse(report) => panic!("question refused: {question:?}: {report:?}"),
    }
}

#[test]
fn golden_question_corpus_compiles_to_exact_canonical_text() {
    for case in golden_cases() {
        let plan = compile_plan(case.question, &schema(case.catalog));
        assert_eq!(
            print_plan(&plan).unwrap(),
            case.expected,
            "question: {:?}",
            case.question
        );
    }
}

#[test]
fn determinism_corpus_is_byte_identical_for_plans_and_refusals() {
    for case in golden_cases() {
        let schema = schema(case.catalog);
        let first = DeterministicCompiler.compile(case.question, &schema);
        let second = DeterministicCompiler.compile(case.question, &schema);
        assert_eq!(first, second, "question: {:?}", case.question);
        if let (Compiled::Plan(first), Compiled::Plan(second)) = (first, second) {
            assert_eq!(first.to_json().unwrap(), second.to_json().unwrap());
        }
    }
    let schema = people_schema();
    for question in refusal_questions() {
        assert_eq!(
            DeterministicCompiler.compile(question, &schema),
            DeterministicCompiler.compile(question, &schema),
            "question: {question:?}"
        );
    }
}

#[test]
fn punctuation_tokenization_corpus_is_exact_and_deterministic() {
    let schema = people_schema();
    let accepted = [
        (
            "who is ada?",
            "nodes(Person) as person | filter person.name = \"ada\"",
        ),
        (
            "who is ada!",
            "nodes(Person) as person | filter person.name = \"ada\"",
        ),
        ("people;", "nodes(Person) as person"),
        (
            "who is ada’s",
            "nodes(Person) as person | filter person.name = \"ada’s\"",
        ),
        (
            "who is 5people",
            "nodes(Person) as person | filter person.name = \"5people\"",
        ),
    ];
    for (question, expected) in accepted {
        let first = DeterministicCompiler.compile(question, &schema);
        let second = DeterministicCompiler.compile(question, &schema);
        assert_eq!(first, second, "question: {question:?}");
        let Compiled::Plan(plan) = first else {
            panic!("punctuation corpus question refused: {question:?}");
        };
        assert_eq!(
            print_plan(&plan).unwrap(),
            expected,
            "question: {question:?}"
        );
        assert_eq!(
            parse(expected).unwrap(),
            Parsed::Query(plan),
            "question: {question:?}"
        );
    }

    for (question, unknown) in [("people with score over .5", ".5"), ("who is \"ada", "is")] {
        let first = DeterministicCompiler.compile(question, &schema);
        let second = DeterministicCompiler.compile(question, &schema);
        assert_eq!(first, second, "question: {question:?}");
        let Compiled::NoParse(report) = first else {
            panic!("malformed punctuation corpus question compiled: {question:?}");
        };
        assert!(
            report
                .unrecognized
                .iter()
                .any(|token| token.token == unknown),
            "question: {question:?}: {report:?}"
        );
    }

    let geo_schema = SchemaSummary {
        node_tables: vec![NodeTableSummary {
            name: "Cafe".to_owned(),
            columns: vec![
                column("id", "Int64", true),
                column("name", "String", false),
                column("location", "GeoPoint", false),
            ],
        }],
        rel_tables: Vec::new(),
        classes: None,
        pins: Vec::new(),
    };
    let unterminated_geo = "cafes within 2 miles of geo(45.5, -122.6";
    let first = DeterministicCompiler.compile(unterminated_geo, &geo_schema);
    let second = DeterministicCompiler.compile(unterminated_geo, &geo_schema);
    assert_eq!(first, second);
    let Compiled::NoParse(report) = first else {
        panic!("unterminated geo literal compiled");
    };
    assert!(
        report
            .unrecognized
            .iter()
            .any(|token| token.token == "geo(45.5, -122.6")
    );
}

fn refusal_questions() -> [&'static str; 8] {
    [
        "knows of knows of knows of Ada",
        "people similar to Ada",
        "top 5 people by oldest",
        "top five people by age",
        "create node table Person",
        "people older than 30",
        "who does Ada know know",
        "total revenu of people",
    ]
}

fn unrecognized(report: &devondb_nl::NoParse) -> Vec<Ungrounded> {
    report.unrecognized.clone()
}

fn hints(report: &devondb_nl::NoParse) -> Vec<String> {
    report
        .nearest
        .iter()
        .map(|hint| hint.example.clone())
        .collect()
}

#[test]
fn refusal_corpus_reports_unknowns_suggestions_and_exact_hints() {
    let schema = people_schema();
    let Compiled::NoParse(older) = DeterministicCompiler.compile("people older than 30", &schema)
    else {
        panic!("out-of-scope adjective compiled");
    };
    assert_eq!(
        unrecognized(&older),
        vec![
            Ungrounded {
                token: "older".to_owned(),
                suggestion: Some("over".to_owned()),
            },
            Ungrounded {
                token: "than".to_owned(),
                suggestion: None,
            },
        ]
    );
    assert_eq!(
        hints(&older),
        vec![
            "top 30 people with <column> over 30 by <sort-column>",
            "how many people have <column> over 30",
            "people with <column> over 30",
        ]
    );

    assert_refusal(
        &schema,
        "knows of knows of knows of Ada",
        &[("knows", Some("multi-hop traversal has a hard 2-hop cap"))],
        &["knows of knows of Ada"],
    );
    assert_refusal(
        &schema,
        "people similar to Ada",
        &[(
            "people",
            Some("`Person` has no vector column to compare by"),
        )],
        &[
            "top <n> people similar to Ada",
            "people similar to Ada",
            "top <n> people with <column> over Ada by <sort-column>",
        ],
    );
    assert_refusal(
        &schema,
        "top 5 people by oldest",
        &[("oldest", None)],
        &[
            "top 5 people with <column> over 5 by <sort-column>",
            "how many people have <column> over 5",
            "people with <column> over 5",
        ],
    );
    assert_refusal(
        &schema,
        "top five people by age",
        &[("five", None)],
        &[
            "top <n> people with age over <value> by age",
            "how many people have age over <value>",
            "people with age over <value>",
        ],
    );
    assert_refusal(
        &schema,
        "create node table Person",
        &[("create", Some("greater")), ("node", None), ("table", None)],
        &[
            "top <n> Person with <column> over Person by <sort-column>",
            "how many Person have <column> over Person",
            "Person with <column> over Person",
        ],
    );
    assert_refusal(
        &schema,
        "who does Ada know know",
        &[("know", None)],
        &[
            "who does Ada Knows with <column> over Ada",
            "who Knows Ada with <column> over Ada",
            "who does Ada Knows",
        ],
    );
}

fn assert_refusal(
    schema: &SchemaSummary,
    question: &str,
    expected_unknown: &[(&str, Option<&str>)],
    expected_hints: &[&str],
) {
    let Compiled::NoParse(report) = DeterministicCompiler.compile(question, schema) else {
        panic!("out-of-scope question compiled: {question:?}");
    };
    let expected_unknown = expected_unknown
        .iter()
        .map(|(token, suggestion)| Ungrounded {
            token: (*token).to_owned(),
            suggestion: suggestion.map(str::to_owned),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        report.unrecognized, expected_unknown,
        "question: {question:?}"
    );
    assert_eq!(
        hints(&report),
        expected_hints
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "question: {question:?}"
    );
}

#[test]
fn ambiguity_corpus_lists_every_candidate_deterministically() {
    let schema = SchemaSummary {
        node_tables: vec![
            person(),
            NodeTableSummary {
                name: "People".to_owned(),
                columns: vec![column("id", "Int64", true), column("name", "String", false)],
            },
        ],
        rel_tables: Vec::new(),
        classes: None,
        pins: Vec::new(),
    };
    let Compiled::NoParse(report) = DeterministicCompiler.compile("list persons", &schema) else {
        panic!("plural ambiguity compiled");
    };
    assert_eq!(
        Compiled::NoParse(report.clone()),
        DeterministicCompiler.compile("list persons", &schema)
    );
    assert_eq!(
        unrecognized(&report),
        vec![Ungrounded {
            token: "persons".to_owned(),
            suggestion: Some("Person, People".to_owned()),
        }]
    );
    assert_eq!(
        report.nearest,
        vec![
            TemplateHint {
                example: "top <n> persons similar to persons".to_owned(),
            },
            TemplateHint {
                example: "persons similar to persons".to_owned(),
            },
            TemplateHint {
                example: "top <n> persons with <column> over <value> by <sort-column>".to_owned(),
            },
        ]
    );
}

#[test]
fn ontology_declared_names_verbs_and_labels_precede_derived_grounding() {
    let schema = ontology_schema();
    for (question, expected) in [
        ("human", "nodes(Person) as person"),
        ("humans", "nodes(Person) as person"),
        (
            "who follows ada",
            "nodes(Person) as person | filter person.handle = \"ada\" | expand Tracks in as other | project other.handle",
        ),
        (
            "who does ada get followed by",
            "nodes(Person) as person | filter person.handle = \"ada\" | expand Tracks in as other | project other.handle",
        ),
    ] {
        assert_eq!(
            print_plan(&compile_plan(question, &schema)).unwrap(),
            expected,
            "question: {question:?}"
        );
    }
}

#[test]
fn ontology_bare_names_list_tables_before_trying_entity_literals() {
    let schema = people_schema();
    for (question, expected) in [
        ("people", "nodes(Person) as person"),
        (
            "ada",
            "nodes(Person) as person | filter person.name = \"ada\"",
        ),
        // `person` can be an open entity literal too, but an exact catalog
        // table is enumerable and therefore wins the ordered-template tie.
        ("person", "nodes(Person) as person"),
    ] {
        assert_eq!(
            print_plan(&compile_plan(question, &schema)).unwrap(),
            expected,
            "question: {question:?}"
        );
    }
}

#[test]
fn ontology_declared_plural_collision_with_exact_table_stays_ambiguous() {
    let schema = SchemaSummary {
        node_tables: vec![
            person(),
            NodeTableSummary {
                name: "Contacts".to_owned(),
                columns: vec![column("id", "Int64", true), column("name", "String", false)],
            },
        ],
        rel_tables: Vec::new(),
        classes: Some(ClassesSummary {
            interfaces: Vec::new(),
            node_classes: vec![
                NodeClassSummary {
                    table: "Person".to_owned(),
                    display: "Person".to_owned(),
                    plural: Some("contacts".to_owned()),
                    label: Some("name".to_owned()),
                    summary: Vec::new(),
                    color: None,
                    description: None,
                    implements: Vec::new(),
                },
                NodeClassSummary {
                    table: "Contacts".to_owned(),
                    display: "Contacts".to_owned(),
                    plural: None,
                    label: Some("name".to_owned()),
                    summary: Vec::new(),
                    color: None,
                    description: None,
                    implements: Vec::new(),
                },
            ],
            rel_classes: Vec::new(),
        }),
        pins: Vec::new(),
    };
    let Compiled::NoParse(report) = DeterministicCompiler.compile("contacts", &schema) else {
        panic!("declared plural collision compiled");
    };
    assert_eq!(
        report.unrecognized,
        vec![Ungrounded {
            token: "contacts".to_owned(),
            suggestion: Some("Contacts, Person".to_owned()),
        }]
    );
}

#[test]
fn ontology_original_multi_token_inputs_keep_exact_refusals() {
    let nearest = vec![
        TemplateHint {
            example: "top <n> persons with <column> over <value> by <sort-column>".to_owned(),
        },
        TemplateHint {
            example: "how many persons have <column> over <value>".to_owned(),
        },
        TemplateHint {
            example: "persons with <column> over <value>".to_owned(),
        },
    ];
    for (question, unrecognized) in [
        (
            "persons as nodes",
            vec![
                Ungrounded {
                    token: "as".to_owned(),
                    suggestion: Some("at".to_owned()),
                },
                Ungrounded {
                    token: "nodes".to_owned(),
                    suggestion: Some("does".to_owned()),
                },
            ],
        ),
        (
            "nodes as persons",
            vec![
                Ungrounded {
                    token: "nodes".to_owned(),
                    suggestion: Some("does".to_owned()),
                },
                Ungrounded {
                    token: "as".to_owned(),
                    suggestion: Some("at".to_owned()),
                },
            ],
        ),
    ] {
        assert_eq!(
            DeterministicCompiler.compile(question, &people_schema()),
            Compiled::NoParse(NoParse {
                recognized: vec![Grounded {
                    token: "persons".to_owned(),
                    target: "Person".to_owned(),
                }],
                unrecognized,
                nearest: nearest.clone(),
            }),
            "question: {question:?}"
        );
    }
}

struct TestDatabase {
    database: Option<Database>,
    directory: PathBuf,
}

impl TestDatabase {
    fn new(ddl: &[&str]) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "devondb-nl-corpus-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let mut database = Database::create(directory.join("corpus.devondb"), 4096).unwrap();
        for statement in ddl {
            let Parsed::Statement(envelope) = parse(statement).unwrap() else {
                panic!("DDL parsed as a query: {statement}");
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

fn people_database() -> TestDatabase {
    TestDatabase::new(&[
        "create node table Person (id Int64 primary key, name String, age Int64, score Float64)",
        "create rel table Knows from Person to Person",
        "insert into Person values (1, \"Ada\", 36, 9.5), (2, \"Grace\", 29, 7.0), (3, \"Linus\", 45, 2.0), (4, \"Bob\", 18, 100.0)",
        "insert rel into Knows values (1 -> 2), (1 -> 3), (2 -> 1)",
    ])
}

fn multi_hop_database() -> TestDatabase {
    TestDatabase::new(&[
        "create node table Person (id Int64 primary key, name String, age Int64, score Float64)",
        "create rel table Knows from Person to Person",
        "create class for Knows (verb \"friends\")",
        "insert into Person values (1, \"Ada\", 36, 9.5), (2, \"Grace\", 29, 7.0), (3, \"Linus\", 45, 2.0), (4, \"Bob\", 18, 100.0)",
        "insert rel into Knows values (1 -> 2), (1 -> 3), (2 -> 1), (2 -> 4)",
    ])
}

fn multi_hop_composed_database() -> TestDatabase {
    TestDatabase::new(&[
        "create node table Person (id Int64 primary key, name String)",
        "create node table Company (id Int64 primary key, name String)",
        "create node table City (id Int64 primary key, name String)",
        "create rel table Works_At from Person to Company",
        "create rel table Located_In from Company to City",
        "create class for Works_At (verb \"works at\", inverse \"employs\")",
        "create class for Located_In (verb \"located in\", inverse \"hosts\")",
        "insert into Person values (1, \"Ada\")",
        "insert into Company values (10, \"Analytical Engines\")",
        "insert into City values (100, \"London\")",
        "insert rel into Works_At values (1 -> 10)",
        "insert rel into Located_In values (10 -> 100)",
    ])
}

#[test]
fn multi_hop_golden_corpus_compiles_to_canonical_text_and_executes_rows() {
    let mut fixture = multi_hop_database();
    let database = fixture.database();
    let expected = "nodes(Person) as person | filter person.name = \"Ada\" | expand Knows out as hop1 | expand Knows out as other | project other.name";
    for question in [
        "friends of friends of ada",
        "who do the people ada knows know",
        "friends of knows of Ada",
    ] {
        let plan = compile_database_plan(question, database);
        let repeated = compile_database_plan(question, database);
        assert_eq!(plan, repeated, "question: {question:?}");
        assert_eq!(
            plan.to_json().unwrap(),
            repeated.to_json().unwrap(),
            "question: {question:?}"
        );
        assert_eq!(
            print_plan(&plan).unwrap(),
            expected,
            "question: {question:?}"
        );
        let result = database.run(&plan).unwrap();
        assert_eq!(result.columns, ["other.name"]);
        assert_eq!(
            format!("{:?}", result.rows),
            "[[String(\"Ada\")], [String(\"Bob\")]]",
            "question: {question:?}"
        );
    }
}

#[test]
fn multi_hop_composes_distinct_endpoints_and_inverse_verbs() {
    let mut fixture = multi_hop_composed_database();
    let database = fixture.database();
    for (question, expected_plan, expected_rows) in [
        (
            "located in of works at of ada",
            "nodes(Person) as person | filter person.name = \"Ada\" | expand Works_At out as hop1 | expand Located_In out as other | project other.name",
            "[[String(\"London\")]]",
        ),
        (
            "employs of hosts of london",
            "nodes(City) as city | filter city.name = \"London\" | expand Located_In in as hop1 | expand Works_At in as other | project other.name",
            "[[String(\"Ada\")]]",
        ),
    ] {
        let plan = compile_database_plan(question, database);
        assert_eq!(print_plan(&plan).unwrap(), expected_plan);
        let result = database.run(&plan).unwrap();
        assert_eq!(result.columns, ["other.name"]);
        assert_eq!(format!("{:?}", result.rows), expected_rows);
    }
}

#[test]
fn multi_hop_three_hop_refusal_names_cap_and_nearest_two_hop_template() {
    let schema = multi_hop_schema();
    for (question, nearest) in [
        (
            "friends of friends of friends of Ada",
            "friends of friends of Ada",
        ),
        (
            "who do the people Ada knows know know",
            "who do the people Ada knows know",
        ),
    ] {
        let Compiled::NoParse(report) = DeterministicCompiler.compile(question, &schema) else {
            panic!("three-hop traversal compiled: {question:?}");
        };
        assert_eq!(report.unrecognized.len(), 1, "question: {question:?}");
        assert_eq!(
            report.unrecognized[0].suggestion.as_deref(),
            Some("multi-hop traversal has a hard 2-hop cap"),
            "question: {question:?}"
        );
        assert_eq!(
            report.nearest,
            [TemplateHint {
                example: nearest.to_owned()
            }]
        );
    }
}

#[test]
fn multi_hop_templates_remain_live_in_version_nine() {
    assert_eq!(NL_VERSION, 10);
}

#[test]
fn entity_value_grounding_emits_stored_spelling_and_executes_the_same_rows() {
    let mut fixture = people_database();
    let database = fixture.database();
    let lower = compile_database_plan("who does ada know", database);
    assert_eq!(
        print_plan(&lower).unwrap(),
        "nodes(Person) as person | filter person.name = \"Ada\" | expand Knows out as other | project other.name"
    );
    let lower_rows = database.run(&lower).unwrap();

    let stored = compile_database_plan("who does Ada know", database);
    let stored_rows = database.run(&stored).unwrap();
    assert_eq!(lower_rows, stored_rows);
    assert_eq!(
        format!("{:?}", lower_rows.rows),
        "[[String(\"Grace\")], [String(\"Linus\")]]"
    );
}

#[test]
fn entity_value_grounding_refuses_ambiguous_and_unknown_stored_values() {
    let mut ambiguous = TestDatabase::new(&[
        "create node table Person (id Int64 primary key, name String)",
        "create rel table Knows from Person to Person",
        "insert into Person values (1, \"Ada\"), (2, \"ADA\")",
    ]);
    let report = within_refusal("who does ada know", ambiguous.database());
    assert_eq!(
        report.unrecognized,
        vec![Ungrounded {
            token: "ada".to_owned(),
            suggestion: Some("Ada, ADA".to_owned()),
        }]
    );

    let mut fixture = people_database();
    let report = within_refusal("who does Ado know", fixture.database());
    assert_eq!(
        report.unrecognized,
        vec![Ungrounded {
            token: "Ado".to_owned(),
            suggestion: Some("Ada".to_owned()),
        }]
    );
}

#[test]
fn entity_value_grounding_without_a_database_preserves_the_typed_spelling() {
    let plan = compile_plan("who does ada know", &people_schema());
    assert_eq!(
        print_plan(&plan).unwrap(),
        "nodes(Person) as person | filter person.name = \"ada\" | expand Knows out as other | project other.name"
    );
}

fn within_database() -> TestDatabase {
    TestDatabase::new(&[
        "create node table Cafe (id Int64 primary key, name String, location GeoPoint, rating Float64)",
        "create node table City (id Int64 primary key, name String, location GeoPoint)",
        "create rel table Knows from Cafe to Cafe",
        "insert into Cafe values (1, \"Downtown\", geo(45.5152, -122.6784), 4.8), (2, \"Near\", geo(45.516, -122.6784), 3.5), (3, \"Far\", geo(45.55, -122.6784), 5.0)",
        "insert into City values (10, \"Portland\", geo(45.5152, -122.6784))",
    ])
}

#[test]
fn within_corpus_compiles_deterministically_and_executes_when_available() {
    assert_eq!(NL_VERSION, 10);
    let mut fixture = within_database();
    let database = fixture.database();
    for case in within_golden_cases() {
        let first = compile_database_plan(case.question, database);
        let second = compile_database_plan(case.question, database);
        assert_eq!(first, second, "question: {:?}", case.question);
        assert_eq!(first.to_json().unwrap(), second.to_json().unwrap());
        assert_eq!(
            print_plan(&first).unwrap(),
            case.expected,
            "question: {:?}",
            case.question
        );
        let result = database.run(&first).unwrap_or_else(|error| {
            panic!("within execution failed for {:?}: {error}", case.question)
        });
        assert_eq!(result.columns, case.expected_columns);
        let first_values = result
            .rows
            .iter()
            .filter_map(|row| row.first().map(ToString::to_string))
            .collect::<Vec<_>>();
        assert_eq!(first_values, case.expected_first_values);
    }
}

#[test]
fn within_units_use_every_pinned_spelling_and_exact_constant() {
    let mut fixture = within_database();
    let database = fixture.database();
    for (unit, meters) in [
        ("mile", "3218.688"),
        ("miles", "3218.688"),
        ("mi", "3218.688"),
        ("kilometer", "2000.0"),
        ("kilometers", "2000.0"),
        ("km", "2000.0"),
        ("meter", "2.0"),
        ("meters", "2.0"),
        ("m", "2.0"),
        ("foot", "0.6096"),
        ("feet", "0.6096"),
        ("ft", "0.6096"),
    ] {
        let question = format!("cafes within 2 {unit} of geo(45.5152, -122.6784)");
        let plan = compile_database_plan(&question, database);
        assert_eq!(
            print_plan(&plan).unwrap(),
            format!("within(Cafe.location, geo(45.5152, -122.6784), {meters})"),
            "unit: {unit}"
        );
    }
}

#[test]
fn within_bare_geo_needs_no_row_grounding_and_canonicalizes_the_literal() {
    let mut fixture = within_database();
    let schema = fixture.database().schema_summary();
    let plan = compile_plan("cafes within 1 km of geo(90, 180)", &schema);
    assert_eq!(
        print_plan(&plan).unwrap(),
        "within(Cafe.location, geo(90.0, 0.0), 1000.0)"
    );
}

fn within_refusal(question: &str, database: &mut Database) -> NoParse {
    let Compiled::NoParse(report) = DeterministicCompiler.compile_with_database(question, database)
    else {
        panic!("within refusal compiled: {question:?}");
    };
    report
}

#[test]
fn within_refuses_unknown_units_missing_and_ambiguous_places_and_non_source_use() {
    let mut fixture = within_database();
    let database = fixture.database();

    let unknown = within_refusal("cafes within 2 yards of Portland", database);
    assert_eq!(
        unknown.unrecognized,
        vec![Ungrounded {
            token: "yards".to_owned(),
            suggestion: None,
        }]
    );
    assert_eq!(
        unknown.nearest.first().map(|hint| hint.example.as_str()),
        Some("cafes within 2 miles of Portland")
    );

    let missing = within_refusal("cafes within 2 miles of Nowhere", database);
    assert_eq!(
        missing.unrecognized,
        vec![Ungrounded {
            token: "Nowhere".to_owned(),
            suggestion: None,
        }]
    );

    assert!(
        matches!(
            DeterministicCompiler.compile("who does Downtown know", &database.schema_summary()),
            Compiled::Plan(_)
        ),
        "the traversal prefix must ground before placement is refused"
    );
    let non_source = within_refusal(
        "who does Downtown know within 2 miles of Portland",
        database,
    );
    assert_eq!(
        non_source.unrecognized,
        vec![Ungrounded {
            token: "within".to_owned(),
            suggestion: Some("use `<table> within <number> <unit> of <place>`".to_owned()),
        }]
    );
    assert_eq!(
        non_source.nearest.first().map(|hint| hint.example.as_str()),
        Some("<table> within 2 miles of Portland")
    );

    let mut ambiguous = TestDatabase::new(&[
        "create node table Cafe (id Int64 primary key, name String, location GeoPoint)",
        "create node table City (id Int64 primary key, name String, location GeoPoint)",
        "create node table Landmark (id Int64 primary key, name String, point GeoPoint)",
        "insert into City values (10, \"Portland\", geo(45.5152, -122.6784))",
        "insert into Landmark values (20, \"Portland\", geo(45.52, -122.68))",
    ]);
    let ambiguous = within_refusal("cafes within 2 miles of Portland", ambiguous.database());
    assert_eq!(
        ambiguous.unrecognized,
        vec![Ungrounded {
            token: "Portland".to_owned(),
            suggestion: Some("City(10), Landmark(20)".to_owned()),
        }]
    );
}

#[test]
fn within_derives_locatable_from_exactly_one_geo_column() {
    let schema = SchemaSummary {
        node_tables: vec![NodeTableSummary {
            name: "Region".to_owned(),
            columns: vec![
                column("id", "Int64", true),
                column("center", "GeoPoint", false),
                column("entrance", "GeoPoint", false),
            ],
        }],
        rel_tables: Vec::new(),
        classes: None,
        pins: Vec::new(),
    };
    let Compiled::NoParse(report) =
        DeterministicCompiler.compile("regions within 2 km of geo(45.5152, -122.6784)", &schema)
    else {
        panic!("table with two GeoPoint columns was treated as Locatable");
    };
    assert_eq!(
        report.unrecognized,
        vec![Ungrounded {
            token: "within".to_owned(),
            suggestion: Some(
                "`Region` is not Locatable: expected exactly one GeoPoint column".to_owned()
            ),
        }]
    );
}

struct SimilarGoldenCase {
    question: &'static str,
    expected: &'static str,
}

/// The § 18 golden rows: default k, explicit N, and `top N`, each pinned to
/// its exact canonical plan text.
fn similar_golden_cases() -> [SimilarGoldenCase; 3] {
    [
        SimilarGoldenCase {
            question: "people similar to ada",
            expected: "knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = \"Ada\" | project anchor.embedding), 10, cosine)",
        },
        SimilarGoldenCase {
            question: "3 people similar to ada",
            expected: "knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = \"Ada\" | project anchor.embedding), 3, cosine)",
        },
        SimilarGoldenCase {
            question: "top 3 people similar to ada",
            expected: "knn(Person.embedding, scalar(nodes(Person) as anchor | filter anchor.name = \"Ada\" | project anchor.embedding), 3, cosine)",
        },
    ]
}

/// Five people with one-hot Vector(4) embeddings, so each anchor is the
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
    let ddl = ddl.iter().map(String::as_str).collect::<Vec<_>>();
    TestDatabase::new(&ddl)
}

#[test]
fn similar_to_corpus_compiles_deterministically_and_executes_ranked_rows() {
    assert_eq!(NL_VERSION, 10);
    let mut fixture = embedded_database();
    let database = fixture.database();
    for case in similar_golden_cases() {
        let first = compile_database_plan(case.question, database);
        let second = compile_database_plan(case.question, database);
        assert_eq!(first, second, "question: {:?}", case.question);
        assert_eq!(first.to_json().unwrap(), second.to_json().unwrap());
        assert_eq!(
            print_plan(&first).unwrap(),
            case.expected,
            "question: {:?}",
            case.question
        );
        let result = database.run(&first).unwrap_or_else(|error| {
            panic!(
                "similar-to execution failed for {:?}: {error}",
                case.question
            )
        });
        assert_eq!(
            result.columns,
            ["Person.id", "Person.name", "Person.embedding", "distance"],
            "question: {:?}",
            case.question
        );
        assert_eq!(
            result.rows[0][1],
            devondb::Value::String("Ada".into()),
            "question: {:?}",
            case.question
        );
        assert_eq!(
            result.rows[0][3],
            devondb::Value::Float64(0.0),
            "question: {:?}",
            case.question
        );
    }
}

#[test]
fn validator_round_trip_corpus_runs_every_plan_against_its_schema() {
    let mut people = TestDatabase::new(&[
        "create node table Person (id Int64 primary key, name String, age Int64, score Float64)",
        "create rel table Knows from Person to Person",
        "insert into Person values (1, \"Ada\", 36, 9.5), (2, \"Grace\", 29, 7.0), (3, \"Linus\", 45, 2.0), (4, \"Bob\", 18, 100.0)",
        "insert rel into Knows values (1 -> 2), (1 -> 3), (2 -> 1)",
    ]);
    let mut places = TestDatabase::new(&[
        "create node table Person (id Int64 primary key, name String, age Int64, score Float64)",
        "create node table City (id Int64 primary key, name String, population Int64)",
        "create rel table Lives_In from Person to City",
        "insert into Person values (1, \"Ada\", 36, 9.5), (2, \"Grace\", 29, 7.0), (3, \"Linus\", 45, 2.0), (4, \"Bob\", 18, 100.0)",
        "insert into City values (10, \"Boston\", 650000), (11, \"London\", 9000000)",
        "insert rel into Lives_In values (1 -> 10), (2 -> 11)",
    ]);
    for case in golden_cases() {
        let database = match case.catalog {
            Catalog::People => people.database(),
            Catalog::Places => places.database(),
        };
        let summary = database.schema_summary();
        let plan = compile_plan(case.question, &summary);
        let result = database
            .run(&plan)
            .unwrap_or_else(|error| panic!("validator rejected {:?}: {error}", case.question));
        let (expected_columns, expected_rows) = expected_execution(case.question);
        assert_eq!(
            result.columns, expected_columns,
            "question: {:?}",
            case.question
        );
        assert_eq!(
            format!("{:?}", result.rows),
            expected_rows,
            "question: {:?}",
            case.question
        );
    }
}
