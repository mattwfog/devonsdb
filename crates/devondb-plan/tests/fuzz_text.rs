use devondb_plan::expr::{BinaryOp, DateTruncUnit, Expr, Metric};
use devondb_plan::ops::{
    AggregateFunction, AggregateItem, Direction, KnnMode, Operator, PLAN_VERSION, Plan,
    ProjectionItem, SortKey, SortOrder,
};
use devondb_plan::statement::{Statement, StatementEnvelope};
use devondb_plan::text::{
    parser::{Parsed, parse},
    printer::{print_plan, print_statement},
};
use devondb_types::{Decimal128, DevonError, value::Value};
use proptest::{
    collection,
    prelude::*,
    test_runner::{RngSeed, TestCaseError, TestCaseResult},
};

const PROPTEST_SEED: u64 = 0x4456_4e50_4c41_4e30;
const SCALAR_V2_PROPTEST_SEED: u64 = 0x4456_5343_414c_4152;
const MAX_INPUT_BYTES: usize = 4096;
const MIN_TIMESTAMP_MICROS: i64 = -62_135_596_800_000_000;
const MAX_TIMESTAMP_MICROS: i64 = 253_402_300_799_999_999;
const DECIMAL_MODULUS: i128 = 10_i128.pow(38);

fn bounded_lossy_utf8(bytes: &[u8]) -> String {
    let mut input = String::from_utf8_lossy(bytes).into_owned();
    if input.len() <= MAX_INPUT_BYTES {
        return input;
    }

    let mut end = MAX_INPUT_BYTES;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    input.truncate(end);
    input
}

fn scalar_v2_fragment_input() -> BoxedStrategy<String> {
    let fragments = [
        "upsert ",
        " values ",
        "(",
        ")",
        ", ",
        r#"timestamp("#,
        r#"timestamp("1970-01-01T00:00:00Z")"#,
        r#"2024-02-29T23:59:59.999999Z"#,
        r#"bytes("#,
        r#"bytes("00ff1a")"#,
        "00FF",
        r#"decimal("#,
        r#"decimal("-19.99")"#,
        "1e5",
        r#"json("#,
        r#"json("{\"k\":1}")"#,
        r#"[1,true,null]"#,
        "{",
        "}",
        "\\",
        "\"",
    ];

    collection::vec(
        prop::sample::select(fragments.map(str::to_owned).to_vec()),
        0..=MAX_INPUT_BYTES / 64,
    )
    .prop_map(|fragments| fragments.concat())
    .boxed()
}

fn a6_fragment_input() -> BoxedStrategy<String> {
    let fragments = [
        "if(",
        "coalesce(",
        "least(",
        "greatest(",
        "date_trunc(",
        r#""day""#,
        r#""hour""#,
        "null",
        "true",
        "p.value",
        ", ",
        ")",
    ];
    collection::vec(
        prop::sample::select(fragments.map(str::to_owned).to_vec()),
        0..=MAX_INPUT_BYTES / 32,
    )
    .prop_map(|fragments| fragments.concat())
    .boxed()
}

fn arbitrary_parser_input() -> BoxedStrategy<String> {
    let arbitrary_bytes = collection::vec(any::<u8>(), 0..=MAX_INPUT_BYTES)
        .prop_map(|bytes| bounded_lossy_utf8(&bytes));
    let arbitrary_unicode = collection::vec(any::<char>(), 0..=MAX_INPUT_BYTES / 4)
        .prop_map(|characters| characters.into_iter().collect());
    let long_runs = (
        prop::sample::select(vec!['\0', '`', '"', '\\', '(', ')', '9', 'a', '|']),
        0..=MAX_INPUT_BYTES,
    )
        .prop_map(|(character, length)| character.to_string().repeat(length));
    let deep_parentheses = (0_usize..=128).prop_map(|depth| {
        format!(
            "nodes(T) as t | filter {}true{}",
            "(".repeat(depth),
            ")".repeat(depth)
        )
    });
    let deep_not = (0_usize..=1000)
        .prop_map(|depth| format!("nodes(T) as t | filter {}true", "not ".repeat(depth)));

    prop_oneof![
        5 => arbitrary_bytes,
        8 => arbitrary_unicode,
        5 => scalar_v2_fragment_input(),
        5 => a6_fragment_input(),
        2 => long_runs,
        1 => deep_parentheses,
        1 => deep_not,
        1 => prop::sample::select(vec!["`".to_owned(), "\"".to_owned()]),
    ]
    .boxed()
}

