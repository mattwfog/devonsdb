//! DevonPlan query operators and the versioned plan envelope
//! (`docs/PLAN_IR.md` § Query operators + § Canonical JSON form, binding).

use crate::expr::{Expr, Metric, json_with_exact_numbers, run_decode_defenses};
use devondb_types::{DevonError, DevonResult, GeoPoint};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value as JsonValue;
use std::fmt;

/// The newest DevonPlan version understood by this crate.
pub const PLAN_VERSION: u32 = 0;

/// The direction in which an [`Operator::Expand`] follows relationships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Follow relationships from their source to their destination.
    Out,
    /// Follow relationships from their destination to their source.
    In,
    /// Follow relationships in either direction.
    Both,
}

/// The ordering applied by a [`SortKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    /// Ascending order.
    Asc,
    /// Descending order.
    Desc,
}

/// An aggregate function in an [`Operator::Aggregate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AggregateFunction {
    /// Count input values.
    Count,
    /// Sum input values.
    Sum,
    /// Select the minimum input value.
    Min,
    /// Select the maximum input value.
    Max,
    /// Average input values.
    Avg,
    /// Select the exact continuous median of Decimal input values.
    #[serde(rename = "percentile_cont")]
    PercentileCont,
}

/// One expression and output name in a projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionItem {
    /// The expression to evaluate.
    pub expr: Expr,
    /// The name assigned to the resulting column.
    #[serde(rename = "as")]
    pub alias: String,
}

/// One expression and ordering in a sort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SortKey {
    /// The expression whose value is used as a key.
    pub expr: Expr,
    /// The order applied to the key.
    pub order: SortOrder,
}

/// One aggregate expression and output name.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateItem {
    /// The aggregate function to apply.
    pub function: AggregateFunction,
    /// The expression supplying input values.
    pub expr: Expr,
    /// The name assigned to the aggregate result.
    pub alias: String,
}

impl Serialize for AggregateItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let percentile = self.function == AggregateFunction::PercentileCont;
        let mut item = serializer.serialize_map(Some(if percentile { 4 } else { 3 }))?;
        item.serialize_entry("fn", &self.function)?;
        if percentile {
            item.serialize_entry("fraction", "0.5")?;
        }
        item.serialize_entry("expr", &self.expr)?;
        item.serialize_entry("as", &self.alias)?;
        item.end()
    }
}

struct AggregateItemVisitor;

impl<'de> Visitor<'de> for AggregateItemVisitor {
    type Value = AggregateItem;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an exact DevonPlan aggregate item object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut function = None;
        let mut fraction: Option<String> = None;
        let mut expr = None;
        let mut alias = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "fn" => read_once(&mut function, "fn", &mut map)?,
                "fraction" => read_once(&mut fraction, "fraction", &mut map)?,
                "expr" => read_once(&mut expr, "expr", &mut map)?,
                "as" => read_once(&mut alias, "as", &mut map)?,
                _ => return Err(de::Error::unknown_field(&key, AGGREGATE_ITEM_FIELDS)),
            }
        }
        let function = function.ok_or_else(|| de::Error::missing_field("fn"))?;
        validate_aggregate_fraction::<A::Error>(function, fraction.as_deref())?;
        Ok(AggregateItem {
            function,
            expr: expr.ok_or_else(|| de::Error::missing_field("expr"))?,
            alias: alias.ok_or_else(|| de::Error::missing_field("as"))?,
        })
    }
}

const AGGREGATE_ITEM_FIELDS: &[&str] = &["fn", "fraction", "expr", "as"];

fn read_once<'de, A, T>(
    slot: &mut Option<T>,
    field: &'static str,
    map: &mut A,
) -> Result<(), A::Error>
where
    A: MapAccess<'de>,
    T: Deserialize<'de>,
{
    if slot.is_some() {
        return Err(de::Error::duplicate_field(field));
    }
    *slot = Some(map.next_value()?);
    Ok(())
}

fn validate_aggregate_fraction<E: de::Error>(
    function: AggregateFunction,
    fraction: Option<&str>,
) -> Result<(), E> {
    match (function, fraction) {
        (AggregateFunction::PercentileCont, Some("0.5")) => Ok(()),
        (AggregateFunction::PercentileCont, Some(value)) => Err(E::custom(format!(
            "aggregate `percentile_cont` fraction must be exactly `0.5`, got `{value}`"
        ))),
        (AggregateFunction::PercentileCont, None) => Err(E::missing_field("fraction")),
        (_, Some(_)) => Err(E::custom(
            "aggregate field `fraction` is only valid for `percentile_cont`",
        )),
        (_, None) => Ok(()),
    }
}

