//! Canonical compatibility and binding validation for relationship properties.
use devondb_plan::{
    ops::Plan,
    text::{
        parser::{Parsed, parse},
        printer::print_plan,
    },
    validate::validate,
};
use devondb_types::{
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema, RelTableSchema},
};

fn query(text: &str) -> Plan {
    let Parsed::Query(plan) = parse(text).unwrap() else {
        panic!("expected query")
    };
    plan
}

fn schemas() -> (Vec<NodeTableSchema>, Vec<RelTableSchema>) {
    let nodes = vec![
        NodeTableSchema::new(
            "Person".into(),
            vec![Column {
                name: "id".into(),
                ty: LogicalType::Int64,
                primary_key: true,
            }],
        )
        .unwrap(),
    ];
    let rels = vec![
        RelTableSchema::new(
            "Knows".into(),
            "Person".into(),
            "Person".into(),
            vec![Column {
                name: "weight".into(),
                ty: LogicalType::Int64,
                primary_key: false,
            }],
        )
        .unwrap(),
        RelTableSchema::new("Empty".into(), "Person".into(), "Person".into(), vec![]).unwrap(),
    ];
    (nodes, rels)
}

#[test]
fn canonical_old_bytes_and_additive_relationship_operator() {
    let old = query("nodes(Person) as p | expand Knows out as q");
    assert_eq!(
        old.to_json().unwrap(),
        r#"{"v":0,"plan":{"op":"Expand","rel":"Knows","direction":"out","from_binding":"p","binding":"q","input":{"op":"ScanNodes","table":"Person","binding":"p"}}}"#
    );
    let new = query("nodes(Person) as p | expand_rel Knows out from p as q via e");
    let expected = "nodes(Person) as p | expand_rel Knows out as q via e";
    assert_eq!(print_plan(&new).unwrap(), expected);
    assert_eq!(query(expected), new);
    assert_eq!(
        new.to_json().unwrap(),
        r#"{"v":0,"plan":{"op":"ExpandRel","rel":"Knows","direction":"out","from_binding":"p","binding":"q","rel_binding":"e","input":{"op":"ScanNodes","table":"Person","binding":"p"}}}"#
    );
    assert_eq!(Plan::from_json(&new.to_json().unwrap()).unwrap(), new);
    let identifiers = "nodes(expand_rel) as via | project via.expand_rel";
    assert_eq!(print_plan(&query(identifiers)).unwrap(), identifiers);
}

#[test]
fn binding_names_types_and_node_identity_are_enforced() {
    let (nodes, rels) = schemas();
    let valid = query(
        "nodes(Person) as p | expand_rel Knows out as q via e | filter E.WEIGHT > 0 | aggregate sum(e.weight) as total",
    );
    validate(&valid, &nodes, &rels).unwrap();
    for text in [
        "nodes(Person) as p | expand_rel Knows out as q via P",
        "nodes(Person) as p | expand_rel Knows out as q via Q",
        "nodes(Person) as p | expand_rel Empty out as q via e | expand Knows out as E",
        "nodes(Person) as p | expand_rel Knows out as q via e | expand Knows out from e as r",
        "nodes(Person) as p | expand_rel Knows out as q via e | project classof(e)",
        "nodes(Person) as p | expand_rel Knows out as q via e | project e.missing",
    ] {
        assert!(validate(&query(text), &nodes, &rels).is_err(), "{text}");
    }
}

#[test]
fn joins_and_scalar_queries_round_trip_without_binding_collisions() {
    let text = "let other = nodes(Person) as r; nodes(Person) as p | expand_rel Knows out as q via j1 | join other on j1.weight = r.id | project scalar(nodes(Person) as z | filter z.id = j1.weight | project z.id)";
    let plan = query(text);
    let printed = print_plan(&plan).unwrap();
    assert!(printed.starts_with("let j2 = "), "{printed}");
    assert_eq!(query(&printed), plan);
    let (nodes, rels) = schemas();
    validate(&plan, &nodes, &rels).unwrap();
    let bad = query(
        "nodes(Person) as p | expand_rel Empty out as q via e | project scalar(nodes(Person) as e | project e.id)",
    );
    assert!(validate(&bad, &nodes, &rels).is_err());
}
