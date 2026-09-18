use devondb_plan::expr::{BinaryOp, Expr, Metric};
use devondb_plan::ops::{
    AggregateFunction, AggregateItem, Direction, KnnMode, Operator, PLAN_VERSION, Plan,
    ProjectionItem, SortKey, SortOrder,
};
use devondb_plan::statement::{RelRow, Statement, StatementEnvelope};
use devondb_types::{DevonError, logical_type::LogicalType, schema::Column, value::Value};
use proptest::{
    collection,
    prelude::*,
    test_runner::{RngSeed, TestCaseError},
};

const PROPTEST_SEED: u64 = 0x4456_4e50_4c41_4e4a;
const MAX_INPUT_BYTES: usize = 4096;

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

fn arbitrary_json_input() -> BoxedStrategy<String> {
    let arbitrary_bytes = collection::vec(any::<u8>(), 0..=MAX_INPUT_BYTES)
        .prop_map(|bytes| bounded_lossy_utf8(&bytes));
    let arbitrary_unicode = collection::vec(any::<char>(), 0..=MAX_INPUT_BYTES / 4)
        .prop_map(|characters| characters.into_iter().collect());
    let jsonish = collection::vec(
        prop::sample::select(
            "{}[],:\\\"0123456789eE+-.nulltruefalsestmopv \\t\\r\\n\\0"
                .chars()
                .collect::<Vec<char>>(),
        ),
        0..=MAX_INPUT_BYTES,
    )
    .prop_map(|characters| characters.into_iter().collect());
    let long_runs = (
        prop::sample::select(vec!['\0', '{', '}', '[', ']', '"', '\\', '9']),
        0..=MAX_INPUT_BYTES,
    )
        .prop_map(|(character, length)| character.to_string().repeat(length));
    let deep_arrays =
        (0_usize..=512).prop_map(|depth| format!("{}null{}", "[".repeat(depth), "]".repeat(depth)));
    let deep_objects = (0_usize..=512)
        .prop_map(|depth| format!("{}null{}", "{\"x\":".repeat(depth), "}".repeat(depth)));
    let huge_numbers = (1_usize..=MAX_INPUT_BYTES - 128).prop_map(|digits| {
        format!(
            r#"{{"v":0,"plan":{{"op":"Filter","predicate":{{"lit":{}}},"input":{{"op":"ScanNodes","table":"T","binding":"t"}}}}}}"#,
            "9".repeat(digits)
        )
    });
    let truncations = (
        prop::sample::select(vec![
            r#"{"v":0,"plan":{"op":"ScanNodes","table":"Person","binding":"p"}}"#.to_owned(),
            r#"{"v":0,"stmt":{"stmt":"InsertNode","table":"Person","rows":[]}}"#.to_owned(),
        ]),
        any::<usize>(),
    )
        .prop_map(|(input, point)| {
            let end = point % (input.len() + 1);
            input[..end].to_owned()
        });
    let invalid_escapes = prop::sample::select(
        [
            r#""\uD800""#,
            r#""\uDC00""#,
            r#""\u{110000}""#,
            r#""\x80""#,
            r#"{"v":0,"plan":"\uD800"}"#,
        ]
        .map(str::to_owned)
        .to_vec(),
    );

    prop_oneof![
        5 => arbitrary_bytes,
        4 => arbitrary_unicode,
        5 => jsonish,
        2 => long_runs,
        1 => deep_arrays,
        1 => deep_objects,
        1 => huge_numbers,
        2 => truncations,
        1 => invalid_escapes,
    ]
    .boxed()
}

fn finite_f32() -> BoxedStrategy<f32> {
    any::<u32>()
        .prop_filter("f32 bit pattern must be finite", |bits| {
            f32::from_bits(*bits).is_finite()
        })
        .prop_map(f32::from_bits)
        .boxed()
}

fn json_round_trip_f64() -> BoxedStrategy<f64> {
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
    let arbitrary = collection::vec(any::<char>(), 0..=8)
        .prop_map(|characters| characters.into_iter().collect());
    let edge_cases = prop::sample::select(
        [
            "filter",
            "two words",
            "dotted.name",
            "quote\"slash\\",
            "line\nfeed",
            "nul\0name",
            "Δεδομένα",
            "🦀",
        ]
        .map(str::to_owned)
        .to_vec(),
    );

    prop_oneof![5 => bare, 3 => arbitrary, 2 => edge_cases].boxed()
}