impl<'de> Deserialize<'de> for AggregateItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(AggregateItemVisitor)
    }
}

/// A DevonPlan v0 logical query operator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Operator {
    /// Scan all nodes in a node table.
    ScanNodes {
        /// The node table to scan.
        table: String,
        /// The binding assigned to scanned nodes.
        binding: String,
    },
    /// Rank one String column by deterministic BM25, with primary-key ties.
    TextScan {
        /// The node table to search.
        table: String,
        /// The String column to tokenize.
        column: String,
        /// Query text, interpreted as data.
        query: String,
        /// Positive maximum number of matching rows.
        k: u64,
        /// Binding assigned to ranked nodes and score provenance.
        binding: String,
    },
    /// Scan every node class that implements an ontology interface.
    ScanInterface {
        /// The ontology interface whose implementing classes are scanned.
        interface: String,
        /// The binding assigned to the interface-projected nodes.
        binding: String,
    },
    /// Follow a relationship table from nodes already in the input.
    Expand {
        /// The relationship table to traverse.
        rel: String,
        /// The traversal direction.
        direction: Direction,
        /// The input binding from which traversal starts.
        from_binding: String,
        /// The binding assigned to reached nodes.
        binding: String,
        /// The input operator.
        input: Box<Operator>,
    },
    /// Follow relationships, binding both reached nodes and edge properties.
    ExpandRel {
        /// The relationship table to traverse.
        rel: String,
        /// The traversal direction.
        direction: Direction,
        /// The input node binding from which traversal starts.
        from_binding: String,
        /// The binding assigned to reached nodes.
        binding: String,
        /// The binding assigned to relationship property values.
        rel_binding: String,
        /// The input operator.
        input: Box<Operator>,
    },
    /// Retain input rows for which a predicate is true.
    Filter {
        /// The predicate evaluated for each input row.
        predicate: Expr,
        /// The input operator.
        input: Box<Operator>,
    },
    /// Compute named output columns.
    Project {
        /// The expressions and names of the output columns.
        exprs: Vec<ProjectionItem>,
        /// The input operator.
        input: Box<Operator>,
    },
    /// Order input rows by one or more keys.
    Sort {
        /// The keys, in precedence order.
        keys: Vec<SortKey>,
        /// The input operator.
        input: Box<Operator>,
    },
    /// Truncate an input stream to a row range.
    Limit {
        /// The maximum number of rows to produce.
        count: u64,
        /// The optional number of rows to skip first.
        #[serde(skip_serializing_if = "Option::is_none")]
        offset: Option<u64>,
        /// The input operator.
        input: Box<Operator>,
    },
    /// Group input rows and compute aggregate expressions.
    Aggregate {
        /// Expressions defining the groups.
        group_by: Vec<Expr>,
        /// Aggregate expressions computed for every group.
        aggs: Vec<AggregateItem>,
        /// The input operator.
        input: Box<Operator>,
    },
    /// Find the nearest node vectors in a table.
    KnnScan {
        /// The node table to search.
        table: String,
        /// The vector column to search.
        column: String,
        /// The literal or scalar-subquery vector source.
        query: KnnVectorSource,
        /// The maximum number of neighbors to produce.
        k: u64,
        /// The vector-distance metric.
        metric: Metric,
        /// Whether an installed index may serve this scan approximately.
        /// The absent-field spelling is `exact`, so every pre-existing and
        /// pinned plan keeps exact semantics byte-for-byte
        /// (`docs/HNSW.md` §11).
        #[serde(default, skip_serializing_if = "KnnMode::is_exact")]
        mode: KnnMode,
    },
    /// Emit the nodes whose GeoPoint column lies within an inclusive
    /// great-circle radius of a center point
    /// (`docs/PLAN_IR.md` § within semantics; execution law
    /// `docs/GEO.md` §7).
    WithinScan {
        /// The node table to scan.
        table: String,
        /// The GeoPoint column measured against `center`.
        column: String,
        /// The canonical center point (validating serde — canonical
        /// object form, same folding rules as stored values).
        center: GeoPoint,
        /// The inclusive great-circle radius in meters (finite, > 0;
        /// enforced by validation).
        meters: f64,
    },
    /// Join two input pipelines on equality keys
    /// (`docs/PLAN_IR.md` § join semantics).
    ///
    /// Output rows carry the union of both inputs' bindings. Ordering is
    /// deterministic: left-input row order, and for each left row its
    /// matches in right-input row order.
    HashJoin {
        /// Inner (default, absent-field spelling in canonical JSON) or
        /// left-outer.
        #[serde(default, skip_serializing_if = "JoinType::is_inner")]
        join: JoinType,
        /// The equality key pairs; every pair's `left` expression may
        /// reference only left-input bindings and `right` only
        /// right-input bindings, with exactly equal types (validation).
        on: Vec<JoinKey>,
        /// The left (probe) input operator.
        left: Box<Operator>,
        /// The right (build) input operator.
        right: Box<Operator>,
    },
}

