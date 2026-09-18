//! TextScan canonical forms, score scope and collision admission.
use devondb_plan::{
    expr::Expr,
    ops::{Operator, Plan},
    text::{
        parser::{Parsed, parse},
        printer::print_plan,
    },
    validate::validate,
};
use devondb_types::{
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
};
fn query(text: &str) -> Plan {
    let Parsed::Query(plan) = parse(text).unwrap() else {
        panic!("query")
    };
    plan
}
fn schema() -> Vec<NodeTableSchema> {
    vec![
        NodeTableSchema::new(
            "Document".into(),
            vec![
                Column {
                    name: "id".into(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                },
                Column {
                    name: "body".into(),
                    ty: LogicalType::String,
                    primary_key: false,
                },
            ],
        )
        .unwrap(),
    ]
}
#[test]
fn canonical_text_json_and_contextual_identifiers() {
    let text =
        "textscan(Document.body, \"Rust graph\", k=10) as d | project d.id, scoreof(d) as score";
    let plan = query(text);
    validate(&plan, &schema(), &[]).unwrap();
    assert_eq!(print_plan(&plan).unwrap(), text);
    let json = plan.to_json().unwrap();
    assert!(json.contains(r#""scoreof":"d""#));
    assert!(json.contains(r#""op":"TextScan""#));
    assert_eq!(Plan::from_json(&json).unwrap(), plan);
    let old = "nodes(textscan) as scoreof | project scoreof.textscan";
    assert_eq!(print_plan(&query(old)).unwrap(), old);
}
#[test]
fn types_limits_and_recursive_score_scope() {
    for text in [
        "textscan(Document.body, \"rust\", k=0) as d",
        "textscan(Document.id, \"rust\", k=1) as d",
        "textscan(Document.missing, \"rust\", k=1) as d",
        "textscan(Missing.body, \"rust\", k=1) as d",
        "nodes(Document) as d | project scoreof(d)",
        "nodes(Document) as d | aggregate sum(scoreof(d)) as total",
        "nodes(Document) as d | aggregate sum(if(true, scoreof(unknown), 0)) as total",
        "textscan(Document.body, \"rust\", k=1) as d | aggregate sum(scoreof(d)) as total | project scoreof(d)",
    ] {
        assert!(validate(&query(text), &schema(), &[]).is_err(), "{text}");
    }
    for text in [
        "textscan(Document.body, \"rust\", k=1) as d | aggregate sum(scoreof(d)) as total",
        "textscan(Document.body, \"rust\", k=1) as d | project scalar(nodes(Document) as x | limit 1 | project scoreof(d))",
        "textscan(Document.body, \"rust\", k=1) as d | project 1 as literal | filter scoreof(d) > 0",
    ] {
        validate(&query(text), &schema(), &[]).unwrap();
    }
}
#[test]
fn projected_metadata_orients_reversed_join_keys() {
    let plan = query(
        "let other = textscan(Document.body, \"rust\", k=1) as r | project 2 as literal_right; textscan(Document.body, \"rust\", k=1) as l | project 1 as literal_left | join other on scoreof(r) = scoreof(l)",
    );
    validate(&plan, &schema(), &[]).unwrap();
    let Operator::HashJoin { on, .. } = &plan.plan else {
        panic!("join")
    };
    assert_eq!(on[0].left, Expr::ScoreOf("l".into()));
    assert_eq!(query(&print_plan(&plan).unwrap()), plan);
}
#[test]
fn private_alias_rejected_globally_only_for_text_plans() {
    for alias in [
        "\0devondb-scoreof\0t",
        "\0DEVONDB-SCOREOF\0t",
        "\0DevonDB-ScoreOf\0t",
    ] {
        let mut old = query("nodes(Document) as x | project 7 as forged");
        let Operator::Project { exprs, .. } = &mut old.plan else {
            panic!("project")
        };
        exprs[0].alias = alias.into();
        validate(&old, &schema(), &[]).unwrap();
        let mut direct = query("textscan(Document.body, \"rust\", k=1) as t | project 7 as forged");
        let Operator::Project { exprs, .. } = &mut direct.plan else {
            panic!("project")
        };
        exprs[0].alias = alias.into();
        assert!(
            validate(
                &Plan::from_json(&direct.to_json().unwrap()).unwrap(),
                &schema(),
                &[]
            )
            .is_err()
        );
        let mut joined = query(
            "let other = nodes(Document) as x | project x.id; textscan(Document.body, \"rust\", k=1) as t | join other on t.id = x.id",
        );
        let Operator::HashJoin { right, .. } = &mut joined.plan else {
            panic!("join")
        };
        **right = old.plan;
        assert!(
            validate(&joined, &schema(), &[])
                .unwrap_err()
                .to_string()
                .contains("private score")
        );
    }
}
