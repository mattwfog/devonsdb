//! DevonPlan DDL/DML statements (`docs/PLAN_IR.md` § Statements, binding).
//!
//! Statements deliberately use their own `{ "v", "stmt" }` envelope rather
//! than sharing queries' `{ "v", "plan" }` envelope. Each top-level JSON shape
//! therefore has one purpose and can evolve behind the same version gate.

use crate::{
    expr::{json_with_exact_numbers, run_decode_defenses},
    ops::ensure_supported_version,
};
use devondb_types::{
    DevonError, DevonResult, logical_type::LogicalType, schema::Column, value::Value,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// One relationship row to insert.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelRow {
    /// The primary-key value of the source node.
    pub from_key: Value,
    /// The primary-key value of the destination node.
    pub to_key: Value,
    /// Relationship-property values in schema order.
    pub values: Vec<Value>,
}

/// One `set <column> = <literal>` assignment in an update statement.
///
/// Values are literals, never expressions — the same law as insert rows
/// (`docs/UI.md` §12.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetItem {
    /// The column being assigned.
    pub column: String,
    /// The literal value assigned to it.
    pub value: Value,
}

/// One required property in an ontology interface.
///
/// Interface properties deliberately omit the table-only `primary_key`
/// marker: an interface specifies a name and logical type, not storage
/// identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterfaceColumn {
    /// Required property name.
    pub name: String,
    /// Required logical type.
    pub ty: LogicalType,
}

/// A DevonPlan v0 data-definition or data-manipulation statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stmt")]
pub enum Statement {
    /// Define a node table.
    CreateNodeTable {
        /// The new table's name.
        name: String,
        /// The new table's columns in declaration order.
        columns: Vec<Column>,
    },
    /// Define a relationship table.
    CreateRelTable {
        /// The new table's name.
        name: String,
        /// The source node-table name.
        from: String,
        /// The destination node-table name.
        to: String,
        /// The new table's property columns in declaration order.
        columns: Vec<Column>,
    },
    /// Insert rows into a node table.
    ///
    /// Rows are data, not expressions, so their [`Value`]s use the type's
    /// derived tagged serde form rather than expressions' natural JSON form.
    InsertNode {
        /// The node table receiving the rows.
        table: String,
        /// Row values in schema-column order.
        rows: Vec<Vec<Value>>,
    },
    /// Insert rows into a relationship table.
    ///
    /// As with node inserts, row values are data and use [`Value`]'s derived
    /// tagged serde representation.
    InsertRel {
        /// The relationship table receiving the rows.
        table: String,
        /// The relationship rows to insert.
        rows: Vec<RelRow>,
    },
    /// Insert-or-replace rows in a node table by primary key: a row whose
    /// primary-key value is absent inserts; a present key replaces that row
    /// wholesale, exactly as [`Statement::UpdateNode`] would. Text form:
    /// `upsert <Table> values (<lit>, …)[, (<lit>, …)…]`.
    ///
    /// As with inserts, row values are data and use [`Value`]'s derived
    /// tagged serde representation.
    UpsertNode {
        /// The node table receiving the rows.
        table: String,
        /// Row values in schema-column order.
        rows: Vec<Vec<Value>>,
    },
    /// Update one node row addressed by its primary key (`docs/UI.md`
    /// §12.1). Text form:
    /// `update <Table> set <col> = <lit>[, …] where <pk-col> = <lit>`.
    UpdateNode {
        /// The node table holding the row.
        table: String,
        /// The column assignments, in statement order.
        set: Vec<SetItem>,
        /// The `where` column as written; validated to be the primary key
        /// at execution.
        key_column: String,
        /// The primary-key literal addressing the row.
        key: Value,
    },
    /// Delete one node row addressed by its primary key (`docs/UI.md`
    /// §12.1). Text form: `delete from <Table> where <pk-col> = <lit>`.
    DeleteNode {
        /// The node table holding the row.
        table: String,
        /// The `where` column as written; validated to be the primary key
        /// at execution.
        key_column: String,
        /// The primary-key literal addressing the row.
        key: Value,
    },
    /// Delete one node row and every visible incident relationship edge,
    /// addressed by the node's primary key (`docs/DETACH_DELETE.md`). Text
    /// form: `detach delete from <Table> where <pk-col> = <lit>`.
    DetachDeleteNode {
        /// The node table holding the row.
        table: String,
        /// The `where` column as written; validated to be the primary key
        /// at execution.
        key_column: String,
        /// The primary-key literal addressing the row.
        key: Value,
    },
    /// Bulk-load a node table from an RFC 4180 CSV file, bypassing the WAL
    /// under the bulk fence (`docs/SCALE.md` §5). Text form:
    /// `copy <Table> from "<path>"` with an optional `sort by <column>`.
    CopyNode {
        /// The node table receiving the load.
        table: String,
        /// Filesystem path of the CSV file, as written in the statement.
        path: String,
        /// Optional sort-on-load column (`docs/SCALE.md` §5.4).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sort_by: Option<String>,
    },
    /// Declare a named ontology interface.
    CreateInterface {
        /// Interface name, unique under ASCII folding.
        name: String,
        /// Required properties in declaration order.
        columns: Vec<InterfaceColumn>,
    },
    /// Annotate a node or relationship table as an ontology class.
    CreateClass {
        /// Existing node or relationship table being annotated.
        table: String,
        /// Optional singular display name for a node class.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<String>,
        /// Optional plural spelling for a node class.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plural: Option<String>,
        /// Optional node label-column reference.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Optional node summary-column references.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summary: Vec<String>,
        /// Optional node display color.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        /// Optional node class description.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Optional forward relationship verb.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verb: Option<String>,
        /// Optional inverse relationship verb.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inverse: Option<String>,
        /// Optional interfaces implemented by a node class.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        implements: Vec<String>,
    },
    /// Create a persistent HNSW index over one vector column through the
    /// explicit, pin-able DDL surface described in `docs/HNSW.md` §11.
    ///
    /// Topology parameters use the §2.2 writer defaults; the level seed is
    /// derived deterministically by the engine. Text form:
    /// `create hnsw index <name> on <table>.<column> metric <metric>`.
    CreateHnswIndex {
        /// The new index's name, unique among indexes.
        name: String,
        /// The node table whose column is indexed.
        table: String,
        /// The vector column to index.
        column: String,
        /// The distance metric the index serves.
        metric: crate::expr::Metric,
    },
    /// Persist a named query plan for deterministic later execution.
    PinPlan {
        /// Pin name, unique under ASCII folding.
        name: String,
        /// Original input text retained as provenance.
        text: String,
        /// Canonical query plan captured by the pin.
        plan: crate::ops::Plan,
    },
    /// Remove a named pinned query plan.
    UnpinPlan {
        /// Pin name resolved under ASCII folding.
        name: String,
    },
}