fn value() -> BoxedStrategy<Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int64),
        json_round_trip_f64().prop_map(Value::Float64),
        collection::vec(any::<char>(), 0..=12)
            .prop_map(|characters| Value::String(characters.into_iter().collect())),
        collection::vec(finite_f32(), 0..=5).prop_map(Value::Vector),
    ]
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
            (inner.clone(), inner, metric()).prop_map(|(left, right, metric)| {
                Expr::Distance {
                    left: Box::new(left),
                    right: Box::new(right),
                    metric,
                }
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

fn logical_type() -> BoxedStrategy<LogicalType> {
    prop_oneof![
        Just(LogicalType::Bool),
        Just(LogicalType::Int64),
        Just(LogicalType::Float64),
        Just(LogicalType::String),
        (0_u32..=1024).prop_map(|dim| LogicalType::Vector { dim }),
    ]
    .boxed()
}

fn column() -> BoxedStrategy<Column> {
    (identifier(), logical_type(), any::<bool>())
        .prop_map(|(name, ty, primary_key)| Column {
            name,
            ty,
            primary_key,
        })
        .boxed()
}

fn rel_row() -> BoxedStrategy<RelRow> {
    (value(), value(), collection::vec(value(), 0..=5))
        .prop_map(|(from_key, to_key, values)| RelRow {
            from_key,
            to_key,
            values,
        })
        .boxed()
}

fn statement() -> BoxedStrategy<Statement> {
    let create_node = (identifier(), collection::vec(column(), 0..=4))
        .prop_map(|(name, columns)| Statement::CreateNodeTable { name, columns });
    let create_rel = (
        identifier(),
        identifier(),
        identifier(),
        collection::vec(column(), 0..=4),
    )
        .prop_map(|(name, from, to, columns)| Statement::CreateRelTable {
            name,
            from,
            to,
            columns,
        });
    let insert_node = (
        identifier(),
        collection::vec(collection::vec(value(), 0..=5), 0..=4),
    )
        .prop_map(|(table, rows)| Statement::InsertNode { table, rows });
    let insert_rel = (identifier(), collection::vec(rel_row(), 0..=4))
        .prop_map(|(table, rows)| Statement::InsertRel { table, rows });
    let create_hnsw = (identifier(), identifier(), identifier(), metric()).prop_map(
        |(name, table, column, metric)| Statement::CreateHnswIndex {
            name,
            table,
            column,
            metric,
        },
    );

    prop_oneof![
        create_node,
        create_rel,
        insert_node,
        insert_rel,
        create_hnsw,
    ]
    .boxed()
}

fn statement_envelope() -> BoxedStrategy<StatementEnvelope> {
    statement()
        .prop_map(|stmt| StatementEnvelope {
            v: PLAN_VERSION,
            stmt,
        })
        .boxed()
}

fn decode_both(input: &str) {
    drop(Plan::from_json(input));
    drop(StatementEnvelope::from_json(input));
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_json_is_panic_free(input in arbitrary_json_input()) {
        prop_assert!(input.len() <= MAX_INPUT_BYTES);
        decode_both(&input);
    }

    #[test]
    fn structurally_generated_plans_round_trip(plan in plan()) {
        let json = plan
            .to_json()
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let decoded = Plan::from_json(&json)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(decoded, plan, "canonical JSON: {}", json);
    }

    #[test]
    fn structurally_generated_statements_round_trip(envelope in statement_envelope()) {
        let json = envelope
            .to_json()
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let decoded = StatementEnvelope::from_json(&json)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(decoded, envelope, "canonical JSON: {}", json);
    }
}

#[test]
fn pathological_four_kib_json_inputs_are_panic_free() {
    let cases = [
        String::new(),
        "\0".to_owned(),
        "{".repeat(MAX_INPUT_BYTES),
        "[".repeat(MAX_INPUT_BYTES),
        "9".repeat(MAX_INPUT_BYTES),
        r#"{"v":0,"plan":{"op":"ScanNodes""#.to_owned(),
        r#"{"v":0,"stmt":{"stmt":"InsertNode","rows":["#.to_owned(),
        r#""\uD800""#.to_owned(),
        r#""\uDC00""#.to_owned(),
        r#""\x80""#.to_owned(),
    ];

    for input in cases {
        assert!(input.len() <= MAX_INPUT_BYTES);
        decode_both(&input);
    }
}

fn nested_array(depth: usize) -> String {
    format!("{}null{}", "[".repeat(depth), "]".repeat(depth))
}

fn nested_object(depth: usize) -> String {
    format!("{}null{}", "{\"nested\":".repeat(depth), "}".repeat(depth))
}

fn nested_envelope(field: &str, nested: &str) -> String {
    format!(r#"{{"v":0,"{field}":{nested}}}"#)
}

fn assert_recursion_limit_error(label: &str, error: &DevonError) {
    let message = error.to_string();
    assert!(
        message.contains("recursion limit exceeded"),
        "{label} returned the wrong error: {message}"
    );
}

#[test]
fn serde_recursion_limit_rejects_deep_arrays_and_objects_cleanly() {
    for depth in [128, 129, 256, 512] {
        let nested_values = [
            ("array", nested_array(depth)),
            ("object", nested_object(depth)),
        ];

        for (shape, nested) in nested_values {
            let plan_json = nested_envelope("plan", &nested);
            let plan_error = Plan::from_json(&plan_json).unwrap_err();
            assert_recursion_limit_error(&format!("plan {shape} depth {depth}"), &plan_error);

            let statement_json = nested_envelope("stmt", &nested);
            let statement_error = StatementEnvelope::from_json(&statement_json).unwrap_err();
            assert_recursion_limit_error(
                &format!("statement {shape} depth {depth}"),
                &statement_error,
            );
        }
    }
}

fn plan_with_predicate(predicate: &str) -> String {
    let mut json = String::from(r#"{"v":0,"plan":{"op":"Filter","predicate":"#);
    json.push_str(predicate);
    json.push_str(r#","input":{"op":"ScanNodes","table":"T","binding":"t"}}}"#);
    json
}

fn assert_plan_rejected(label: &str, json: &str) {
    assert!(
        Plan::from_json(json).is_err(),
        "{label} was accepted instead of rejected: {json}"
    );
}

fn assert_integer_plan_rejected(label: &str, literal: &str) {
    let predicate = format!(r#"{{"lit":{literal}}}"#);
    let error = Plan::from_json(&plan_with_predicate(&predicate))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(literal),
        "{label} did not name `{literal}`: {error}"
    );
    assert!(
        error.contains("Int64 range"),
        "{label} did not name the Int64 range: {error}"
    );
}

#[test]
fn unknown_operator_names_are_rejected() {
    let cases = [
        ("unknown operator", r#"{"v":0,"plan":{"op":"Teleport"}}"#),
        (
            "wrong operator case",
            r#"{"v":0,"plan":{"op":"scannodes","table":"Person","binding":"p"}}"#,
        ),
        ("unknown statement", r#"{"v":0,"stmt":{"stmt":"Teleport"}}"#),
    ];

    for (label, json) in cases {
        if json.contains(r#""plan""#) {
            assert_plan_rejected(label, json);
        } else {
            assert!(
                StatementEnvelope::from_json(json).is_err(),
                "{label} was accepted instead of rejected: {json}"
            );
        }
    }
}

#[test]
fn binary_expressions_with_wrong_arity_are_rejected() {
    let operators = [
        "eq", "ne", "lt", "le", "gt", "ge", "and", "or", "add", "sub", "mul", "div",
    ];
    let wrong_operands = ["[]", r#"[{"lit":1}]"#, r#"[{"lit":1},{"lit":2},{"lit":3}]"#];

    for operator in operators {
        for operands in wrong_operands {
            let predicate = format!(r#"{{"{operator}":{operands}}}"#);
            assert_plan_rejected(
                &format!("{operator} with operands {operands}"),
                &plan_with_predicate(&predicate),
            );
        }
    }
}

#[test]
fn non_single_key_expression_objects_are_rejected() {
    let cases = [
        ("empty expression", "{}"),
        ("column and literal", r#"{"col":"p.age","lit":1}"#),
        (
            "negation and literal",
            r#"{"not":{"lit":true},"lit":false}"#,
        ),
        ("unknown expression key", r#"{"teleport":{"lit":1}}"#),
    ];

    for (label, predicate) in cases {
        assert_plan_rejected(label, &plan_with_predicate(predicate));
    }
}

#[test]
fn int64_boundary_literals_decode_as_int64() {
    let cases = [
        (i64::MIN.to_string(), i64::MIN),
        (i64::MAX.to_string(), i64::MAX),
    ];

    for (literal, expected) in cases {
        let predicate = format!(r#"{{"lit":{literal}}}"#);
        let decoded = Plan::from_json(&plan_with_predicate(&predicate)).unwrap();
        let Operator::Filter {
            predicate: Expr::Lit(Value::Int64(actual)),
            ..
        } = decoded.plan
        else {
            panic!("boundary literal `{literal}` did not decode as Int64");
        };
        assert_eq!(actual, expected);
    }
}

#[test]
fn out_of_range_integer_literals_are_rejected() {
    let cases = [
        ("Int64 minimum minus one", "-9223372036854775809"),
        ("Int64 maximum plus one", "9223372036854775808"),
        ("u64 maximum", "18446744073709551615"),
    ];

    for (label, literal) in cases {
        assert_integer_plan_rejected(label, literal);
    }
}

// The first raw-JSON pass stores overflowing integer syntax in JsonValue. For
// a negative overflow serde_json converts that syntax to f64, so Expr sees a
// Float64 and accepts it contrary to PLAN_IR.md. This is the smallest Int64
// underflow; the decoder must preserve number syntax to reject it honestly.
#[test]
fn regression_negative_out_of_range_integer_is_not_reinterpreted_as_float() {
    assert_integer_plan_rejected("Int64 minimum minus one", "-9223372036854775809");
}

// Both envelope decoders parse through JsonValue before typed decoding. With
// serde_json's current parser, this finite value does not preserve its f64
// bits through that two-stage path.
#[test]
fn regression_plan_float64_round_trip_preserves_precision() {
    let plan = Plan {
        v: PLAN_VERSION,
        plan: Operator::Filter {
            predicate: Expr::Lit(Value::Float64(1.5025765793750853e61)),
            input: Box::new(Operator::ScanNodes {
                table: String::new(),
                binding: String::new(),
            }),
        },
    };
    let json = plan.to_json().unwrap();

    assert_eq!(Plan::from_json(&json).unwrap(), plan);
}

// The statement entry point has the same two-stage Float64 precision defect;
// this is the smallest statement shape found by the deterministic generator.
#[test]
fn regression_statement_float64_round_trip_preserves_precision() {
    let envelope = StatementEnvelope {
        v: PLAN_VERSION,
        stmt: Statement::InsertNode {
            table: String::new(),
            rows: vec![vec![Value::Float64(-3.3183252480780223e217)]],
        },
    };
    let json = envelope.to_json().unwrap();

    assert_eq!(StatementEnvelope::from_json(&json).unwrap(), envelope);
}

#[test]
fn extreme_float64_values_round_trip_with_exact_bits() {
    let values = [
        0.0,
        -0.0,
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        1.5025765793750853e61,
        -3.3183252480780223e217,
        f64::MAX,
        -f64::MAX,
    ];

    for value in values {
        assert_plan_float_bits(value);
        assert_statement_float_bits(value);
    }
}

fn assert_plan_float_bits(value: f64) {
    let plan = Plan {
        v: PLAN_VERSION,
        plan: Operator::Filter {
            predicate: Expr::Lit(Value::Float64(value)),
            input: Box::new(Operator::ScanNodes {
                table: String::new(),
                binding: String::new(),
            }),
        },
    };
    let decoded = Plan::from_json(&plan.to_json().unwrap()).unwrap();
    let Operator::Filter {
        predicate: Expr::Lit(Value::Float64(actual)),
        ..
    } = decoded.plan
    else {
        panic!("Float64 plan literal decoded with a different type");
    };
    assert_eq!(actual.to_bits(), value.to_bits(), "Float64 value {value:e}");
}

fn assert_statement_float_bits(value: f64) {
    let envelope = StatementEnvelope {
        v: PLAN_VERSION,
        stmt: Statement::InsertNode {
            table: String::new(),
            rows: vec![vec![Value::Float64(value)]],
        },
    };
    let decoded = StatementEnvelope::from_json(&envelope.to_json().unwrap()).unwrap();
    let Statement::InsertNode { rows, .. } = decoded.stmt else {
        panic!("Float64 statement literal decoded with a different statement type");
    };
    let Value::Float64(actual) = rows[0][0] else {
        panic!("Float64 statement literal decoded with a different value type");
    };
    assert_eq!(actual.to_bits(), value.to_bits(), "Float64 value {value:e}");
}