fn assert_error_position_is_honest(input: &str) -> TestCaseResult {
    let Err(error) = parse(input) else {
        return Ok(());
    };
    let context = match error {
        DevonError::InvalidArgument { context } => context,
        other => {
            return Err(TestCaseError::fail(format!(
                "parse returned a non-InvalidArgument error for {input:?}: {other}"
            )));
        }
    };
    let position = parse_error_position(&context).ok_or_else(|| {
        TestCaseError::fail(format!(
            "parse error omitted its character position for {input:?}: {context}"
        ))
    })?;
    let end = input.chars().count() + 1;
    prop_assert!(
        (1..=end).contains(&position),
        "reported position {position} outside 1..={end} for {input:?}: {context}"
    );
    Ok(())
}

fn parse_error_position(context: &str) -> Option<usize> {
    const PREFIXES: [&str; 2] = [
        "DevonPlan text lex error at position ",
        "DevonPlan text parse error at position ",
    ];
    PREFIXES.iter().find_map(|prefix| {
        context
            .strip_prefix(prefix)?
            .split_once(':')?
            .0
            .parse()
            .ok()
    })
}

fn finite_f32() -> BoxedStrategy<f32> {
    any::<u32>()
        .prop_filter("f32 bit pattern must be finite", |bits| {
            f32::from_bits(*bits).is_finite()
        })
        .prop_map(f32::from_bits)
        .boxed()
}

fn finite_f64() -> BoxedStrategy<f64> {
    any::<u64>()
        .prop_filter("f64 bit pattern must be finite", |bits| {
            f64::from_bits(*bits).is_finite()
        })
        .prop_map(f64::from_bits)
        .boxed()
}

fn identifier() -> BoxedStrategy<String> {
    let bare = (
        prop::sample::select(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_"
                .chars()
                .collect::<Vec<char>>(),
        ),
        collection::vec(
            prop::sample::select(
                "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789"
                    .chars()
                    .collect::<Vec<char>>(),
            ),
            0..=10,
        ),
    )
        .prop_map(|(first, rest)| std::iter::once(first).chain(rest).collect());
    let quoted = collection::vec(any::<char>(), 1..=8)
        .prop_map(|characters| characters.into_iter().collect());
    let edge_cases = prop::sample::select(
        [
            "filter",
            "if",
            "coalesce",
            "least",
            "greatest",
            "date_trunc",
            "two words",
            "dotted.name",
            "tick`slash\\",
            "line\nfeed",
            "nul\0name",
            "Δεδομένα",
            "🦀",
        ]
        .map(str::to_owned)
        .to_vec(),
    );

    prop_oneof![5 => bare, 3 => quoted, 2 => edge_cases].boxed()
}

fn value() -> BoxedStrategy<Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int64),
        finite_f64().prop_map(Value::Float64),
        collection::vec(any::<char>(), 0..=12)
            .prop_map(|characters| Value::String(characters.into_iter().collect())),
        collection::vec(finite_f32(), 0..=5).prop_map(Value::Vector),
        scalar_v2_value(),
    ]
    .boxed()
}

fn timestamp_value() -> BoxedStrategy<Value> {
    prop_oneof![
        8 => (MIN_TIMESTAMP_MICROS..=MAX_TIMESTAMP_MICROS).prop_map(Value::Timestamp),
        1 => prop::sample::select(vec![
            MIN_TIMESTAMP_MICROS,
            -1,
            0,
            1,
            MAX_TIMESTAMP_MICROS,
        ])
        .prop_map(Value::Timestamp),
    ]
    .boxed()
}

fn bytes_value() -> BoxedStrategy<Value> {
    collection::vec(any::<u8>(), 0..=32)
        .prop_map(Value::Bytes)
        .boxed()
}

