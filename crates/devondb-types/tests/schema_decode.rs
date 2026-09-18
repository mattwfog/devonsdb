use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema, RelTableSchema};

fn decode_node(json: &str) -> String {
    serde_json::from_str::<NodeTableSchema>(json)
        .expect_err("invalid node schema must fail during deserialization")
        .to_string()
}

fn decode_rel(json: &str) -> String {
    serde_json::from_str::<RelTableSchema>(json)
        .expect_err("invalid relationship schema must fail during deserialization")
        .to_string()
}

#[test]
fn node_decode_requires_exactly_one_primary_key() {
    let zero = decode_node(
        r#"{"name":"Person","columns":[{"name":"id","ty":"Int64","primary_key":false}]}"#,
    );
    assert!(
        zero.contains("node table `Person` must have exactly one primary key column; found []"),
        "{zero}"
    );

    let two = decode_node(
        r#"{"name":"Person","columns":[{"name":"id","ty":"Int64","primary_key":true},{"name":"external_id","ty":"String","primary_key":true}]}"#,
    );
    assert!(
        two.contains(
            "node table `Person` must have exactly one primary key column; found [\"id\", \"external_id\"]"
        ),
        "{two}"
    );
}

#[test]
fn schema_decode_rejects_fold_duplicate_columns() {
    let error = decode_node(
        r#"{"name":"Person","columns":[{"name":"Name","ty":"Int64","primary_key":true},{"name":"name","ty":"String","primary_key":false}]}"#,
    );
    assert!(
        error.contains("column `name` is duplicated in node table `Person`"),
        "{error}"
    );
}

#[test]
fn relationship_decode_rejects_primary_keys() {
    let error = decode_rel(
        r#"{"name":"Knows","from":"Person","to":"Person","columns":[{"name":"since","ty":"Int64","primary_key":true}]}"#,
    );
    assert!(
        error.contains("column `since` in relationship table `Knows` cannot be a primary key"),
        "{error}"
    );
}

#[test]
fn schema_decode_rejects_invalid_decimal_bounds() {
    let error = decode_node(
        r#"{"name":"Amounts","columns":[{"name":"id","ty":"Int64","primary_key":true},{"name":"amount","ty":{"Decimal":{"precision":0,"scale":0}},"primary_key":false}]}"#,
    );
    assert!(
        error.contains(
            "column `amount` in node table `Amounts` has Decimal precision 0; precision must be between 1 and 38"
        ),
        "{error}"
    );
}

#[test]
fn valid_schemas_round_trip_without_changing_json_shape() {
    let node = NodeTableSchema::new(
        "Person".to_owned(),
        vec![Column {
            name: "id".to_owned(),
            ty: LogicalType::Int64,
            primary_key: true,
        }],
    )
    .unwrap();
    let rel = RelTableSchema::new(
        "Knows".to_owned(),
        "Person".to_owned(),
        "Person".to_owned(),
        Vec::new(),
    )
    .unwrap();

    let node_json = serde_json::to_string(&node).unwrap();
    let rel_json = serde_json::to_string(&rel).unwrap();
    assert_eq!(
        node_json,
        r#"{"name":"Person","columns":[{"name":"id","ty":"Int64","primary_key":true}]}"#
    );
    assert_eq!(
        rel_json,
        r#"{"name":"Knows","from":"Person","to":"Person","columns":[]}"#
    );
    assert_eq!(
        serde_json::from_str::<NodeTableSchema>(&node_json).unwrap(),
        node
    );
    assert_eq!(
        serde_json::from_str::<RelTableSchema>(&rel_json).unwrap(),
        rel
    );
}