/// The vector supplied to a [`Operator::KnnScan`].
///
/// Literal vectors retain their historical bare-array JSON spelling. Scalar
/// subqueries use the `{"scalar":{"plan":<operator>}}` encoding.
#[derive(Debug, Clone, PartialEq)]
pub enum KnnVectorSource {
    /// A vector embedded directly in the plan.
    Literal(Vec<f32>),
    /// An uncorrelated scalar subquery whose sole output is the query vector.
    Scalar {
        /// The embedded operator tree, without a nested plan envelope.
        plan: Box<Operator>,
    },
}

impl From<Vec<f32>> for KnnVectorSource {
    fn from(value: Vec<f32>) -> Self {
        Self::Literal(value)
    }
}

#[derive(Serialize)]
struct KnnScalarRef<'a> {
    plan: &'a Operator,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KnnScalarOwned {
    plan: Operator,
}

impl Serialize for KnnVectorSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Literal(vector) => vector.serialize(serializer),
            Self::Scalar { plan } => {
                let mut object = serializer.serialize_map(Some(1))?;
                object.serialize_entry("scalar", &KnnScalarRef { plan })?;
                object.end()
            }
        }
    }
}

struct KnnVectorSourceVisitor;

impl<'de> Visitor<'de> for KnnVectorSourceVisitor {
    type Value = KnnVectorSource;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a vector array or single-key scalar-subquery object")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut vector = Vec::new();
        while let Some(value) = sequence.next_element::<f32>()? {
            // serde's f32 path maps an overflowing literal (`1e999`) to
            // infinity without error; non-finite elements have no canonical
            // spelling and break the round-trip law.
            if !value.is_finite() {
                return Err(de::Error::custom(format!(
                    "vector element {} is not a finite f32 number",
                    vector.len()
                )));
            }
            vector.push(value);
        }
        Ok(KnnVectorSource::Literal(vector))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let key = map
            .next_key::<String>()?
            .ok_or_else(|| de::Error::custom("KNN vector source object has no key"))?;
        if key != "scalar" {
            return Err(de::Error::unknown_field(&key, &["scalar"]));
        }
        let scalar = map.next_value::<KnnScalarOwned>()?;
        if let Some(offending_key) = map.next_key::<String>()? {
            return Err(de::Error::custom(format!(
                "KNN vector source object has multiple keys; offending key `{offending_key}`"
            )));
        }
        Ok(KnnVectorSource::Scalar {
            plan: Box::new(scalar.plan),
        })
    }
}

impl<'de> Deserialize<'de> for KnnVectorSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(KnnVectorSourceVisitor)
    }
}

/// The join shape of a `HashJoin`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinType {
    /// Emit only left rows with at least one right match. The default and
    /// the absent-field spelling in plan JSON.
    #[default]
    Inner,
    /// Emit every left row; unmatched left rows carry NULL for every
    /// right-input column.
    Left,
}

impl JoinType {
    /// Returns whether this is the inner (default) join shape.
    #[must_use]
    pub fn is_inner(&self) -> bool {
        *self == Self::Inner
    }
}

/// One equality key pair of a `HashJoin`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinKey {
    /// The key expression over the left input's bindings.
    pub left: Expr,
    /// The key expression over the right input's bindings.
    pub right: Expr,
}

/// Whether a `KnnScan` may be served by an installed index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KnnMode {
    /// Exact scan: never uses an index. The default and the absent-field
    /// spelling in plan JSON.
    #[default]
    Exact,
    /// A matching installed index may serve the scan approximately; the
    /// uncovered tail is always scanned exactly (`docs/HNSW.md` §4).
    Approximate,
}

impl KnnMode {
    /// Returns whether this is the exact (default) mode.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        *self == Self::Exact
    }
}

/// A versioned DevonPlan query envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// The DevonPlan version used by this query.
    pub v: u32,
    /// The root of the logical operator tree.
    pub plan: Operator,
}