fn decimal_value() -> BoxedStrategy<Value> {
    (any::<i128>(), 0_u8..=38)
        .prop_map(|(digits, scale)| {
            Value::Decimal(Decimal128::new(digits % DECIMAL_MODULUS, scale).unwrap())
        })
        .boxed()
}

fn json_value() -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|value| serde_json::Value::Number(value.into())),
        collection::vec(any::<char>(), 0..=12)
            .prop_map(|characters| { serde_json::Value::String(characters.into_iter().collect()) }),
    ];

    leaf.prop_recursive(3, 32, 4, |inner| {
        prop_oneof![
            collection::vec(inner.clone(), 0..=4).prop_map(serde_json::Value::Array),
            collection::btree_map(
                collection::vec(any::<char>(), 0..=8)
                    .prop_map(|characters| characters.into_iter().collect::<String>()),
                inner,
                0..=4,
            )
            .prop_map(|entries| serde_json::Value::Object(entries.into_iter().collect())),
        ]
    })
    .prop_map(|document| Value::Json(serde_json::to_string(&document).unwrap()))
    .boxed()
}

fn scalar_v2_value() -> BoxedStrategy<Value> {
    prop_oneof![
        timestamp_value(),
        bytes_value(),
        decimal_value(),
        json_value(),
    ]
    .boxed()
}

fn scalar_v2_row() -> BoxedStrategy<Vec<Value>> {
    (
        timestamp_value(),
        bytes_value(),
        decimal_value(),
        json_value(),
    )
        .prop_map(|(timestamp, bytes, decimal, json)| vec![timestamp, bytes, decimal, json])
        .boxed()
}

fn scalar_v2_upsert() -> BoxedStrategy<StatementEnvelope> {
    (identifier(), collection::vec(scalar_v2_row(), 1..=4))
        .prop_map(|(table, rows)| StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::UpsertNode { table, rows },
        })
        .boxed()
}

fn expression() -> BoxedStrategy<Expr> {
    let leaf = prop_oneof![
        identifier().prop_map(|column| Expr::Col(format!("b.{column}"))),
        value().prop_map(Expr::Lit),
    ];

    leaf.prop_recursive(4, 64, 4, |inner| {
        prop_oneof![
            inner.clone().prop_map(|expr| Expr::Not(Box::new(expr))),
            (binary_operator(), inner.clone(), inner.clone()).prop_map(|(op, left, right)| {
                Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                }
            }),
            (inner.clone(), inner.clone(), metric()).prop_map(|(left, right, metric)| {
                Expr::Distance {
                    left: Box::new(left),
                    right: Box::new(right),
                    metric,
                }
            }),
            (inner.clone(), inner.clone(), inner.clone()).prop_map(
                |(cond, then_expr, else_expr)| Expr::If {
                    cond: Box::new(cond),
                    then_expr: Box::new(then_expr),
                    else_expr: Box::new(else_expr),
                },
            ),
            collection::vec(inner.clone(), 2..=4).prop_map(Expr::Coalesce),
            collection::vec(inner.clone(), 2..=4).prop_map(Expr::Least),
            collection::vec(inner.clone(), 2..=4).prop_map(Expr::Greatest),
            inner.prop_map(|value| Expr::DateTrunc {
                unit: DateTruncUnit::Day,
                value: Box::new(value),
            }),
        ]
    })
    .boxed()
}

fn binary_operator() -> BoxedStrategy<BinaryOp> {
    prop::sample::select(vec![
        BinaryOp::Eq,
        BinaryOp::Ne,
        BinaryOp::Lt,
        BinaryOp::Le,
        BinaryOp::Gt,
        BinaryOp::Ge,
        BinaryOp::And,
        BinaryOp::Or,
        BinaryOp::Add,
        BinaryOp::Sub,
        BinaryOp::Mul,
        BinaryOp::Div,
    ])
    .boxed()
}

fn metric() -> BoxedStrategy<Metric> {
    prop::sample::select(vec![Metric::Cosine, Metric::L2]).boxed()
}

