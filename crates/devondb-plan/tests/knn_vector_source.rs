use devondb_plan::expr::{Expr, Metric};
use devondb_plan::ops::{KnnMode, KnnVectorSource, Operator, PLAN_VERSION, Plan, ProjectionItem};
use devondb_plan::text::parser::{Parsed, parse};
use devondb_plan::text::printer::print_plan;
use devondb_plan::validate::validate;
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema};

fn document_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Document".into(),
        vec![
            Column {
                name: "id".into(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "name".into(),
                ty: LogicalType::String,
                primary_key: false,
            },
            Column {
                name: "embedding".into(),
                ty: LogicalType::Vector { dim: 3 },
                primary_key: false,
            },
            Column {
                name: "small_embedding".into(),
                ty: LogicalType::Vector { dim: 2 },
                primary_key: false,
            },
        ],
    )
    .unwrap()
}

fn projected_scalar(reference: &str) -> KnnVectorSource {
    KnnVectorSource::Scalar {
        plan: Box::new(Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col(reference.into()),
                alias: "query".into(),
            }],
            input: Box::new(Operator::ScanNodes {
                table: "Document".into(),
                binding: "anchor".into(),
            }),
        }),
    }
}

fn knn(query: KnnVectorSource) -> Operator {
    Operator::KnnScan {
        table: "Document".into(),
        column: "embedding".into(),
        query,
        k: 5,
        metric: Metric::Cosine,
        mode: KnnMode::Exact,
    }
}

fn assert_invalid(operator: Operator, fragments: &[&str]) {
    let error = validate(
        &Plan {
            v: PLAN_VERSION,
            plan: operator,
        },
        &[document_schema()],
        &[],
    )
    .unwrap_err()
    .to_string();
    for fragment in fragments {
        assert!(
            error.contains(fragment),
            "missing `{fragment}` in `{error}`"
        );
    }
}

#[test]
fn knn_vector_source_json_round_trip_both_variants() {
    let fixtures = [
        r#"{"v":0,"plan":{"op":"KnnScan","table":"Document","column":"embedding","query":[0.25,0.75],"k":5,"metric":"cosine"}}"#,
        r#"{"v":0,"plan":{"op":"KnnScan","table":"Document","column":"embedding","query":{"scalar":{"plan":{"op":"Project","exprs":[{"expr":{"col":"anchor.embedding"},"as":"embedding"}],"input":{"op":"ScanNodes","table":"Document","binding":"anchor"}}}},"k":5,"metric":"cosine"}}"#,
    ];

    for fixture in fixtures {
        let plan = Plan::from_json(fixture).unwrap();
        assert_eq!(plan.to_json().unwrap(), fixture);
        assert_eq!(Plan::from_json(&plan.to_json().unwrap()).unwrap(), plan);
    }
}

#[test]
fn knn_vector_source_literal_json_is_byte_stable() {
    let historical = r#"{"v":0,"plan":{"op":"KnnScan","table":"Document","column":"embedding","query":[0.5],"k":2,"metric":"l2"}}"#;

    assert_eq!(
        Plan::from_json(historical).unwrap().to_json().unwrap(),
        historical
    );
}

#[test]
fn knn_vector_source_text_round_trip_both_variants() {
    let fixtures = [
        "knn(Document.embedding, [0.25, 0.75, 1], 5, cosine)",
        "knn(Document.embedding, scalar(nodes(Document) as anchor | project anchor.embedding), 5, cosine)",
    ];

    for fixture in fixtures {
        let Parsed::Query(plan) = parse(fixture).unwrap() else {
            panic!("expected query plan");
        };
        assert_eq!(print_plan(&plan).unwrap(), fixture);
        assert_eq!(
            parse(&print_plan(&plan).unwrap()).unwrap(),
            Parsed::Query(plan)
        );
    }
}

#[test]
fn knn_vector_source_rejects_scalar_dimension_mismatch_with_both_types() {
    assert_invalid(
        knn(projected_scalar("anchor.small_embedding")),
        &["KnnScan", "Vector(2)", "Vector(3)"],
    );
}

#[test]
fn knn_vector_source_rejects_non_vector_scalar_with_both_types() {
    assert_invalid(
        knn(projected_scalar("anchor.name")),
        &["KnnScan", "String", "Vector(3)"],
    );
}

#[test]
fn knn_vector_source_validates_the_complete_scalar_plan() {
    assert_invalid(
        knn(projected_scalar("anchor.missing")),
        &["KnnScan query", "scalar", "anchor.missing"],
    );
}

#[test]
fn knn_vector_source_rejects_scalar_with_multiple_output_columns() {
    let query = KnnVectorSource::Scalar {
        plan: Box::new(Operator::ScanNodes {
            table: "Document".into(),
            binding: "anchor".into(),
        }),
    };

    assert_invalid(
        knn(query),
        &["scalar subquery", "exactly one output column", "got 4"],
    );
}

#[test]
fn knn_vector_source_scalar_is_uncorrelated_to_enclosing_bindings() {
    let scalar_knn = Operator::Project {
        exprs: vec![ProjectionItem {
            expr: Expr::Col("Document.id".into()),
            alias: "id".into(),
        }],
        input: Box::new(knn(projected_scalar("outer.embedding"))),
    };
    let plan = Operator::Project {
        exprs: vec![ProjectionItem {
            expr: Expr::Scalar {
                plan: Box::new(scalar_knn),
            },
            alias: "nearest".into(),
        }],
        input: Box::new(Operator::ScanNodes {
            table: "Document".into(),
            binding: "outer".into(),
        }),
    };

    assert_invalid(plan, &["KnnScan query", "outer.embedding"]);
}
