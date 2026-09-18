//! Read-only schema introspection for UI and tooling surfaces.
//!
//! Design: `docs/UI.md` § 7. The summary is a serializable snapshot of
//! the committed catalog; column types use the plan-IR `Display`
//! spelling so consumers never parse the engine's internal type
//! encoding.

use serde::Serialize;

use devondb_plan::{ops::Plan, text::printer::print_plan};
use devondb_storage::catalog::{NodeClassEntry, PinEntry, RelClassEntry};
use devondb_types::{
    DevonResult,
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema, fold},
};

use crate::database::Database;

const IRREGULAR_PLURALS: [(&str, &str); 8] = [
    ("person", "people"),
    ("child", "children"),
    ("man", "men"),
    ("woman", "women"),
    ("foot", "feet"),
    ("tooth", "teeth"),
    ("goose", "geese"),
    ("mouse", "mice"),
];

/// One column of a node or relationship table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColumnSummary {
    /// Column name.
    pub name: String,
    /// Plan-IR type spelling (`Int64`, `String`, `Vector(768)`, …).
    #[serde(rename = "type")]
    pub ty: String,
    /// True for a node table's primary-key column.
    pub primary_key: bool,
}

/// Schema of one node table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeTableSummary {
    /// Table name.
    pub name: String,
    /// Columns in schema order.
    pub columns: Vec<ColumnSummary>,
}

/// Schema of one relationship table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RelTableSummary {
    /// Table name.
    pub name: String,
    /// Source node table.
    pub from: String,
    /// Target node table.
    pub to: String,
    /// Property columns in schema order.
    pub columns: Vec<ColumnSummary>,
}

/// One required property exposed by an ontology interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InterfaceColumnSummary {
    /// Required property name.
    pub name: String,
    /// Plan-IR type spelling.
    #[serde(rename = "type")]
    pub ty: String,
}

/// One declared ontology interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InterfaceSummary {
    /// Interface name.
    pub name: String,
    /// Required properties in declaration order.
    pub columns: Vec<InterfaceColumnSummary>,
}

/// Consumer-ready ontology metadata for one node table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeClassSummary {
    /// Node table annotated by this class.
    pub table: String,
    /// Singular display name, declared or derived from `table`.
    pub display: String,
    /// Plural spelling, declared or derived from the folded table name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plural: Option<String>,
    /// Label column, declared or derived by the `name`/`title`/`label`
    /// string-column heuristic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Declared leading columns for cards and grids.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub summary: Vec<String>,
    /// Optional declared display color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Optional declared description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared interfaces in declaration order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<String>,
}

/// Ontology metadata for one relationship table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RelClassSummary {
    /// Relationship table annotated by this class.
    pub table: String,
    /// Optional forward verb phrase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verb: Option<String>,
    /// Optional inverse verb phrase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inverse: Option<String>,
}

/// The ontology section of a [`SchemaSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClassesSummary {
    /// Declared interfaces in catalog order.
    pub interfaces: Vec<InterfaceSummary>,
    /// Consumer-ready node classes in node-table order, including derived
    /// defaults for node tables without an explicit class declaration.
    pub node_classes: Vec<NodeClassSummary>,
    /// Declared relationship classes in catalog order.
    pub rel_classes: Vec<RelClassSummary>,
}

/// One pinned plan exposed through schema introspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PinSummary {
    /// Pin name in its declaration spelling.
    pub name: String,
    /// Original source text retained as provenance.
    pub text: String,
    /// Canonical engine-printed text of the stored plan.
    pub canonical: String,
    /// Plan decode or print failure, when `canonical` could not be produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A serializable summary of the committed catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchemaSummary {
    /// Node tables in catalog order.
    pub node_tables: Vec<NodeTableSummary>,
    /// Relationship tables in catalog order.
    pub rel_tables: Vec<RelTableSummary>,
    /// Consumer-ready ontology metadata, including derived node classes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classes: Option<ClassesSummary>,
    /// Pinned plans in catalog order, omitted when none exist.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<PinSummary>,
}

fn column_summary(column: &Column) -> ColumnSummary {
    ColumnSummary {
        name: column.name.clone(),
        ty: column.ty.to_string(),
        primary_key: column.primary_key,
    }
}