/// A versioned envelope containing one DevonPlan statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatementEnvelope {
    /// The DevonPlan version used by this statement.
    pub v: u32,
    /// The enclosed DDL or DML statement.
    pub stmt: Statement,
}

#[derive(Deserialize)]
struct RawStatementEnvelope {
    v: u32,
    stmt: JsonValue,
}

impl StatementEnvelope {
    /// Decodes a statement from its canonical JSON envelope.
    ///
    /// Applies the same decode defenses as the query path
    /// (`crate::ops::Plan::from_json`): the integer-literal range scan and
    /// duplicate-field typed pre-pass cover the envelope, every embedded
    /// plan, and every `Value` payload, because the exact-number rebuild's
    /// object map would otherwise hide duplicates by last-wins insertion.
    pub fn from_json(json: &str) -> DevonResult<Self> {
        let raw: RawStatementEnvelope =
            serde_json::from_str(json).map_err(invalid_statement_json)?;
        ensure_supported_version(raw.v)?;
        run_decode_defenses::<Self>(json).map_err(invalid_statement_json_context)?;
        let exact_json = json_with_exact_numbers(json).map_err(invalid_statement_json_context)?;
        let exact_raw: RawStatementEnvelope =
            serde_json::from_value(exact_json).map_err(invalid_statement_json)?;
        let stmt: Statement =
            serde_json::from_value(exact_raw.stmt).map_err(invalid_statement_json)?;
        // A pin embeds a complete plan envelope; gate its version exactly
        // as the envelope's own `v` is gated (docs/PLAN_IR.md § Versioning).
        if let Statement::PinPlan { plan, .. } = &stmt {
            ensure_supported_version(plan.v)?;
        }
        Ok(Self { v: raw.v, stmt })
    }

    /// Encodes this statement in canonical compact JSON with stable field order.
    pub fn to_json(&self) -> DevonResult<String> {
        serde_json::to_string(self).map_err(invalid_statement_json)
    }
}

fn invalid_statement_json(error: serde_json::Error) -> DevonError {
    invalid_statement_json_context(error)
}

fn invalid_statement_json_context(error: impl std::fmt::Display) -> DevonError {
    DevonError::InvalidArgument {
        context: format!("invalid DevonPlan statement JSON: {error}"),
    }
}