fn source() -> BoxedStrategy<Operator> {
    let scan = (identifier(), identifier())
        .prop_map(|(table, binding)| Operator::ScanNodes { table, binding });
    let knn = (
        identifier(),
        identifier(),
        collection::vec(finite_f32(), 0..=8),
        0_u64..=10_000,
        metric(),
        prop::sample::select(vec![KnnMode::Exact, KnnMode::Approximate]),
    )
        .prop_map(
            |(table, column, query, k, metric, mode)| Operator::KnnScan {
                table,
                column,
                query: query.into(),
                k,
                metric,
                mode,
            },
        );

    prop_oneof![scan, knn].boxed()
}

fn projection_items() -> BoxedStrategy<Vec<ProjectionItem>> {
    collection::vec((expression(), identifier()), 1..=3)
        .prop_map(|items| {
            items
                .into_iter()
                .map(|(expr, alias)| ProjectionItem { expr, alias })
                .collect()
        })
        .boxed()
}

fn sort_keys() -> BoxedStrategy<Vec<SortKey>> {
    collection::vec(
        (
            expression(),
            prop::sample::select(vec![SortOrder::Asc, SortOrder::Desc]),
        ),
        1..=3,
    )
    .prop_map(|keys| {
        keys.into_iter()
            .map(|(expr, order)| SortKey { expr, order })
            .collect()
    })
    .boxed()
}

fn aggregate_items() -> BoxedStrategy<Vec<AggregateItem>> {
    collection::vec(
        (
            prop::sample::select(vec![
                AggregateFunction::Count,
                AggregateFunction::Sum,
                AggregateFunction::Min,
                AggregateFunction::Max,
                AggregateFunction::Avg,
            ]),
            expression(),
            identifier(),
        ),
        1..=3,
    )
    .prop_map(|items| {
        items
            .into_iter()
            .map(|(function, expr, alias)| AggregateItem {
                function,
                expr,
                alias,
            })
            .collect()
    })
    .boxed()
}

fn operator() -> BoxedStrategy<Operator> {
    source()
        .prop_recursive(7, 128, 1, |input| {
            prop_oneof![
                (
                    identifier(),
                    prop::sample::select(vec![Direction::Out, Direction::In, Direction::Both]),
                    identifier(),
                    identifier(),
                    input.clone(),
                )
                    .prop_map(|(rel, direction, from_binding, binding, input)| {
                        Operator::Expand {
                            rel,
                            direction,
                            from_binding,
                            binding,
                            input: Box::new(input),
                        }
                    }),
                (expression(), input.clone()).prop_map(|(predicate, input)| Operator::Filter {
                    predicate,
                    input: Box::new(input),
                }),
                (projection_items(), input.clone()).prop_map(|(exprs, input)| {
                    Operator::Project {
                        exprs,
                        input: Box::new(input),
                    }
                }),
                (sort_keys(), input.clone()).prop_map(|(keys, input)| Operator::Sort {
                    keys,
                    input: Box::new(input),
                }),
                (
                    0_u64..=10_000,
                    prop::option::of(0_u64..=10_000),
                    input.clone(),
                )
                    .prop_map(|(count, offset, input)| Operator::Limit {
                        count,
                        offset,
                        input: Box::new(input),
                    }),
                (
                    collection::vec(expression(), 0..=3),
                    aggregate_items(),
                    input,
                )
                    .prop_map(|(group_by, aggs, input)| Operator::Aggregate {
                        group_by,
                        aggs,
                        input: Box::new(input),
                    }),
            ]
        })
        .boxed()
}

