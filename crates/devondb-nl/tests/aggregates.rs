//! NL_VERSION 7 aggregate family corpus (`docs/NL.md` § 17): canonical text
//! AND executed rows on a seeded Sale/Person fixture, refusal reports, the
//! `by`-group vs Q6-sort disambiguation, and the determinism property.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use devondb::text::parser::{Parsed, parse};
use devondb::text::printer::print_plan;
use devondb::{ColumnSummary, Database, NodeTableSummary, SchemaSummary};
use devondb_nl::{
    Compiled, DeterministicCompiler, IntentCompiler, NL_VERSION, NoParse, Ungrounded,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.to_owned(),
        ty: ty.to_owned(),
        primary_key,
    }
}

fn sale() -> NodeTableSummary {
    NodeTableSummary {
        name: "Sale".to_owned(),
        columns: vec![
            column("id", "Int64", true),
            column("revenue", "Int64", false),
            column("price", "Float64", false),
            column("amount", "Decimal(10,2)", false),
            column("region", "String", false),
            column("year", "Int64", false),
        ],
    }
}

fn person() -> NodeTableSummary {
    NodeTableSummary {
        name: "Person".to_owned(),
        columns: vec![
            column("id", "Int64", true),
            column("name", "String", false),
            column("city", "String", false),
        ],
    }
}

fn sales_schema() -> SchemaSummary {
    SchemaSummary {
        node_tables: vec![sale(), person()],
        rel_tables: Vec::new(),
        classes: None,
        pins: Vec::new(),
    }
}