#[derive(Deserialize)]
struct RawPlan {
    v: u32,
    plan: JsonValue,
}

impl Plan {
    /// Decodes a plan from its canonical JSON envelope.
    pub fn from_json(json: &str) -> DevonResult<Self> {
        let raw: RawPlan = serde_json::from_str(json).map_err(invalid_plan_json)?;
        ensure_supported_version(raw.v)?;
        run_decode_defenses::<Self>(json).map_err(invalid_plan_json_context)?;
        let exact_json = json_with_exact_numbers(json).map_err(invalid_plan_json_context)?;
        let exact_raw: RawPlan = serde_json::from_value(exact_json).map_err(invalid_plan_json)?;
        let plan = serde_json::from_value(exact_raw.plan).map_err(invalid_plan_json)?;
        Ok(Self { v: raw.v, plan })
    }

    /// Encodes this plan in canonical compact JSON with stable field order.
    pub fn to_json(&self) -> DevonResult<String> {
        serde_json::to_string(self).map_err(invalid_plan_json)
    }
}

pub(crate) fn ensure_supported_version(version: u32) -> DevonResult<()> {
    if version > PLAN_VERSION {
        return Err(DevonError::InvalidArgument {
            context: format!(
                "DevonPlan version {version} is newer than supported DevonPlan version {PLAN_VERSION}"
            ),
        });
    }
    Ok(())
}

fn invalid_plan_json(error: serde_json::Error) -> DevonError {
    invalid_plan_json_context(error)
}

fn invalid_plan_json_context(error: impl std::fmt::Display) -> DevonError {
    DevonError::InvalidArgument {
        context: format!("invalid DevonPlan JSON: {error}"),
    }
}

#[cfg(test)]
mod ops_tests {
    use super::{
        AggregateFunction, AggregateItem, Direction, Operator, PLAN_VERSION, Plan, ProjectionItem,
        SortKey, SortOrder,
    };
    use crate::expr::{BinaryOp, Expr, Metric};
    use devondb_types::{DevonError, value::Value};

    fn scan() -> Operator {
        Operator::ScanNodes {
            table: "Person".into(),
            binding: "p".into(),
        }
    }

    fn column(name: &str) -> Expr {
        Expr::Col(name.into())
    }

    fn round_trip(operator: Operator) {
        let plan = Plan {
            v: PLAN_VERSION,
            plan: operator,
        };
        let json = plan.to_json().unwrap();
        assert_eq!(Plan::from_json(&json).unwrap(), plan);
    }

    #[test]
    fn ops_plan_ir_full_example_round_trips_canonically() {
        let source = r#"
        {
          "v": 0,
          "plan": {
            "op": "Project",
            "exprs": [{"expr": {"col": "p.name"}, "as": "name"}],
            "input": {
              "op": "Filter",
              "predicate": {"gt": [{"col": "p.age"}, {"lit": 30}]},
              "input": {"op": "ScanNodes", "table": "Person", "binding": "p"}
            }
          }
        }"#;
        let canonical = r#"{"v":0,"plan":{"op":"Project","exprs":[{"expr":{"col":"p.name"},"as":"name"}],"input":{"op":"Filter","predicate":{"gt":[{"col":"p.age"},{"lit":30}]},"input":{"op":"ScanNodes","table":"Person","binding":"p"}}}}"#;

        let plan = Plan::from_json(source).unwrap();
        assert_eq!(plan.to_json().unwrap(), canonical);
        assert_eq!(Plan::from_json(canonical).unwrap(), plan);
    }

    #[test]
    fn ops_every_operator_variant_round_trips() {
        let predicate = Expr::Binary {
            op: BinaryOp::Gt,
            left: Box::new(column("p.age")),
            right: Box::new(Expr::Lit(Value::Int64(30))),
        };
        let operators = vec![
            scan(),
            Operator::Expand {
                rel: "Knows".into(),
                direction: Direction::Both,
                from_binding: "p".into(),
                binding: "friend".into(),
                input: Box::new(scan()),
            },
            Operator::Filter {
                predicate,
                input: Box::new(scan()),
            },
            Operator::Project {
                exprs: vec![ProjectionItem {
                    expr: column("p.name"),
                    alias: "name".into(),
                }],
                input: Box::new(scan()),
            },
            Operator::Sort {
                keys: vec![SortKey {
                    expr: column("p.age"),
                    order: SortOrder::Desc,
                }],
                input: Box::new(scan()),
            },
            Operator::Limit {
                count: 10,
                offset: Some(5),
                input: Box::new(scan()),
            },
            Operator::Aggregate {
                group_by: vec![column("p.city")],
                aggs: vec![AggregateItem {
                    function: AggregateFunction::Count,
                    expr: column("p.id"),
                    alias: "n".into(),
                }],
                input: Box::new(scan()),
            },
            Operator::KnnScan {
                table: "Document".into(),
                column: "embedding".into(),
                query: vec![0.25, 0.75].into(),
                k: 3,
                metric: Metric::Cosine,
                mode: super::KnnMode::Exact,
            },
            Operator::WithinScan {
                table: "Place".into(),
                column: "location".into(),
                center: devondb_types::GeoPoint::new(45.5152, -122.6784).unwrap(),
                meters: 3218.688,
            },
        ];

        for operator in operators {
            round_trip(operator);
        }
    }