fn plan() -> BoxedStrategy<Plan> {
    operator()
        .prop_map(|plan| Plan {
            v: PLAN_VERSION,
            plan,
        })
        .boxed()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_text_is_panic_free_and_position_honest(input in arbitrary_parser_input()) {
        assert_error_position_is_honest(&input)?;
    }

    #[test]
    fn structurally_generated_plans_round_trip(plan in plan()) {
        let text = print_plan(&plan).map_err(|error| TestCaseError::fail(error.to_string()))?;
        let parsed = parse(&text).map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(&parsed, &Parsed::Query(plan), "canonical text: {}", text);
        let Parsed::Query(parsed_plan) = parsed else {
            return Err(TestCaseError::fail("plan text parsed as a statement"));
        };
        let fixed_text = print_plan(&parsed_plan)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(fixed_text, text);
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(SCALAR_V2_PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    #[test]
    fn scalar_v2_upserts_have_a_text_fixpoint(envelope in scalar_v2_upsert()) {
        let first_text = print_statement(&envelope)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let first_parse = parse(&first_text)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(
            &first_parse,
            &Parsed::Statement(envelope),
            "first canonical text: {}",
            first_text
        );

        let Parsed::Statement(first_envelope) = &first_parse else {
            return Err(TestCaseError::fail("upsert text parsed as a query"));
        };
        let second_text = print_statement(first_envelope)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let second_parse = parse(&second_text)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;

        prop_assert_eq!(second_text, first_text);
        prop_assert_eq!(second_parse, first_parse);
    }
}

#[test]
fn pathological_four_kib_inputs_are_panic_free_and_position_honest() {
    let nested = format!(
        "nodes(T) as t | filter {}true{}",
        "(".repeat(128),
        ")".repeat(128)
    );
    let negated = format!("nodes(T) as t | filter {}true", "not ".repeat(1000));
    let cases = [
        "\0".to_owned(),
        "`".to_owned(),
        "\"".to_owned(),
        "`".repeat(MAX_INPUT_BYTES),
        "\"".repeat(MAX_INPUT_BYTES),
        "(".repeat(MAX_INPUT_BYTES),
        nested,
        negated,
    ];

    for input in cases {
        assert!(input.len() <= MAX_INPUT_BYTES);
        assert_error_position_is_honest(&input).unwrap();
    }
}

// Recursive parenthesis parsing can exhaust the test thread's stack. This is
// the smallest input that reliably reproduces the condition guarded by the
// parser's explicit nesting limit.
#[test]
fn regression_unmatched_parentheses_should_return_an_error() {
    let input = format!("nodes(T)as t|sort{}", "(".repeat(490));
    let DevonError::InvalidArgument { context } = parse(&input).unwrap_err() else {
        panic!("expected InvalidArgument");
    };
    assert_eq!(
        context,
        "DevonPlan text parse error at position 146: expression nesting exceeds 128 levels; \
         offending token \"(\""
    );
}

#[test]
fn expression_nesting_limit_accepts_boundary_and_rejects_one_past_it() {
    for depth in [127, 128] {
        let input = format!(
            "nodes(T) as t | filter {}true{}",
            "(".repeat(depth),
            ")".repeat(depth)
        );
        let parsed = parse(&input).unwrap();
        let Parsed::Query(plan) = &parsed else {
            panic!("expected query");
        };
        let canonical = print_plan(plan).unwrap();
        assert_eq!(parse(&canonical).unwrap(), parsed);
    }

    let input = format!(
        "nodes(T) as t | filter {}true{}",
        "(".repeat(129),
        ")".repeat(129)
    );
    let DevonError::InvalidArgument { context } = parse(&input).unwrap_err() else {
        panic!("expected InvalidArgument");
    };
    assert_eq!(
        context,
        "DevonPlan text parse error at position 152: expression nesting exceeds 128 levels; \
         offending token \"(\""
    );
}

#[test]
fn a6_call_forms_count_toward_the_expression_nesting_limit() {
    let forms = [
        ("if(true, true, ", ")"),
        ("coalesce(null, ", ")"),
        ("least(0, ", ")"),
        ("greatest(0, ", ")"),
        ("date_trunc(\"day\", ", ")"),
    ];
    for (prefix, suffix) in forms {
        let boundary = format!(
            "nodes(T) as t | filter {}true{}",
            prefix.repeat(128),
            suffix.repeat(128)
        );
        let parsed = parse(&boundary).unwrap();
        let Parsed::Query(plan) = &parsed else {
            panic!("expected query");
        };
        assert_eq!(parse(&print_plan(plan).unwrap()).unwrap(), parsed);

        let over = format!(
            "nodes(T) as t | filter {}true{}",
            prefix.repeat(129),
            suffix.repeat(129)
        );
        let DevonError::InvalidArgument { context } = parse(&over).unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        assert!(
            context.contains("expression nesting exceeds 128 levels"),
            "{context}"
        );
        assert!(context.contains("offending token \"(\""), "{context}");
    }
}