fn classes_summary(catalog: &devondb_storage::catalog::Catalog) -> Option<ClassesSummary> {
    if catalog.node_tables().is_empty() && catalog.ontology().is_none() {
        return None;
    }
    Some(ClassesSummary {
        interfaces: catalog.ontology().map_or_else(Vec::new, |ontology| {
            ontology
                .interfaces
                .iter()
                .map(|interface| InterfaceSummary {
                    name: interface.name.clone(),
                    columns: interface
                        .columns
                        .iter()
                        .map(|column| InterfaceColumnSummary {
                            name: column.name.clone(),
                            ty: column.ty.to_string(),
                        })
                        .collect(),
                })
                .collect()
        }),
        node_classes: catalog
            .node_tables()
            .iter()
            .map(|table| node_class_summary(table, catalog.node_class(table.name())))
            .collect(),
        rel_classes: catalog.ontology().map_or_else(Vec::new, |ontology| {
            ontology.rel_classes.iter().map(rel_class_summary).collect()
        }),
    })
}

fn node_class_summary(
    table: &NodeTableSchema,
    declaration: Option<&NodeClassEntry>,
) -> NodeClassSummary {
    NodeClassSummary {
        table: declaration.map_or_else(|| table.name().to_owned(), |class| class.table.clone()),
        display: declaration
            .and_then(|class| class.display.clone())
            .unwrap_or_else(|| table.name().to_owned()),
        plural: Some(
            declaration
                .and_then(|class| class.plural.clone())
                .unwrap_or_else(|| derived_plural(table.name())),
        ),
        label: declaration
            .and_then(|class| class.label.clone())
            .or_else(|| derived_label(table)),
        summary: declaration.map_or_else(Vec::new, |class| class.summary.clone()),
        color: declaration.and_then(|class| class.color.clone()),
        description: declaration.and_then(|class| class.description.clone()),
        implements: declaration.map_or_else(Vec::new, |class| class.implements.clone()),
    }
}

fn derived_plural(table_name: &str) -> String {
    let folded = fold(table_name);
    if let Some((_, plural)) = IRREGULAR_PLURALS
        .iter()
        .find(|(singular, _)| *singular == folded)
    {
        return (*plural).to_owned();
    }
    if let Some(stem) = folded.strip_suffix('y') {
        return format!("{stem}ies");
    }
    if ["s", "x", "z", "ch", "sh"]
        .iter()
        .any(|suffix| folded.ends_with(suffix))
    {
        return format!("{folded}es");
    }
    format!("{folded}s")
}

fn derived_label(table: &NodeTableSchema) -> Option<String> {
    ["name", "title", "label"]
        .into_iter()
        .find_map(|candidate| {
            table
                .columns()
                .iter()
                .find(|column| {
                    column.ty == LogicalType::String && fold(&column.name).as_ref() == candidate
                })
                .map(|column| column.name.clone())
        })
}

fn rel_class_summary(class: &RelClassEntry) -> RelClassSummary {
    RelClassSummary {
        table: class.table.clone(),
        verb: class.verb.clone(),
        inverse: class.inverse.clone(),
    }
}

fn pin_summaries(catalog: &devondb_storage::catalog::Catalog) -> Vec<PinSummary> {
    catalog.pins().iter().map(pin_summary).collect()
}

fn pin_summary(pin: &PinEntry) -> PinSummary {
    let (canonical, error) = match canonical_pin_text(pin) {
        Ok(canonical) => (canonical, None),
        Err(error) => (String::new(), Some(error.to_string())),
    };
    PinSummary {
        name: pin.name.clone(),
        text: pin.text.clone(),
        canonical,
        error,
    }
}

fn canonical_pin_text(pin: &PinEntry) -> DevonResult<String> {
    pin.plan_json()
        .and_then(|json| Plan::from_json(&json))
        .and_then(|plan| print_plan(&plan))
}

impl Database {
    /// Summarizes the committed catalog at a freshly pinned snapshot.
    pub fn schema_summary(&self) -> SchemaSummary {
        let snapshot = self.snapshot();
        let catalog = &snapshot.state.catalog;
        SchemaSummary {
            node_tables: catalog
                .node_tables()
                .iter()
                .map(|table| NodeTableSummary {
                    name: table.name().to_string(),
                    columns: table.columns().iter().map(column_summary).collect(),
                })
                .collect(),
            rel_tables: catalog
                .rel_tables()
                .iter()
                .map(|table| RelTableSummary {
                    name: table.name().to_string(),
                    from: table.from().to_string(),
                    to: table.to().to_string(),
                    columns: table.columns().iter().map(column_summary).collect(),
                })
                .collect(),
            classes: classes_summary(catalog),
            pins: pin_summaries(catalog),
        }
    }
}