    #[test]
    fn ops_direction_order_function_and_metric_use_spec_spellings() {
        assert_eq!(serde_json::to_string(&Direction::Out).unwrap(), r#""out""#);
        assert_eq!(serde_json::to_string(&Direction::In).unwrap(), r#""in""#);
        assert_eq!(
            serde_json::to_string(&Direction::Both).unwrap(),
            r#""both""#
        );
        assert_eq!(serde_json::to_string(&SortOrder::Asc).unwrap(), r#""asc""#);
        assert_eq!(
            serde_json::to_string(&SortOrder::Desc).unwrap(),
            r#""desc""#
        );
        assert_eq!(
            serde_json::to_string(&AggregateFunction::Avg).unwrap(),
            r#""avg""#
        );
        assert_eq!(serde_json::to_string(&Metric::L2).unwrap(), r#""l2""#);
    }

    #[test]
    fn ops_limit_without_offset_omits_the_field_and_round_trips() {
        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::Limit {
                count: 10,
                offset: None,
                input: Box::new(scan()),
            },
        };

        let json = plan.to_json().unwrap();
        assert!(!json.contains("offset"));
        assert_eq!(Plan::from_json(&json).unwrap(), plan);
    }

    #[test]
    fn ops_exact_knn_omits_mode_and_absent_mode_is_exact() {
        let exact = Plan {
            v: PLAN_VERSION,
            plan: Operator::KnnScan {
                table: "Document".into(),
                column: "embedding".into(),
                query: vec![0.5].into(),
                k: 2,
                metric: Metric::L2,
                mode: super::KnnMode::Exact,
            },
        };
        let json = exact.to_json().unwrap();
        // The absent-field spelling is defined by docs/HNSW.md §11:
        // pre-existing and pinned plans keep exact semantics byte-for-byte.
        assert!(
            !json.contains("mode"),
            "exact plan leaked a mode field: {json}"
        );
        assert_eq!(Plan::from_json(&json).unwrap(), exact);

        let approximate = Plan {
            v: PLAN_VERSION,
            plan: Operator::KnnScan {
                table: "Document".into(),
                column: "embedding".into(),
                query: vec![0.5].into(),
                k: 2,
                metric: Metric::L2,
                mode: super::KnnMode::Approximate,
            },
        };
        let json = approximate.to_json().unwrap();
        assert!(json.contains(r#""mode":"approximate""#), "{json}");
        assert_eq!(Plan::from_json(&json).unwrap(), approximate);
    }

    #[test]
    fn ops_newer_plan_version_is_rejected_and_names_both_versions() {
        let error =
            Plan::from_json(r#"{"v":1,"plan":{"op":"ScanNodes","table":"Person","binding":"p"}}"#)
                .unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };

        assert!(context.contains("version 1"));
        assert!(context.contains("version 0"));
    }

    #[test]
    fn ops_unknown_operator_is_rejected_and_named() {
        let error = Plan::from_json(r#"{"v":0,"plan":{"op":"Teleport"}}"#).unwrap_err();
        assert!(error.to_string().contains("Teleport"));
    }

    #[test]
    fn knn_literal_vector_rejects_non_finite_elements_naming_the_index() {
        // `1e39` is a finite f64 but overflows f32 to infinity on decode.
        let operator_json = r#"{"op":"KnnScan","table":"Document","column":"embedding","query":[0.5,1e39],"k":2,"metric":"l2"}"#;
        let error = serde_json::from_str::<Operator>(operator_json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("vector element 1"), "{error}");
        assert!(error.contains("finite"), "{error}");

        let error = Plan::from_json(&format!(r#"{{"v":0,"plan":{operator_json}}}"#))
            .unwrap_err()
            .to_string();
        assert!(error.contains("finite"), "{error}");
    }
}