#[cfg(test)]
mod ops_statement_tests {
    use super::{InterfaceColumn, RelRow, Statement, StatementEnvelope};
    use crate::ops::PLAN_VERSION;
    use devondb_types::{
        DevonError,
        logical_type::LogicalType,
        schema::{Column, NodeTableSchema},
        value::Value,
    };

    fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
        Column {
            name: name.into(),
            ty,
            primary_key,
        }
    }

    fn round_trip(statement: Statement) {
        let envelope = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: statement,
        };
        let json = envelope.to_json().unwrap();
        assert_eq!(StatementEnvelope::from_json(&json).unwrap(), envelope);
    }

    #[test]
    fn ops_all_statement_variants_round_trip() {
        let statements = vec![
            Statement::CreateNodeTable {
                name: "Person".into(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("name", LogicalType::String, false),
                ],
            },
            Statement::CreateRelTable {
                name: "Knows".into(),
                from: "Person".into(),
                to: "Person".into(),
                columns: vec![column("since", LogicalType::Int64, false)],
            },
            Statement::InsertNode {
                table: "Person".into(),
                rows: vec![vec![Value::Int64(1), Value::String("Ada".into())]],
            },
            Statement::InsertRel {
                table: "Knows".into(),
                rows: vec![RelRow {
                    from_key: Value::Int64(1),
                    to_key: Value::Int64(2),
                    values: vec![Value::Int64(1843)],
                }],
            },
            Statement::CreateInterface {
                name: "Nameable".into(),
                columns: vec![InterfaceColumn {
                    name: "name".into(),
                    ty: LogicalType::String,
                }],
            },
            Statement::CreateClass {
                table: "Person".into(),
                display: Some("Person".into()),
                plural: Some("people".into()),
                label: Some("name".into()),
                summary: vec!["name".into(), "age".into()],
                color: Some("#7aa2ff".into()),
                description: Some("a human".into()),
                verb: None,
                inverse: None,
                implements: vec!["Nameable".into()],
            },
            Statement::PinPlan {
                name: "ada friends".into(),
                text: "who does ada know".into(),
                plan: crate::ops::Plan {
                    v: PLAN_VERSION,
                    plan: crate::ops::Operator::ScanNodes {
                        table: "Person".into(),
                        binding: "person".into(),
                    },
                },
            },
            Statement::UnpinPlan {
                name: "ada friends".into(),
            },
            Statement::DetachDeleteNode {
                table: "Person".into(),
                key_column: "id".into(),
                key: Value::Int64(7),
            },
        ];

        for statement in statements {
            round_trip(statement);
        }
    }

    #[test]
    fn ops_insert_values_use_derived_value_serde() {
        let envelope = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::InsertNode {
                table: "Person".into(),
                rows: vec![vec![Value::Int64(1), Value::String("Ada".into())]],
            },
        };

        assert_eq!(
            envelope.to_json().unwrap(),
            r#"{"v":0,"stmt":{"stmt":"InsertNode","table":"Person","rows":[[{"Int64":1},{"String":"Ada"}]]}}"#
        );
    }

    #[test]
    fn detach_delete_statement_json_is_canonical() {
        let envelope = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::DetachDeleteNode {
                table: "Person".into(),
                key_column: "id".into(),
                key: Value::Int64(7),
            },
        };

        let json = envelope.to_json().unwrap();
        assert_eq!(
            json,
            r#"{"v":0,"stmt":{"stmt":"DetachDeleteNode","table":"Person","key_column":"id","key":{"Int64":7}}}"#
        );
        assert_eq!(StatementEnvelope::from_json(&json).unwrap(), envelope);
    }

    #[test]
    fn ontology_statement_json_is_canonical_and_omits_absent_clauses() {
        let interface = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::CreateInterface {
                name: "Nameable".into(),
                columns: vec![InterfaceColumn {
                    name: "name".into(),
                    ty: LogicalType::String,
                }],
            },
        };
        assert_eq!(
            interface.to_json().unwrap(),
            r#"{"v":0,"stmt":{"stmt":"CreateInterface","name":"Nameable","columns":[{"name":"name","ty":"String"}]}}"#
        );

        let class = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::CreateClass {
                table: "Person".into(),
                display: None,
                plural: None,
                label: None,
                summary: Vec::new(),
                color: None,
                description: None,
                verb: None,
                inverse: None,
                implements: Vec::new(),
            },
        };
        assert_eq!(
            class.to_json().unwrap(),
            r#"{"v":0,"stmt":{"stmt":"CreateClass","table":"Person"}}"#
        );
        assert_eq!(
            StatementEnvelope::from_json(&class.to_json().unwrap()).unwrap(),
            class
        );
    }

    #[test]
    fn pin_statement_json_is_canonical() {
        let pin = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::PinPlan {
                name: "ada friends".into(),
                text: "who does ada know".into(),
                plan: crate::ops::Plan {
                    v: PLAN_VERSION,
                    plan: crate::ops::Operator::ScanNodes {
                        table: "Person".into(),
                        binding: "person".into(),
                    },
                },
            },
        };
        assert_eq!(
            pin.to_json().unwrap(),
            r#"{"v":0,"stmt":{"stmt":"PinPlan","name":"ada friends","text":"who does ada know","plan":{"v":0,"plan":{"op":"ScanNodes","table":"Person","binding":"person"}}}}"#
        );

        let unpin = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::UnpinPlan {
                name: "ada friends".into(),
            },
        };
        assert_eq!(
            unpin.to_json().unwrap(),
            r#"{"v":0,"stmt":{"stmt":"UnpinPlan","name":"ada friends"}}"#
        );
    }

    #[test]
    fn ops_newer_statement_version_is_rejected_and_names_both_versions() {
        let error = StatementEnvelope::from_json(
            r#"{"v":1,"stmt":{"stmt":"InsertNode","table":"Person","rows":[]}}"#,
        )
        .unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };

        assert!(context.contains("version 1"));
        assert!(context.contains("version 0"));
    }

    #[test]
    fn ops_statement_deserialization_defers_schema_validation() {
        let json = concat!(
            r#"{"v":0,"stmt":{"stmt":"CreateNodeTable","name":"Person","columns":["#,
            r#"{"name":"id","ty":"Int64","primary_key":true},"#,
            r#"{"name":"id","ty":"String","primary_key":false}]}}"#
        );

        let envelope = StatementEnvelope::from_json(json).unwrap();
        let Statement::CreateNodeTable { name, columns } = envelope.stmt else {
            panic!("expected CreateNodeTable");
        };
        let error = NodeTableSchema::new(name, columns).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("id"));
        assert!(context.contains("duplicated"));
    }

    #[test]
    fn statement_decode_rejects_duplicate_fields_naming_the_field() {
        let json = r#"{"v":0,"stmt":{"stmt":"InsertNode","table":"A","table":"B","rows":[]}}"#;
        let error = StatementEnvelope::from_json(json).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("duplicate"), "{context}");
        assert!(context.contains("table"), "{context}");
    }

    #[test]
    fn statement_decode_rejects_duplicate_fields_inside_value_payloads() {
        let json =
            r#"{"v":0,"stmt":{"stmt":"InsertNode","table":"A","rows":[[{"Int64":1,"Int64":2}]]}}"#;
        assert!(
            StatementEnvelope::from_json(json).is_err(),
            "duplicate keys inside a `Value` payload must not decode"
        );
    }

    #[test]
    fn statement_decode_rejects_out_of_range_integer_literal_in_embedded_plan() {
        let embedded = concat!(
            r#"{"op":"Filter","predicate":{"gt":[{"col":"p.age"},"#,
            r#"{"lit":99999999999999999999999}]},"#,
            r#""input":{"op":"ScanNodes","table":"Person","binding":"p"}}"#
        );
        let statement_json = format!(
            r#"{{"v":0,"stmt":{{"stmt":"PinPlan","name":"p","text":"t","plan":{{"v":0,"plan":{embedded}}}}}}}"#
        );

        let error = StatementEnvelope::from_json(&statement_json).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("outside the Int64 range"), "{context}");

        // Same law, same wording as the query path.
        let query_error =
            crate::ops::Plan::from_json(&format!(r#"{{"v":0,"plan":{embedded}}}"#)).unwrap_err();
        assert!(
            query_error.to_string().contains("outside the Int64 range"),
            "{query_error}"
        );
    }

    #[test]
    fn pin_plan_embedded_newer_plan_version_is_rejected_like_the_envelope() {
        let json = concat!(
            r#"{"v":0,"stmt":{"stmt":"PinPlan","name":"p","text":"t","plan":{"v":1,"plan":"#,
            r#"{"op":"ScanNodes","table":"Person","binding":"p"}}}}"#
        );
        let error = StatementEnvelope::from_json(json).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert!(context.contains("version 1"), "{context}");
        assert!(context.contains("version 0"), "{context}");
    }
}
