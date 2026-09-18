//! NL_VERSION 9 edge-case behavior:
//!
//! - `<table> with no <relationship>` means no incident edge in the only
//!   endpoint direction determined by the source table. It compiles to a
//!   left anti-join whose right input scans the source class and expands that
//!   relationship. A self-relationship refuses because inbound and outbound
//!   absence are different plans.
//! - `<table> without <column>` means the property is NULL, never that an edge
//!   is absent. It compiles to
//!   `filter coalesce(b.c = b.c, false) = false`.
//! - `<table> not in <column> <value>` means a non-NULL property unequal to
//!   the value. It compiles to `filter b.c != value`; NULL rows do not match.
//! - Numeric `between` is closed: `c >= lower and c <= upper`. Reversed or
//!   otherwise empty ranges remain valid plans and may execute to zero rows.
//! - `from <month> to <month> <year>` is an Int64 epoch-second calendar range
//!   with a closed named-month surface and half-open plan bounds: first day of
//!   the first month inclusive, first day after the last month exclusive.
//!   An omitted time column derives only when exactly one conventional named
//!   Int64 time column exists.
//! - A well-formed question is compiled independently of result cardinality;
//!   zero matching rows are a successful empty result, never `NoParse`.
//! - Every slot ambiguity produces the existing exact `NoParse` structure and
//!   never selects one candidate.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use devondb::text::parser::{Parsed, parse};
use devondb::text::printer::print_plan;
use devondb::{ColumnSummary, Database, NodeTableSummary, SchemaSummary};
use devondb_nl::{
    Compiled, DeterministicCompiler, IntentCompiler, NoParse, TemplateHint, Ungrounded,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    database: Option<Database>,
    directory: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "devondb-nl-edge-cases-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let mut database = Database::create(directory.join("edge-cases.devondb"), 4096).unwrap();
        for statement in fixture_statements() {
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

fn fixture_statements() -> [&'static str; 8] {
    [
        "create node table Customer (id Int64 primary key, name String)",
        "create node table Product (id Int64 primary key, name String, category String, revenue Int64)",
        "create node table Order (id Int64 primary key, title String, revenue Int64, region String, ordered_at Int64)",
        "create rel table Orders from Customer to Product",
        "insert into Customer values (1, \"Ada\"), (2, \"Grace\"), (3, \"Linus\"), (4, \"Empty\")",
        "insert into Product values (1, \"Alpha\", \"X\", 100), (2, \"Beta\", \"Y\", 150), (3, \"Gamma\", \"X\", 200), (4, \"Delta\", null, 250), (5, \"Epsilon\", \"Z\", 300)",
        "insert into Order values (1, \"January\", 50, \"East\", 1768435200), (2, \"February\", 100, null, 1770681600), (3, \"March start\", 125, \"West\", 1772323200), (4, \"March end\", 150, \"East\", 1774915200), (5, \"April\", 175, null, 1776211200), (6, \"May\", 200, \"West\", 1780185600), (7, \"June\", 225, \"East\", 1780272000), (8, \"Unknown date\", 250, \"North\", null)",
        "insert rel into Orders values (1 -> 1), (2 -> 2), (2 -> 3)",
    ]
}

struct ExecutedCase {
    question: &'static str,
    ids: &'static [i64],
}

fn executed_cases() -> Vec<ExecutedCase> {
    vec![
        ExecutedCase {
            question: "customers with no orders",
            ids: &[3, 4],
        },
        ExecutedCase {
            question: "show customers with no orders",
            ids: &[3, 4],
        },
        ExecutedCase {
            question: "list customers with no orders",
            ids: &[3, 4],
        },
        ExecutedCase {
            question: "find customers with no orders",
            ids: &[3, 4],
        },
        ExecutedCase {
            question: "display customers with no orders",
            ids: &[3, 4],
        },
        ExecutedCase {
            question: "orders without region",
            ids: &[2, 5],
        },
        ExecutedCase {
            question: "show orders without region",
            ids: &[2, 5],
        },
        ExecutedCase {
            question: "find orders without region",
            ids: &[2, 5],
        },
        ExecutedCase {
            question: "orders without title",
            ids: &[],
        },
        ExecutedCase {
            question: "products not in category X",
            ids: &[2, 5],
        },
        ExecutedCase {
            question: "show products not in category X",
            ids: &[2, 5],
        },
        ExecutedCase {
            question: "products not in category \"Y\"",
            ids: &[1, 3, 5],
        },
        ExecutedCase {
            question: "products not in category Z",
            ids: &[1, 2, 3],
        },
        ExecutedCase {
            question: "products not in category Missing",
            ids: &[1, 2, 3, 5],
        },
        ExecutedCase {
            question: "products revenue between 100 and 200",
            ids: &[1, 2, 3],
        },
        ExecutedCase {
            question: "products with revenue between 100 and 200",
            ids: &[1, 2, 3],
        },
        ExecutedCase {
            question: "show products revenue between 100 and 200",
            ids: &[1, 2, 3],
        },
        ExecutedCase {
            question: "list products revenue between 100 and 200",
            ids: &[1, 2, 3],
        },
        ExecutedCase {
            question: "find products revenue between 100 and 200",
            ids: &[1, 2, 3],
        },
        ExecutedCase {
            question: "products revenue between 101 and 199",
            ids: &[2],
        },
        ExecutedCase {
            question: "products revenue between 100 and 100",
            ids: &[1],
        },
        ExecutedCase {
            question: "products revenue between 301 and 400",
            ids: &[],
        },
        ExecutedCase {
            question: "orders revenue between 100 and 200",
            ids: &[2, 3, 4, 5, 6],
        },
        ExecutedCase {
            question: "orders with revenue between 126 and 199",
            ids: &[4, 5],
        },
        ExecutedCase {
            question: "orders revenue between 300 and 200",
            ids: &[],
        },
        ExecutedCase {
            question: "orders from march to may 2026",
            ids: &[3, 4, 5, 6],
        },
        ExecutedCase {
            question: "show orders from march to may 2026",
            ids: &[3, 4, 5, 6],
        },
        ExecutedCase {
            question: "orders ordered_at from march to may 2026",
            ids: &[3, 4, 5, 6],
        },
        ExecutedCase {
            question: "orders from april to april 2026",
            ids: &[5],
        },
        ExecutedCase {
            question: "orders from june to june 2026",
            ids: &[7],
        },
        ExecutedCase {
            question: "orders from july to august 2026",
            ids: &[],
        },
        ExecutedCase {
            question: "orders from january to february 2026",
            ids: &[1, 2],
        },
        ExecutedCase {
            question: "orders from may to june 2026",
            ids: &[6, 7],
        },
        ExecutedCase {
            question: "list orders from march to march 2026",
            ids: &[3, 4],
        },
        ExecutedCase {
            question: "find orders from february to may 2026",
            ids: &[2, 3, 4, 5, 6],
        },
    ]
}

fn expected_plan(question: &str) -> &'static str {
    match question {
        "customers with no orders"
        | "show customers with no orders"
        | "list customers with no orders"
        | "find customers with no orders"
        | "display customers with no orders" => concat!(
            "let j1 = nodes(Customer) as edge_source | expand Orders out as edge_target | ",
            "project edge_source.id;\n",
            "nodes(Customer) as customer | left join j1 on customer.id = edge_source.id | ",
            "filter coalesce(edge_source.id = edge_source.id, false) = false | ",
            "project customer.id, customer.name"
        ),
        "orders without region" | "show orders without region" | "find orders without region" => {
            "nodes(Order) as order | filter coalesce(order.region = order.region, false) = false"
        }
        "orders without title" => {
            "nodes(Order) as order | filter coalesce(order.title = order.title, false) = false"
        }
        "products not in category X" | "show products not in category X" => {
            "nodes(Product) as product | filter product.category != \"X\""
        }
        "products not in category \"Y\"" => {
            "nodes(Product) as product | filter product.category != \"Y\""
        }
        "products not in category Z" => {
            "nodes(Product) as product | filter product.category != \"Z\""
        }
        "products not in category Missing" => {
            "nodes(Product) as product | filter product.category != \"Missing\""
        }
        "products revenue between 100 and 200"
        | "products with revenue between 100 and 200"
        | "show products revenue between 100 and 200"
        | "list products revenue between 100 and 200"
        | "find products revenue between 100 and 200" => {
            "nodes(Product) as product | filter product.revenue >= 100 and product.revenue <= 200"
        }
        "products revenue between 101 and 199" => {
            "nodes(Product) as product | filter product.revenue >= 101 and product.revenue <= 199"
        }
        "products revenue between 100 and 100" => {
            "nodes(Product) as product | filter product.revenue >= 100 and product.revenue <= 100"
        }
        "products revenue between 301 and 400" => {
            "nodes(Product) as product | filter product.revenue >= 301 and product.revenue <= 400"
        }
        "orders revenue between 100 and 200" => {
            "nodes(Order) as order | filter order.revenue >= 100 and order.revenue <= 200"
        }
        "orders with revenue between 126 and 199" => {
            "nodes(Order) as order | filter order.revenue >= 126 and order.revenue <= 199"
        }
        "orders revenue between 300 and 200" => {
            "nodes(Order) as order | filter order.revenue >= 300 and order.revenue <= 200"
        }
        "orders from march to may 2026"
        | "show orders from march to may 2026"
        | "orders ordered_at from march to may 2026" => {
            "nodes(Order) as order | filter order.ordered_at >= 1772323200 and order.ordered_at < 1780272000"
        }
        "orders from april to april 2026" => {
            "nodes(Order) as order | filter order.ordered_at >= 1775001600 and order.ordered_at < 1777593600"
        }
        "orders from june to june 2026" => {
            "nodes(Order) as order | filter order.ordered_at >= 1780272000 and order.ordered_at < 1782864000"
        }
        "orders from july to august 2026" => {
            "nodes(Order) as order | filter order.ordered_at >= 1782864000 and order.ordered_at < 1788220800"
        }
        "orders from january to february 2026" => {
            "nodes(Order) as order | filter order.ordered_at >= 1767225600 and order.ordered_at < 1772323200"
        }
        "orders from may to june 2026" => {
            "nodes(Order) as order | filter order.ordered_at >= 1777593600 and order.ordered_at < 1782864000"
        }
        "list orders from march to march 2026" => {
            "nodes(Order) as order | filter order.ordered_at >= 1772323200 and order.ordered_at < 1775001600"
        }
        "find orders from february to may 2026" => {
            "nodes(Order) as order | filter order.ordered_at >= 1769904000 and order.ordered_at < 1780272000"
        }
        _ => panic!("missing golden plan for {question:?}"),
    }
}

#[test]
fn version_9_corpus_executes_exact_rows_and_preserves_empty_results() {
    let cases = executed_cases();
    assert!(cases.len() >= 30);
    let mut fixture = TestDatabase::new();
    let database = fixture.database();
    let mut empty_results = 0;
    for case in cases {
        let first = DeterministicCompiler.compile_with_database(case.question, database);
        let second = DeterministicCompiler.compile_with_database(case.question, database);
        assert_eq!(first, second, "question: {:?}", case.question);
        let Compiled::Plan(plan) = first else {
            panic!(
                "well-formed question refused: {:?}: {first:?}",
                case.question
            );
        };
        assert_eq!(
            print_plan(&plan).unwrap(),
            expected_plan(case.question),
            "question: {:?}",
            case.question
        );
        let result = database.run(&plan).unwrap();
        let ids = result
            .rows
            .iter()
            .map(|row| row[0].to_string().parse::<i64>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, case.ids, "question: {:?}", case.question);
        if result.rows.is_empty() {
            empty_results += 1;
        }
    }
    assert!(
        empty_results >= 4,
        "empty-result phrasing was not exercised"
    );
}

#[test]
fn version_9_plan_shapes_pin_negation_and_range_laws() {
    let mut fixture = TestDatabase::new();
    let database = fixture.database();
    for (question, expected) in [
        (
            "orders without region",
            "nodes(Order) as order | filter coalesce(order.region = order.region, false) = false",
        ),
        (
            "products not in category X",
            "nodes(Product) as product | filter product.category != \"X\"",
        ),
        (
            "products revenue between 100 and 200",
            "nodes(Product) as product | filter product.revenue >= 100 and product.revenue <= 200",
        ),
        (
            "orders from march to may 2026",
            "nodes(Order) as order | filter order.ordered_at >= 1772323200 and order.ordered_at < 1780272000",
        ),
    ] {
        let Compiled::Plan(plan) = DeterministicCompiler.compile_with_database(question, database)
        else {
            panic!("plan-shape question refused: {question:?}");
        };
        assert_eq!(
            print_plan(&plan).unwrap(),
            expected,
            "question: {question:?}"
        );
    }
    let Compiled::Plan(anti_join) =
        DeterministicCompiler.compile_with_database("customers with no orders", database)
    else {
        panic!("anti-edge question refused");
    };
    let canonical = print_plan(&anti_join).unwrap();
    assert!(canonical.contains("left join j1 on customer.id = edge_source.id"));
    assert!(canonical.contains("expand Orders out as edge_target"));
    assert!(canonical.contains("coalesce(edge_source.id = edge_source.id, false) = false"));
}

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.to_owned(),
        ty: ty.to_owned(),
        primary_key,
    }
}