struct TestDatabase {
    database: Option<Database>,
    directory: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "devondb-nl-aggregates-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let mut database = Database::create(directory.join("aggregates.devondb"), 4096).unwrap();
        for statement in [
            "create node table Sale (id Int64 primary key, revenue Int64, price Float64, amount Decimal(10,2), region String, year Int64)",
            "create node table Person (id Int64 primary key, name String, city String)",
            "insert into Sale values (1, 100, 9.5, decimal(\"10.50\"), \"West\", 2023), (2, 200, 7.0, decimal(\"20.00\"), \"East\", 2024), (3, 300, 2.5, decimal(\"30.25\"), \"West\", 2024), (4, 150, 4.0, decimal(\"15.00\"), \"East\", 2025)",
            "insert into Person values (1, \"Ada\", \"Portland\"), (2, \"Grace\", \"London\"), (3, \"Linus\", \"Portland\")",
        ] {
            let Parsed::Statement(envelope) = parse(statement).unwrap() else {
                panic!("fixture input parsed as a query: {statement}");
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

struct AggregateCase {
    question: &'static str,
    expected: &'static str,
    columns: &'static [&'static str],
    rows: &'static [&'static [&'static str]],
}

const TOTAL: &str = "nodes(Sale) as sale | aggregate sum(sale.revenue) as total";
const TOTAL_BY: &str = "nodes(Sale) as sale | aggregate sum(sale.revenue) as total by sale.region";
const TOTAL_FILTERED: &str =
    "nodes(Sale) as sale | filter sale.year > 2023 | aggregate sum(sale.revenue) as total";
const AVERAGE: &str = "nodes(Sale) as sale | aggregate avg(sale.price) as average";
const AVERAGE_BY: &str =
    "nodes(Sale) as sale | aggregate avg(sale.price) as average by sale.region";
const HIGHEST: &str = "nodes(Sale) as sale | aggregate max(sale.revenue) as highest";
const LOWEST: &str = "nodes(Sale) as sale | aggregate min(sale.revenue) as lowest";

fn aggregate_cases() -> Vec<AggregateCase> {
    vec![
        // total/sum, head-first.
        AggregateCase {
            question: "total revenue of sales",
            expected: TOTAL,
            columns: &["total"],
            rows: &[&["750"]],
        },
        AggregateCase {
            question: "sum of revenue of sales",
            expected: TOTAL,
            columns: &["total"],
            rows: &[&["750"]],
        },
        AggregateCase {
            question: "total revenue for sales",
            expected: TOTAL,
            columns: &["total"],
            rows: &[&["750"]],
        },
        AggregateCase {
            question: "total revenue of sales by region",
            expected: TOTAL_BY,
            columns: &["sale.region", "total"],
            rows: &[&["\"East\"", "350"], &["\"West\"", "400"]],
        },
        AggregateCase {
            question: "total revenue of sales with year over 2023",
            expected: TOTAL_FILTERED,
            columns: &["total"],
            rows: &[&["650"]],
        },
        AggregateCase {
            question: "total revenue of sales with year over 2023 by region",
            expected: "nodes(Sale) as sale | filter sale.year > 2023 | aggregate sum(sale.revenue) as total by sale.region",
            columns: &["sale.region", "total"],
            rows: &[&["\"East\"", "350"], &["\"West\"", "300"]],
        },
        AggregateCase {
            question: "total revenue of sales with year over 2023 and price under 5",
            expected: "nodes(Sale) as sale | filter sale.year > 2023 and sale.price < 5 | aggregate sum(sale.revenue) as total",
            columns: &["total"],
            rows: &[&["450"]],
        },
        // average/mean, head-first.
        AggregateCase {
            question: "average price of sales",
            expected: AVERAGE,
            columns: &["average"],
            rows: &[&["5.75"]],
        },
        AggregateCase {
            question: "mean price of sales",
            expected: AVERAGE,
            columns: &["average"],
            rows: &[&["5.75"]],
        },
        AggregateCase {
            question: "average price of sales by region",
            expected: AVERAGE_BY,
            columns: &["sale.region", "average"],
            rows: &[&["\"East\"", "5.5"], &["\"West\"", "6"]],
        },
        AggregateCase {
            question: "average revenue of sales with year under 2025 by region",
            expected: "nodes(Sale) as sale | filter sale.year < 2025 | aggregate avg(sale.revenue) as average by sale.region",
            columns: &["sale.region", "average"],
            rows: &[&["\"East\"", "200"], &["\"West\"", "200"]],
        },
        // highest/maximum/largest/max, head-first.
        AggregateCase {
            question: "highest revenue of sales",
            expected: HIGHEST,
            columns: &["highest"],
            rows: &[&["300"]],
        },
        AggregateCase {
            question: "maximum revenue of sales",
            expected: HIGHEST,
            columns: &["highest"],
            rows: &[&["300"]],
        },
        AggregateCase {
            question: "largest revenue of sales",
            expected: HIGHEST,
            columns: &["highest"],
            rows: &[&["300"]],
        },
        AggregateCase {
            question: "max revenue of sales",
            expected: HIGHEST,
            columns: &["highest"],
            rows: &[&["300"]],
        },
        AggregateCase {
            question: "highest revenue of sales by region",
            expected: "nodes(Sale) as sale | aggregate max(sale.revenue) as highest by sale.region",
            columns: &["sale.region", "highest"],
            rows: &[&["\"East\"", "200"], &["\"West\"", "300"]],
        },
        // lowest/minimum/smallest/min, head-first.
        AggregateCase {
            question: "lowest revenue of sales",
            expected: LOWEST,
            columns: &["lowest"],
            rows: &[&["100"]],
        },
        AggregateCase {
            question: "minimum revenue of sales",
            expected: LOWEST,
            columns: &["lowest"],
            rows: &[&["100"]],
        },
        AggregateCase {
            question: "min revenue of sales",
            expected: LOWEST,
            columns: &["lowest"],
            rows: &[&["100"]],
        },
        AggregateCase {
            question: "smallest revenue of sales",
            expected: LOWEST,
            columns: &["lowest"],
            rows: &[&["100"]],
        },
        AggregateCase {
            question: "lowest revenue of sales by region",
            expected: "nodes(Sale) as sale | aggregate min(sale.revenue) as lowest by sale.region",
            columns: &["sale.region", "lowest"],
            rows: &[&["\"East\"", "150"], &["\"West\"", "100"]],
        },
        // Table-first head position.
        AggregateCase {
            question: "sales total revenue",
            expected: TOTAL,
            columns: &["total"],
            rows: &[&["750"]],
        },
        AggregateCase {
            question: "sales total revenue by region",
            expected: TOTAL_BY,
            columns: &["sale.region", "total"],
            rows: &[&["\"East\"", "350"], &["\"West\"", "400"]],
        },
        AggregateCase {
            question: "sales average price",
            expected: AVERAGE,
            columns: &["average"],
            rows: &[&["5.75"]],
        },
        AggregateCase {
            question: "sales with year over 2023 total revenue",
            expected: TOTAL_FILTERED,
            columns: &["total"],
            rows: &[&["650"]],
        },
        // Count per group (extends Q5).
        AggregateCase {
            question: "how many sales by region",
            expected: "nodes(Sale) as sale | aggregate count(sale.id) as `count` by sale.region",
            columns: &["sale.region", "count"],
            rows: &[&["\"East\"", "2"], &["\"West\"", "2"]],
        },
        AggregateCase {
            question: "how many sales with year over 2023 by region",
            expected: "nodes(Sale) as sale | filter sale.year > 2023 | aggregate count(sale.id) as `count` by sale.region",
            columns: &["sale.region", "count"],
            rows: &[&["\"East\"", "2"], &["\"West\"", "1"]],
        },
        AggregateCase {
            question: "how many people by city",
            expected: "nodes(Person) as person | aggregate count(person.id) as `count` by person.city",
            columns: &["person.city", "count"],
            rows: &[&["\"London\"", "1"], &["\"Portland\"", "2"]],
        },
        // Decimal(10,2): sum/min/max are exact.
        AggregateCase {
            question: "total amount of sales",
            expected: "nodes(Sale) as sale | aggregate sum(sale.amount) as total",
            columns: &["total"],
            rows: &[&["decimal(\"75.75\")"]],
        },
        AggregateCase {
            question: "highest amount of sales",
            expected: "nodes(Sale) as sale | aggregate max(sale.amount) as highest",
            columns: &["highest"],
            rows: &[&["decimal(\"30.25\")"]],
        },
        AggregateCase {
            question: "lowest amount of sales by region",
            expected: "nodes(Sale) as sale | aggregate min(sale.amount) as lowest by sale.region",
            columns: &["sale.region", "lowest"],
            rows: &[
                &["\"East\"", "decimal(\"15.00\")"],
                &["\"West\"", "decimal(\"10.50\")"],
            ],
        },
    ]
}

fn compile_plan(question: &str, schema: &SchemaSummary) -> devondb::Plan {
    match DeterministicCompiler.compile(question, schema) {
        Compiled::Plan(plan) => plan,
        Compiled::NoParse(report) => panic!("question refused: {question:?}: {report:?}"),
    }
}

fn display_rows(result: &devondb::QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|row| row.iter().map(ToString::to_string).collect())
        .collect()
}

#[test]
fn aggregate_golden_corpus_pins_canonical_text_and_executed_rows() {
    assert_eq!(NL_VERSION, 10);
    let schema = sales_schema();
    let mut fixture = TestDatabase::new();
    let database = fixture.database();
    for case in aggregate_cases() {
        let plan = compile_plan(case.question, &schema);
        assert_eq!(
            print_plan(&plan).unwrap(),
            case.expected,
            "question: {:?}",
            case.question
        );
        let result = database
            .run(&plan)
            .unwrap_or_else(|error| panic!("execution failed for {:?}: {error}", case.question));
        assert_eq!(
            result.columns, case.columns,
            "question: {:?}",
            case.question
        );
        assert_eq!(
            display_rows(&result),
            case.rows,
            "question: {:?}",
            case.question
        );
    }
}

#[test]
fn by_after_an_aggregate_head_groups_while_top_keeps_q6_sort() {
    let schema = sales_schema();
    // Q6 stays Q6: `by` under a `top` head is the sort key, never a group.
    let top = compile_plan("top 5 sales by revenue", &schema);
    assert_eq!(
        print_plan(&top).unwrap(),
        "nodes(Sale) as sale | sort sale.revenue desc | limit 5"
    );
    let bottom = compile_plan("bottom 2 sales by region", &schema);
    assert_eq!(
        print_plan(&bottom).unwrap(),
        "nodes(Sale) as sale | sort sale.region | limit 2"
    );
    // The same `by <column>` tail under an aggregate head is GROUP.
    let group = compile_plan("total revenue of sales by region", &schema);
    assert_eq!(print_plan(&group).unwrap(), TOTAL_BY);

    let mut fixture = TestDatabase::new();
    let database = fixture.database();
    let result = database.run(&top).unwrap();
    let ids = result
        .rows
        .iter()
        .map(|row| row[0].to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["3", "2", "4", "1"]);
}

#[test]
fn aggregate_refusals_report_exact_unrecognized_tokens_and_hints() {
    let schema = sales_schema();
    let assert_refusal =
        |question: &str, expected_unknown: &[(&str, Option<&str>)], expected_hints: &[&str]| {
            let Compiled::NoParse(report) = DeterministicCompiler.compile(question, &schema) else {
                panic!("refusal case compiled: {question:?}");
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
            let hints = report
                .nearest
                .iter()
                .map(|hint| hint.example.as_str())
                .collect::<Vec<_>>();
            assert_eq!(hints, expected_hints, "question: {question:?}");
        };

    // Unknown aggregate column: did_you_mean plus the instantiated hint.
    assert_refusal(
        "total revenu of sales",
        &[("revenu", Some("revenue"))],
        &[
            "total revenue of sales",
            "sales total revenue",
            "how many sales by revenue",
        ],
    );
    // Unknown source table.
    assert_refusal(
        "total revenue of widgets",
        &[("widgets", None)],
        &[
            "total revenue of <table>",
            "<table> total revenue",
            "how many <table> by revenue",
        ],
    );
    // Missing <column> after a head.
    assert_refusal(
        "total for sales",
        &[(
            "total",
            Some("aggregate head requires a column to aggregate"),
        )],
        &[
            "total <column> of sales",
            "sales total <column>",
            "how many sales by <column>",
        ],
    );
    // Group column equal to the aggregated column.
    assert_refusal(
        "total revenue of sales by revenue",
        &[(
            "revenue",
            Some("group column must differ from the aggregated column"),
        )],
        &[
            "total revenue of sales",
            "sales total revenue",
            "how many sales by revenue",
        ],
    );
    // Unknown group column.
    assert_refusal(
        "total revenue of sales by regin",
        &[("regin", Some("region"))],
        &[
            "total revenue of sales",
            "sales total revenue",
            "how many sales by revenue",
        ],
    );
    // Count per group on the counted (primary-key) column.
    assert_refusal(
        "how many sales by id",
        &[(
            "id",
            Some("group column must differ from the aggregated column"),
        )],
        &[
            "total id of sales",
            "sales total id",
            "how many sales by id",
        ],
    );
    // A trailing `by` with no group token consumes nothing.
    let Compiled::NoParse(report) = DeterministicCompiler.compile("how many sales by", &schema)
    else {
        panic!("dangling `by` compiled");
    };
    assert_eq!(report.unrecognized, Vec::new());
    // Table-first with a missing column. The grounded group column fills the
    // hint's column slot per the § 7 grounded-slots mechanism.
    assert_refusal(
        "sales total by region",
        &[(
            "total",
            Some("aggregate head requires a column to aggregate"),
        )],
        &[
            "total region of sales",
            "sales total region",
            "how many sales by region",
        ],
    );
}

#[test]
fn aggregate_ambiguous_column_refusal_lists_every_candidate() {
    let mut schema = sales_schema();
    schema.node_tables[0]
        .columns
        .push(column("Revenue", "Int64", false));
    let Compiled::NoParse(report) =
        DeterministicCompiler.compile("total revenue of sales", &schema)
    else {
        panic!("fold-ambiguous column compiled");
    };
    assert_eq!(
        report.unrecognized,
        vec![Ungrounded {
            token: "revenue".to_owned(),
            suggestion: Some("revenue, Revenue".to_owned()),
        }]
    );
    assert_eq!(
        report.nearest.first().map(|hint| hint.example.as_str()),
        Some("total revenue of sales")
    );
}

#[test]
fn aggregate_column_typing_is_the_engines_law() {
    let schema = sales_schema();
    let mut fixture = TestDatabase::new();
    let database = fixture.database();

    // The compiler emits; the engine refuses `sum` over String.
    let plan = compile_plan("total region of sales", &schema);
    assert_eq!(
        print_plan(&plan).unwrap(),
        "nodes(Sale) as sale | aggregate sum(sale.region) as total"
    );
    let error = database.run(&plan).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("operator `sum` requires a numeric operand; got String"),
        "unexpected error: {error}"
    );

    // The compiler emits; the engine refuses `avg` over Decimal with the
    // exact refuse-never-round message.
    let plan = compile_plan("average amount of sales", &schema);
    assert_eq!(
        print_plan(&plan).unwrap(),
        "nodes(Sale) as sale | aggregate avg(sale.amount) as average"
    );
    let error = database.run(&plan).unwrap_err();
    assert!(
        error.to_string().contains(
            "avg over Decimal would round; compute sum and count and divide consumer-side"
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn aggregate_compilation_is_deterministic_across_shuffled_runs() {
    let schema = sales_schema();
    let questions = aggregate_cases()
        .iter()
        .map(|case| case.question)
        .chain([
            "total revenu of sales",
            "total for sales",
            "how many sales by id",
            "top 5 sales by revenue",
        ])
        .collect::<Vec<_>>();
    let canonical = |question: &str| match DeterministicCompiler.compile(question, &schema) {
        Compiled::Plan(plan) => print_plan(&plan).unwrap(),
        Compiled::NoParse(report) => format!("refusal: {report:?}"),
    };
    let baseline = questions
        .iter()
        .map(|question| (*question, canonical(question)))
        .collect::<Vec<_>>();
    // Fixed-seed LCG Fisher-Yates: 1,000 shuffled orders, identical outputs.
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    for _ in 0..1_000 {
        let mut order = (0..questions.len()).collect::<Vec<_>>();
        for index in (1..order.len()).rev() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let swap = (state >> 33) as usize % (index + 1);
            order.swap(index, swap);
        }
        for slot in order {
            let (question, expected) = &baseline[slot];
            assert_eq!(&canonical(question), expected, "question: {question:?}");
        }
    }
}

#[test]
fn aggregate_refusals_are_byte_identical_compiles() {
    let schema = sales_schema();
    for question in [
        "total revenu of sales",
        "total revenue of widgets",
        "total for sales",
        "total revenue of sales by revenue",
        "total revenue of sales by regin",
        "how many sales by id",
        "how many sales by",
        "sales total by region",
    ] {
        assert_eq!(
            DeterministicCompiler.compile(question, &schema),
            DeterministicCompiler.compile(question, &schema),
            "question: {question:?}"
        );
    }
    let _ = NoParse::default();
}
