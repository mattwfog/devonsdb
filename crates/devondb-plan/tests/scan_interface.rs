use std::collections::HashMap;

use devondb_plan::expr::Expr;
use devondb_plan::ops::{Operator, PLAN_VERSION, Plan, ProjectionItem};
use devondb_plan::statement::InterfaceColumn;
use devondb_plan::text::parser::{Parsed, parse_with_schema};
use devondb_plan::text::printer::print_plan;
use devondb_plan::typing::{ExpressionType, InterfaceSchema, SchemaInput, expression_type};
use devondb_plan::validate::validate_with_schema;
use devondb_types::DevonError;
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema, RelTableSchema};

struct Fixture {
    nodes: Vec<NodeTableSchema>,
    rels: Vec<RelTableSchema>,
    interfaces: Vec<InterfaceSchema>,
}

impl Fixture {
    fn new() -> Self {
        let person = NodeTableSchema::new(
            "Person".to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
                column("score", LogicalType::Int64, false),
                column("private_note", LogicalType::String, false),
            ],
        )
        .unwrap();
        let nameable = InterfaceSchema::new(
            "Nameable".to_owned(),
            vec![
                InterfaceColumn {
                    name: "name".to_owned(),
                    ty: LogicalType::String,
                },
                InterfaceColumn {
                    name: "score".to_owned(),
                    ty: LogicalType::Int64,
                },
            ],
        );
        Self {
            nodes: vec![person],
            rels: Vec::new(),
            interfaces: vec![nameable],
        }
    }

    fn schema(&self) -> SchemaInput<'_> {
        SchemaInput::new(&self.nodes, &self.rels, &self.interfaces)
    }
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn interface_scan(interface: &str, binding: &str) -> Operator {
    Operator::ScanInterface {
        interface: interface.to_owned(),
        binding: binding.to_owned(),
    }
}

fn plan(operator: Operator) -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: operator,
    }
}

#[test]
fn scan_interface_json_and_classof_round_trip_canonically() {
    let plan = plan(Operator::Project {
        exprs: vec![
            ProjectionItem {
                expr: Expr::Col("n.name".to_owned()),
                alias: "name".to_owned(),
            },
            ProjectionItem {
                expr: Expr::ClassOf("n".to_owned()),
                alias: "class".to_owned(),
            },
        ],
        input: Box::new(interface_scan("Nameable", "n")),
    });
    let expected = r#"{"v":0,"plan":{"op":"Project","exprs":[{"expr":{"col":"n.name"},"as":"name"},{"expr":{"classof":"n"},"as":"class"}],"input":{"op":"ScanInterface","interface":"Nameable","binding":"n"}}}"#;

    assert_eq!(plan.to_json().unwrap(), expected);
    assert_eq!(Plan::from_json(expected).unwrap(), plan);
}

#[test]
fn scan_interface_text_round_trip_uses_schema_resolution() {
    let fixture = Fixture::new();
    let expected = plan(Operator::Project {
        exprs: vec![
            ProjectionItem {
                expr: Expr::Col("n.name".to_owned()),
                alias: "name".to_owned(),
            },
            ProjectionItem {
                expr: Expr::ClassOf("n".to_owned()),
                alias: "kind".to_owned(),
            },
        ],
        input: Box::new(interface_scan("Nameable", "n")),
    });

    let text = print_plan(&expected).unwrap();
    assert_eq!(
        text,
        "nodes(Nameable) as n | project n.name as name, classof(n) as kind"
    );
    assert_eq!(
        parse_with_schema(&text, &fixture.schema()).unwrap(),
        Parsed::Query(expected)
    );
}

#[test]
fn scan_interface_typing_exposes_exact_declared_columns() {
    let fixture = Fixture::new();
    let valid = plan(Operator::Project {
        exprs: vec![
            ProjectionItem {
                expr: Expr::Col("N.NAME".to_owned()),
                alias: "name".to_owned(),
            },
            ProjectionItem {
                expr: Expr::Col("n.score".to_owned()),
                alias: "score".to_owned(),
            },
        ],
        input: Box::new(interface_scan("nameable", "n")),
    });
    validate_with_schema(&valid, &fixture.schema()).unwrap();

    for unavailable in ["n.id", "n.private_note"] {
        let invalid = plan(Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col(unavailable.to_owned()),
                alias: "leak".to_owned(),
            }],
            input: Box::new(interface_scan("Nameable", "n")),
        });
        let error = validate_with_schema(&invalid, &fixture.schema()).unwrap_err();
        assert!(error.to_string().contains(unavailable), "{error}");
        assert!(error.to_string().contains("input schema"), "{error}");
    }
}

#[test]
fn scan_interface_zero_implementers_is_typing_legal() {
    let fixture = Fixture::new();
    validate_with_schema(&plan(interface_scan("Nameable", "n")), &fixture.schema()).unwrap();
}

#[test]
fn scan_interface_unknown_name_uses_folded_suggestion() {
    let fixture = Fixture::new();
    let error =
        validate_with_schema(&plan(interface_scan("Namable", "n")), &fixture.schema()).unwrap_err();
    let DevonError::NotFound { what } = error else {
        panic!("expected NotFound, got {error}");
    };
    assert_eq!(
        what,
        "interface `Namable` referenced by ScanInterface (did you mean `Nameable`?)"
    );
}

#[test]
fn scan_interface_shadow_collision_names_both_objects() {
    let mut fixture = Fixture::new();
    fixture.nodes.push(
        NodeTableSchema::new(
            "nameable".to_owned(),
            vec![column("id", LogicalType::Int64, true)],
        )
        .unwrap(),
    );
    let error = validate_with_schema(&plan(interface_scan("Nameable", "n")), &fixture.schema())
        .unwrap_err();
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert!(context.contains("Nameable"), "{context}");
    assert!(context.contains("nameable"), "{context}");
    assert!(context.contains("shadowed"), "{context}");

    let Parsed::Query(parsed) =
        parse_with_schema("nodes(Nameable) as n", &fixture.schema()).unwrap()
    else {
        panic!("expected a query");
    };
    assert!(matches!(parsed.plan, Operator::ScanNodes { .. }));
}

#[test]
fn scan_interface_classof_types_string_and_requires_interface_binding() {
    assert_eq!(
        expression_type(&Expr::ClassOf("n".to_owned()), &HashMap::new()).unwrap(),
        ExpressionType::Value(LogicalType::String)
    );

    let fixture = Fixture::new();
    let valid = plan(Operator::Project {
        exprs: vec![ProjectionItem {
            expr: Expr::ClassOf("n".to_owned()),
            alias: "class".to_owned(),
        }],
        input: Box::new(interface_scan("Nameable", "n")),
    });
    validate_with_schema(&valid, &fixture.schema()).unwrap();

    let invalid = plan(Operator::Project {
        exprs: vec![ProjectionItem {
            expr: Expr::ClassOf("p".to_owned()),
            alias: "class".to_owned(),
        }],
        input: Box::new(Operator::ScanNodes {
            table: "Person".to_owned(),
            binding: "p".to_owned(),
        }),
    });
    let error = validate_with_schema(&invalid, &fixture.schema()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("classof binding `p` is not an interface-scan binding"),
        "{error}"
    );
}