fn table_mut<'a>(schema: &'a mut SchemaSummary, name: &str) -> &'a mut NodeTableSummary {
    schema
        .node_tables
        .iter_mut()
        .find(|table| table.name == name)
        .unwrap()
}

fn refusal(question: &str, schema: &SchemaSummary) -> NoParse {
    match DeterministicCompiler.compile(question, schema) {
        Compiled::NoParse(report) => report,
        Compiled::Plan(plan) => panic!("ambiguous question compiled: {question:?}: {plan:?}"),
    }
}

fn assert_exact_refusal(
    question: &str,
    schema: &SchemaSummary,
    token: &str,
    suggestion: &str,
    hint: &str,
) {
    let report = refusal(question, schema);
    assert_eq!(
        report.unrecognized,
        [Ungrounded {
            token: token.to_owned(),
            suggestion: Some(suggestion.to_owned()),
        }],
        "question: {question:?}"
    );
    assert_eq!(
        report.nearest,
        [TemplateHint {
            example: hint.to_owned(),
        }],
        "question: {question:?}"
    );
    assert_eq!(report, refusal(question, schema), "refusal must be exact");
}

#[test]
fn version_9_ambiguity_corpus_refuses_eight_two_plan_phrasings_exactly() {
    let mut fixture = TestDatabase::new();
    let base = fixture.database().schema_summary();

    let mut category = base.clone();
    table_mut(&mut category, "Product")
        .columns
        .push(column("Category", "String", false));
    assert_exact_refusal(
        "products not in category X",
        &category,
        "category",
        "category, Category",
        "<table> not in <column> <value>",
    );

    let mut region = base.clone();
    table_mut(&mut region, "Order")
        .columns
        .push(column("Region", "String", false));
    assert_exact_refusal(
        "orders without region",
        &region,
        "region",
        "region, Region",
        "<table> without <column>",
    );

    let mut revenue = base.clone();
    table_mut(&mut revenue, "Product")
        .columns
        .push(column("Revenue", "Int64", false));
    assert_exact_refusal(
        "products revenue between 100 and 200",
        &revenue,
        "revenue",
        "revenue, Revenue",
        "<table> with <column> between <lower> and <upper>",
    );

    let mut table = base.clone();
    let mut duplicate = table.node_tables[0].clone();
    duplicate.name = "customer".to_owned();
    table.node_tables.push(duplicate);
    assert_exact_refusal(
        "customers with no orders",
        &table,
        "customers",
        "Customer, customer",
        "<table> with no <relationship>",
    );

    let mut relation = base.clone();
    let mut duplicate = relation.rel_tables[0].clone();
    duplicate.name = "orders".to_owned();
    relation.rel_tables.push(duplicate);
    assert_exact_refusal(
        "customers with no orders",
        &relation,
        "orders",
        "Orders, orders",
        "<table> with no <relationship>",
    );

    let mut inferred_time = base.clone();
    table_mut(&mut inferred_time, "Order")
        .columns
        .push(column("created_at", "Int64", false));
    assert_exact_refusal(
        "orders from march to may 2026",
        &inferred_time,
        "from",
        "ambiguous time column: `ordered_at`, `created_at`",
        "<table> [<column>] from <month> to <month> <year>",
    );

    let mut self_edge = base.clone();
    self_edge.rel_tables[0].to = "Customer".to_owned();
    assert_exact_refusal(
        "customers with no orders",
        &self_edge,
        "orders",
        "ambiguous edge direction for self-relationship `Orders`",
        "<table> with no <relationship>",
    );

    let mut explicit_time = base;
    table_mut(&mut explicit_time, "Order")
        .columns
        .push(column("Ordered_At", "Int64", false));
    assert_exact_refusal(
        "orders ordered_at from march to may 2026",
        &explicit_time,
        "ordered_at",
        "ordered_at, Ordered_At",
        "<table> [<column>] from <month> to <month> <year>",
    );
}
