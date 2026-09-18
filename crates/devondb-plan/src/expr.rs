//! DevonPlan expressions (`docs/PLAN_IR.md` § Expressions, binding).

use devondb_types::value::Value;
use serde::de::{self, MapAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value as JsonValue;
use std::fmt;

use crate::ops::Operator;

/// An expression in a DevonPlan operator.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A column reference written as `binding.column`.
    Col(String),
    /// The implementing node-class name for an interface-scan binding.
    ClassOf(String),
    /// The BM25 score carried by a TextScan binding.
    ScoreOf(String),
    /// A literal runtime value.
    Lit(Value),
    /// An operation with a left and right operand.
    Binary {
        /// The operation applied to the operands.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// Boolean negation.
    Not(Box<Expr>),
    /// The distance between two vector expressions.
    Distance {
        /// The left vector expression.
        left: Box<Expr>,
        /// The right vector expression.
        right: Box<Expr>,
        /// The distance metric.
        metric: Metric,
    },
    /// A conditional expression with an explicit branch for each outcome.
    If {
        /// The condition selecting the result branch.
        cond: Box<Expr>,
        /// The expression selected when `cond` is true.
        then_expr: Box<Expr>,
        /// The expression selected when `cond` is false or null.
        else_expr: Box<Expr>,
    },
    /// The first non-null expression in source order.
    Coalesce(Vec<Expr>),
    /// The least non-null expression under the scalar total order.
    Least(Vec<Expr>),
    /// The greatest non-null expression under the scalar total order.
    Greatest(Vec<Expr>),
    /// Truncates a timestamp to a calendar boundary in UTC.
    DateTrunc {
        /// The calendar unit to which the timestamp is truncated.
        unit: DateTruncUnit,
        /// The timestamp expression to truncate.
        value: Box<Expr>,
    },
    /// Adds a checked number of exact UTC days to a timestamp.
    DateAdd {
        /// The closed timestamp-offset unit.
        unit: DateTruncUnit,
        /// The timestamp expression to offset.
        value: Box<Expr>,
        /// The signed number of UTC days to add.
        amount: Box<Expr>,
    },
    /// Rounds a Decimal value while retaining its declared type.
    Round {
        /// The Decimal expression to round.
        value: Box<Expr>,
        /// The number of fractional places retained before padding to scale.
        places: u8,
    },
    /// Divides two Decimals exactly and rounds once at the requested place.
    RoundDiv {
        /// The Decimal numerator, whose type normally carries the result.
        numerator: Box<Expr>,
        /// The Decimal denominator.
        denominator: Box<Expr>,
        /// The number of fractional places retained before padding to scale.
        places: u8,
    },
    /// A query whose zero-or-one-row, one-column result is used as a value.
    Scalar {
        /// The embedded operator tree, sharing the containing plan envelope.
        plan: Box<Operator>,
    },
}

/// A binary DevonPlan expression operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// Equality.
    Eq,
    /// Inequality.
    Ne,
    /// Less than.
    Lt,
    /// Less than or equal to.
    Le,
    /// Greater than.
    Gt,
    /// Greater than or equal to.
    Ge,
    /// Boolean conjunction.
    And,
    /// Boolean disjunction.
    Or,
    /// Addition.
    Add,
    /// Subtraction.
    Sub,
    /// Multiplication.
    Mul,
    /// Division.
    Div,
}

/// A vector-distance metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    /// Cosine distance.
    Cosine,
    /// Euclidean (L2) distance.
    L2,
}

/// A closed DevonPlan timestamp-truncation unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DateTruncUnit {
    /// A UTC day beginning at midnight.
    Day,
}

impl BinaryOp {
    fn key(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Gt => "gt",
            Self::Ge => "ge",
            Self::And => "and",
            Self::Or => "or",
            Self::Add => "add",
            Self::Sub => "sub",
            Self::Mul => "mul",
            Self::Div => "div",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        match key {
            "eq" => Some(Self::Eq),
            "ne" => Some(Self::Ne),
            "lt" => Some(Self::Lt),
            "le" => Some(Self::Le),
            "gt" => Some(Self::Gt),
            "ge" => Some(Self::Ge),
            "and" => Some(Self::And),
            "or" => Some(Self::Or),
            "add" => Some(Self::Add),
            "sub" => Some(Self::Sub),
            "mul" => Some(Self::Mul),
            "div" => Some(Self::Div),
            _ => None,
        }
    }
}

#[derive(Serialize)]
struct DistanceRef<'a> {
    left: &'a Expr,
    right: &'a Expr,
    metric: Metric,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DistanceOwned {
    left: Expr,
    right: Expr,
    metric: Metric,
}

#[derive(Serialize)]
struct IfRef<'a> {
    cond: &'a Expr,
    #[serde(rename = "then")]
    then_expr: &'a Expr,
    #[serde(rename = "else")]
    else_expr: &'a Expr,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IfOwned {
    cond: Expr,
    #[serde(rename = "then")]
    then_expr: Expr,
    #[serde(rename = "else")]
    else_expr: Expr,
}

#[derive(Serialize)]
struct DateTruncRef<'a> {
    unit: DateTruncUnit,
    value: &'a Expr,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DateTruncOwned {
    unit: DateTruncUnit,
    value: Expr,
}

#[derive(Serialize)]
struct DateAddRef<'a> {
    unit: DateTruncUnit,
    value: &'a Expr,
    amount: &'a Expr,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DateAddOwned {
    unit: DateTruncUnit,
    value: Expr,
    amount: Expr,
}

#[derive(Serialize)]
struct RoundRef<'a> {
    value: &'a Expr,
    places: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoundOwned {
    value: Expr,
    places: u8,
}

#[derive(Serialize)]
struct RoundDivRef<'a> {
    numerator: &'a Expr,
    denominator: &'a Expr,
    places: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoundDivOwned {
    numerator: Expr,
    denominator: Expr,
    places: u8,
}

#[derive(Serialize)]
struct ScalarRef<'a> {
    plan: &'a Operator,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScalarOwned {
    plan: Operator,
}

struct NaturalValue<'a>(&'a Value);

impl Serialize for NaturalValue<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Int64(value) => serializer.serialize_i64(*value),
            Value::Float64(value) if value.is_finite() => serializer.serialize_f64(*value),
            Value::Float64(_) => Err(serde::ser::Error::custom(
                "non-finite Float64 is not a JSON number",
            )),
            Value::String(value) => serializer.serialize_str(value),
            Value::Vector(values) => serialize_vector(values, serializer),
            // The tagged natural spelling (`docs/PLAN_IR.md` § Literals):
            // {"geo":{"lat_deg":…,"lng_deg":…}}. Canonical form guarantees
            // finite components, so this always serializes.
            Value::GeoPoint(point) => {
                let mut object = serializer.serialize_map(Some(1))?;
                object.serialize_entry("geo", point)?;
                object.end()
            }
            // Tagged natural spellings from `docs/PLAN_IR.md` § Literals.
            // Emission only for now; decoding remains staged so both
            // directions become available together.
            Value::Timestamp(micros) => {
                let mut object = serializer.serialize_map(Some(1))?;
                object.serialize_entry("ts", micros)?;
                object.end()
            }
            Value::Bytes(bytes) => {
                let mut hex = String::with_capacity(bytes.len() * 2);
                for byte in bytes {
                    hex.push_str(&format!("{byte:02x}"));
                }
                let mut object = serializer.serialize_map(Some(1))?;
                object.serialize_entry("bytes", &hex)?;
                object.end()
            }
            Value::Decimal(value) => {
                let mut object = serializer.serialize_map(Some(1))?;
                object.serialize_entry("decimal", &value.to_string())?;
                object.end()
            }
            Value::Json(text) => {
                let mut object = serializer.serialize_map(Some(1))?;
                object.serialize_entry("json", text)?;
                object.end()
            }
        }
    }
}

fn serialize_vector<S>(values: &[f32], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if values.iter().any(|value| !value.is_finite()) {
        return Err(serde::ser::Error::custom(
            "non-finite vector element is not a JSON number",
        ));
    }

    let mut sequence = serializer.serialize_seq(Some(values.len()))?;
    for value in values {
        sequence.serialize_element(value)?;
    }
    sequence.end()
}

impl Serialize for Expr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        validate_variadic_arity(self).map_err(serde::ser::Error::custom)?;
        let mut object = serializer.serialize_map(Some(1))?;
        serialize_expression_entry(&mut object, self)?;
        object.end()
    }
}

fn serialize_expression_entry<M>(object: &mut M, expression: &Expr) -> Result<(), M::Error>
where
    M: SerializeMap,
{
    match expression {
        Expr::Col(column) => object.serialize_entry("col", column),
        Expr::ClassOf(binding) => object.serialize_entry("classof", binding),
        Expr::ScoreOf(binding) => object.serialize_entry("scoreof", binding),
        Expr::Lit(value) => object.serialize_entry("lit", &NaturalValue(value)),
        Expr::Binary { op, left, right } => {
            let operands = [left.as_ref(), right.as_ref()];
            object.serialize_entry(op.key(), &operands)
        }
        Expr::Not(expression) => object.serialize_entry("not", expression),
        Expr::Distance {
            left,
            right,
            metric,
        } => object.serialize_entry(
            "distance",
            &DistanceRef {
                left,
                right,
                metric: *metric,
            },
        ),
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => object.serialize_entry(
            "if",
            &IfRef {
                cond,
                then_expr,
                else_expr,
            },
        ),
        Expr::Coalesce(expressions) => object.serialize_entry("coalesce", expressions),
        Expr::Least(expressions) => object.serialize_entry("least", expressions),
        Expr::Greatest(expressions) => object.serialize_entry("greatest", expressions),
        Expr::DateTrunc { unit, value } => {
            object.serialize_entry("date_trunc", &DateTruncRef { unit: *unit, value })
        }
        Expr::DateAdd {
            unit,
            value,
            amount,
        } => object.serialize_entry(
            "date_add",
            &DateAddRef {
                unit: *unit,
                value,
                amount,
            },
        ),
        Expr::Round { value, places } => object.serialize_entry(
            "round",
            &RoundRef {
                value,
                places: *places,
            },
        ),
        Expr::RoundDiv {
            numerator,
            denominator,
            places,
        } => object.serialize_entry(
            "round_div",
            &RoundDivRef {
                numerator,
                denominator,
                places: *places,
            },
        ),
        Expr::Scalar { plan } => object.serialize_entry("scalar", &ScalarRef { plan }),
    }
}

fn validate_variadic_arity(expression: &Expr) -> Result<(), String> {
    let invalid = match expression {
        Expr::Coalesce(expressions) if expressions.len() < 2 => {
            Some(("coalesce", expressions.len()))
        }
        Expr::Least(expressions) if expressions.len() < 2 => Some(("least", expressions.len())),
        Expr::Greatest(expressions) if expressions.len() < 2 => {
            Some(("greatest", expressions.len()))
        }
        Expr::Round { places, .. } | Expr::RoundDiv { places, .. } if *places > 38 => {
            return Err(format!("expression places must be in 0..=38, got {places}"));
        }
        _ => None,
    };
    let Some((key, operand_count)) = invalid else {
        return Ok(());
    };
    Err(format!(
        "expression key `{key}` has wrong arity: expected at least 2 operands, got {operand_count}"
    ))
}

struct ExprVisitor;

impl<'de> Visitor<'de> for ExprVisitor {
    type Value = Expr;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a single-key DevonPlan expression object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let key = map
            .next_key::<String>()?
            .ok_or_else(|| de::Error::custom("expression object has no key"))?;
        let expression = match key.as_str() {
            "date_add" => map
                .next_value::<DateAddOwned>()
                .map(|payload| Expr::DateAdd {
                    unit: payload.unit,
                    value: Box::new(payload.value),
                    amount: Box::new(payload.amount),
                })?,
            "round" => map.next_value::<RoundOwned>().and_then(|payload| {
                validate_places::<A::Error>("round", payload.places)?;
                Ok(Expr::Round {
                    value: Box::new(payload.value),
                    places: payload.places,
                })
            })?,
            "round_div" => map.next_value::<RoundDivOwned>().and_then(|payload| {
                validate_places::<A::Error>("round_div", payload.places)?;
                Ok(Expr::RoundDiv {
                    numerator: Box::new(payload.numerator),
                    denominator: Box::new(payload.denominator),
                    places: payload.places,
                })
            })?,
            "scalar" => map
                .next_value::<ScalarOwned>()
                .map(|payload| Expr::Scalar {
                    plan: Box::new(payload.plan),
                })?,
            _ => {
                let value = map.next_value::<JsonValue>()?;
                expression_from_entry(&key, value).map_err(de::Error::custom)?
            }
        };

        if let Some(offending_key) = map.next_key::<String>()? {
            return Err(de::Error::custom(format!(
                "expression object has multiple keys; offending key `{offending_key}`"
            )));
        }

        Ok(expression)
    }
}

fn validate_places<E: de::Error>(key: &str, places: u8) -> Result<(), E> {
    if places <= 38 {
        return Ok(());
    }
    Err(E::custom(format!(
        "invalid expression key `{key}`: places must be in 0..=38, got {places}"
    )))
}

impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ExprVisitor)
    }
}

fn expression_from_entry(key: &str, value: JsonValue) -> Result<Expr, String> {
    match key {
        "col" => column_from_json(value),
        "classof" => classof_from_json(value),
        "scoreof" => match value {
            JsonValue::String(binding) if !binding.is_empty() => Ok(Expr::ScoreOf(binding)),
            _ => Err("scoreof expects a nonempty binding name".into()),
        },
        "lit" => literal_from_json(value).map(Expr::Lit),
        "not" => {
            expression_from_json("not", value).map(|expression| Expr::Not(Box::new(expression)))
        }
        "distance" => distance_from_json(value),
        "if" => if_from_json(value),
        "coalesce" => variadic_from_json("coalesce", value).map(Expr::Coalesce),
        "least" => variadic_from_json("least", value).map(Expr::Least),
        "greatest" => variadic_from_json("greatest", value).map(Expr::Greatest),
        "date_trunc" => date_trunc_from_json(value),
        _ => match BinaryOp::from_key(key) {
            Some(op) => binary_from_json(op, value),
            None => Err(format!("unknown expression key `{key}`")),
        },
    }
}

fn classof_from_json(value: JsonValue) -> Result<Expr, String> {
    match value {
        JsonValue::String(binding) if !binding.is_empty() => Ok(Expr::ClassOf(binding)),
        JsonValue::String(_) => {
            Err("expression key `classof` requires a non-empty binding name".to_owned())
        }
        _ => Err("expression key `classof` requires a string".to_owned()),
    }
}

fn column_from_json(value: JsonValue) -> Result<Expr, String> {
    match value {
        JsonValue::String(column) => Ok(Expr::Col(column)),
        _ => Err("expression key `col` requires a string".to_owned()),
    }
}

fn binary_from_json(op: BinaryOp, value: JsonValue) -> Result<Expr, String> {
    let JsonValue::Array(operands) = value else {
        return Err(format!(
            "expression key `{}` has wrong arity: expected 2 operands",
            op.key()
        ));
    };
    let [left, right]: [JsonValue; 2] = operands.try_into().map_err(|operands: Vec<_>| {
        format!(
            "expression key `{}` has wrong arity: expected 2 operands, got {}",
            op.key(),
            operands.len()
        )
    })?;

    Ok(Expr::Binary {
        op,
        left: Box::new(expression_from_json(op.key(), left)?),
        right: Box::new(expression_from_json(op.key(), right)?),
    })
}

fn expression_from_json(key: &str, value: JsonValue) -> Result<Expr, String> {
    serde_json::from_value(value)
        .map_err(|error| format!("invalid expression key `{key}`: {error}"))
}

fn distance_from_json(value: JsonValue) -> Result<Expr, String> {
    let distance: DistanceOwned = serde_json::from_value(value)
        .map_err(|error| format!("invalid expression key `distance`: {error}"))?;
    Ok(Expr::Distance {
        left: Box::new(distance.left),
        right: Box::new(distance.right),
        metric: distance.metric,
    })
}

fn if_from_json(value: JsonValue) -> Result<Expr, String> {
    let conditional: IfOwned = serde_json::from_value(value)
        .map_err(|error| format!("invalid expression key `if`: {error}"))?;
    Ok(Expr::If {
        cond: Box::new(conditional.cond),
        then_expr: Box::new(conditional.then_expr),
        else_expr: Box::new(conditional.else_expr),
    })
}

fn variadic_from_json(key: &str, value: JsonValue) -> Result<Vec<Expr>, String> {
    let JsonValue::Array(operands) = value else {
        return Err(format!(
            "expression key `{key}` has wrong arity: expected at least 2 operands"
        ));
    };
    if operands.len() < 2 {
        return Err(format!(
            "expression key `{key}` has wrong arity: expected at least 2 operands, got {}",
            operands.len()
        ));
    }
    operands
        .into_iter()
        .map(|operand| expression_from_json(key, operand))
        .collect()
}

fn date_trunc_from_json(value: JsonValue) -> Result<Expr, String> {
    let truncation: DateTruncOwned = serde_json::from_value(value)
        .map_err(|error| format!("invalid expression key `date_trunc`: {error}"))?;
    Ok(Expr::DateTrunc {
        unit: truncation.unit,
        value: Box::new(truncation.value),
    })
}

fn literal_from_json(value: JsonValue) -> Result<Value, String> {
    match value {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(value) => Ok(Value::Bool(value)),
        JsonValue::Number(number) => number_from_json(&number),
        JsonValue::String(value) => Ok(Value::String(value)),
        JsonValue::Array(values) => vector_from_json(values).map(Value::Vector),
        JsonValue::Object(object) => tagged_literal_from_json(object),
    }
}

/// Decodes the exactly-one-key tagged object literal vocabulary.
fn tagged_literal_from_json(object: serde_json::Map<String, JsonValue>) -> Result<Value, String> {
    if object.len() != 1 {
        return Err("expression key `lit` object literal must have exactly one key".to_owned());
    }
    let Some((tag, payload)) = object.into_iter().next() else {
        return Err("expression key `lit` object literal must have exactly one key".to_owned());
    };
    match tag.as_str() {
        "geo" => serde_json::from_value::<devondb_types::GeoPoint>(payload)
            .map(Value::GeoPoint)
            .map_err(|error| format!("invalid `geo` literal: {error}")),
        "ts" => timestamp_from_json(payload),
        "bytes" => bytes_from_json(payload),
        "decimal" => decimal_from_json(payload),
        "json" => json_document_from_json(payload),
        _ => Err(format!(
            "expression key `lit` has unknown tagged object literal `{tag}`"
        )),
    }
}

fn timestamp_from_json(payload: JsonValue) -> Result<Value, String> {
    let JsonValue::Number(number) = payload else {
        return Err("invalid `ts` literal: expected an i64 epoch-microseconds integer".to_owned());
    };
    number.as_i64().map(Value::Timestamp).ok_or_else(|| {
        "invalid `ts` literal: expected an i64 epoch-microseconds integer".to_owned()
    })
}

fn bytes_from_json(payload: JsonValue) -> Result<Value, String> {
    let JsonValue::String(text) = payload else {
        return Err("invalid `bytes` literal: expected a lowercase hex string".to_owned());
    };
    decode_lowercase_hex(&text).map(Value::Bytes)
}

fn decode_lowercase_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("invalid `bytes` literal: hex string must have even length".to_owned());
    }
    if text.bytes().any(|byte| matches!(byte, b'A'..=b'F')) {
        return Err("invalid `bytes` literal: hex string must be lowercase".to_owned());
    }
    if !text
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(
            "invalid `bytes` literal: expected only lowercase hexadecimal digits".to_owned(),
        );
    }
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = hex_nibble(pair[0])
                .ok_or_else(|| "invalid `bytes` literal: invalid hex digit".to_owned())?;
            let low = hex_nibble(pair[1])
                .ok_or_else(|| "invalid `bytes` literal: invalid hex digit".to_owned())?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn decimal_from_json(payload: JsonValue) -> Result<Value, String> {
    let JsonValue::String(text) = payload else {
        return Err("invalid `decimal` literal: expected an exact decimal string".to_owned());
    };
    text.parse::<devondb_types::Decimal128>()
        .map(Value::Decimal)
        .map_err(|error| format!("invalid `decimal` literal: {error}"))
}

fn json_document_from_json(payload: JsonValue) -> Result<Value, String> {
    let JsonValue::String(text) = payload else {
        return Err("invalid `json` literal: expected a JSON document string".to_owned());
    };
    canonical_json_preserving_key_order(&text)
        .map(Value::Json)
        .map_err(|error| format!("invalid `json` literal: document is not valid JSON: {error}"))
}

fn number_from_json(number: &serde_json::Number) -> Result<Value, String> {
    if let Some(value) = number.as_i64() {
        return Ok(Value::Int64(value));
    }
    if let Some(value) = number.as_u64() {
        return i64::try_from(value).map(Value::Int64).map_err(|_| {
            format!("expression key `lit` integer `{number}` is outside the Int64 range")
        });
    }
    number
        .as_f64()
        .map(Value::Float64)
        .ok_or_else(|| format!("expression key `lit` number `{number}` is not a finite Float64"))
}

fn vector_from_json(values: Vec<JsonValue>) -> Result<Vec<f32>, String> {
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let element = serde_json::from_value::<f32>(value).map_err(|error| {
                format!("expression key `lit` vector element {index} is not an f32 number: {error}")
            })?;
            // serde's f32 path maps an overflowing literal (`1e999`) to
            // infinity without error; non-finite elements have no canonical
            // spelling and break the round-trip law.
            if !element.is_finite() {
                return Err(format!(
                    "expression key `lit` vector element {index} is not a finite f32 number"
                ));
            }
            Ok(element)
        })
        .collect()
}

/// Rejects raw integer-syntax `lit` values that serde_json would coerce to f64.
pub(crate) fn ensure_integer_literals_in_range(json: &str) -> Result<(), String> {
    let bytes = json.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let Some(end) = string_token_end(bytes, cursor) else {
            return Ok(());
        };
        check_key_value(json, cursor, end)?;
        cursor = end;
    }
    Ok(())
}

/// Runs the two decode defenses shared by the plan and statement envelopes:
/// the integer-literal range scan, then a typed pre-pass whose derived
/// decoders reject duplicate fields that the exact-number rebuild's object
/// map would otherwise hide by last-wins insertion.
pub(crate) fn run_decode_defenses<T: serde::de::DeserializeOwned>(
    json: &str,
) -> Result<(), String> {
    ensure_integer_literals_in_range(json)?;
    serde_json::from_str::<T>(json)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn check_key_value(json: &str, start: usize, end: usize) -> Result<(), String> {
    let bytes = json.as_bytes();
    let mut cursor = skip_json_whitespace(bytes, end);
    if bytes.get(cursor) != Some(&b':') {
        return Ok(());
    }
    let key: String = serde_json::from_str(&json[start..end]).map_err(|error| error.to_string())?;
    cursor = skip_json_whitespace(bytes, cursor + 1);
    if key == "lit" {
        check_integer_literal(json, cursor)?;
    }
    Ok(())
}

fn check_integer_literal(json: &str, start: usize) -> Result<(), String> {
    let bytes = json.as_bytes();
    if !matches!(bytes.get(start), Some(b'-' | b'0'..=b'9')) {
        return Ok(());
    }
    let end = json_number_end(bytes, start);
    let literal = &json[start..end];
    if literal.contains(['.', 'e', 'E'])
        || literal.parse::<i64>().is_ok()
        || literal.parse::<u64>().is_ok()
    {
        return Ok(());
    }
    Err(format!(
        "expression key `lit` integer `{literal}` is outside the Int64 range {}..={}",
        i64::MIN,
        i64::MAX
    ))
}

fn string_token_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start + 1;
    let mut escaped = false;
    while let Some(byte) = bytes.get(cursor) {
        if !escaped && *byte == b'"' {
            return Some(cursor + 1);
        }
        escaped = !escaped && *byte == b'\\';
        cursor += 1;
    }
    None
}

fn skip_json_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while matches!(bytes.get(cursor), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        cursor += 1;
    }
    cursor
}

fn json_number_end(bytes: &[u8], mut cursor: usize) -> usize {
    while matches!(
        bytes.get(cursor),
        Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
    ) {
        cursor += 1;
    }
    cursor
}

/// Rebuilds validated JSON with number nodes parsed from their original text.
///
/// The ordinary serde_json pass remains the source of syntax and envelope errors.
/// This pass exists only because the default number parser does not promise an
/// exact f64 round-trip, and enabling workspace-wide number features would
/// change other crates' decoding behavior.
pub(crate) fn json_with_exact_numbers(json: &str) -> Result<JsonValue, String> {
    let mut parser = ExactJsonParser {
        source: json,
        cursor: 0,
    };
    let value = parser.parse_value()?;
    parser.skip_whitespace();
    if parser.cursor != json.len() {
        return Err("unexpected trailing characters in validated JSON".to_owned());
    }
    Ok(value)
}

/// Canonicalizes a JSON document to minimal spacing while preserving
/// document key order — the canonical-form law for `Json` values
/// (`docs/PLAN_IR.md` § Type system). The workspace pins serde_json without
/// `preserve_order` (root `Cargo.toml`), so rebuilding through
/// `serde_json::Value` would alphabetize keys; this emitter walks the
/// document in order instead. Number tokens follow the same fidelity rule
/// on every parse boundary: integers must fit `i64`/`u64`, floats must
/// stay finite and must not underflow a nonzero mantissa to zero.
pub(crate) fn canonical_json_preserving_key_order(text: &str) -> Result<String, String> {
    let mut parser = ExactJsonParser {
        source: text,
        cursor: 0,
    };
    let mut output = String::new();
    parser.emit_canonical_value(&mut output)?;
    parser.skip_whitespace();
    if parser.cursor != text.len() {
        return Err("unexpected trailing characters in JSON document".to_owned());
    }
    Ok(output)
}

/// The canonical serde_json spelling of one JSON number token.
fn canonical_json_number(literal: &str) -> Result<String, String> {
    let invalid =
        || format!("json literal number `{literal}` is outside the finite JSON number range");
    if !literal.contains(['.', 'e', 'E']) {
        if let Ok(value) = literal.parse::<i64>() {
            return Ok(value.to_string());
        }
        if let Ok(value) = literal.parse::<u64>() {
            return Ok(value.to_string());
        }
        return Err(invalid());
    }
    let value = literal.parse::<f64>().map_err(|_| invalid())?;
    let mantissa_has_nonzero_digit = literal
        .split(['e', 'E'])
        .next()
        .is_some_and(|mantissa| mantissa.chars().any(|digit| ('1'..='9').contains(&digit)));
    if !value.is_finite() || (value == 0.0 && mantissa_has_nonzero_digit) {
        return Err(invalid());
    }
    let number = serde_json::Number::from_f64(value).ok_or_else(invalid)?;
    Ok(number.to_string())
}

struct ExactJsonParser<'a> {
    source: &'a str,
    cursor: usize,
}

impl ExactJsonParser<'_> {
    fn parse_value(&mut self) -> Result<JsonValue, String> {
        self.skip_whitespace();
        match self.source.as_bytes().get(self.cursor) {
            Some(b'n') => self.parse_constant(4, JsonValue::Null),
            Some(b't') => self.parse_constant(4, JsonValue::Bool(true)),
            Some(b'f') => self.parse_constant(5, JsonValue::Bool(false)),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'-' | b'0'..=b'9') => self.parse_number().map(JsonValue::Number),
            _ => Err("unexpected token in validated JSON".to_owned()),
        }
    }

    fn parse_constant(&mut self, byte_len: usize, value: JsonValue) -> Result<JsonValue, String> {
        self.cursor += byte_len;
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let start = self.cursor;
        let end = string_token_end(self.source.as_bytes(), start)
            .ok_or_else(|| "unterminated string in validated JSON".to_owned())?;
        self.cursor = end;
        serde_json::from_str(&self.source[start..end]).map_err(|error| error.to_string())
    }

    fn parse_array(&mut self) -> Result<JsonValue, String> {
        self.cursor += 1;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.take_byte(b']') {
            return Ok(JsonValue::Array(values));
        }
        loop {
            values.push(self.parse_value()?);
            self.skip_whitespace();
            if self.take_byte(b']') {
                return Ok(JsonValue::Array(values));
            }
            self.require_byte(b',')?;
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, String> {
        self.cursor += 1;
        self.skip_whitespace();
        let mut values = serde_json::Map::new();
        if self.take_byte(b'}') {
            return Ok(JsonValue::Object(values));
        }
        loop {
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.require_byte(b':')?;
            values.insert(key, self.parse_value()?);
            self.skip_whitespace();
            if self.take_byte(b'}') {
                return Ok(JsonValue::Object(values));
            }
            self.require_byte(b',')?;
            self.skip_whitespace();
        }
    }

    fn parse_number(&mut self) -> Result<serde_json::Number, String> {
        let start = self.cursor;
        self.cursor = json_number_end(self.source.as_bytes(), start);
        let literal = &self.source[start..self.cursor];
        if !literal.contains(['.', 'e', 'E']) {
            if let Ok(value) = literal.parse::<i64>() {
                return Ok(value.into());
            }
            if let Ok(value) = literal.parse::<u64>() {
                return Ok(value.into());
            }
        }
        let value = literal
            .parse::<f64>()
            .map_err(|error| format!("invalid number `{literal}`: {error}"))?;
        serde_json::Number::from_f64(value)
            .ok_or_else(|| format!("number `{literal}` is not a finite Float64"))
    }

    fn skip_whitespace(&mut self) {
        self.cursor = skip_json_whitespace(self.source.as_bytes(), self.cursor);
    }

    fn take_byte(&mut self, expected: u8) -> bool {
        if self.source.as_bytes().get(self.cursor) != Some(&expected) {
            return false;
        }
        self.cursor += 1;
        true
    }

    fn require_byte(&mut self, expected: u8) -> Result<(), String> {
        if self.take_byte(expected) {
            return Ok(());
        }
        Err("unexpected delimiter in validated JSON".to_owned())
    }

    fn emit_canonical_value(&mut self, output: &mut String) -> Result<(), String> {
        self.skip_whitespace();
        match self.source.as_bytes().get(self.cursor) {
            Some(b'n') => self.emit_constant("null", output),
            Some(b't') => self.emit_constant("true", output),
            Some(b'f') => self.emit_constant("false", output),
            Some(b'"') => self.emit_string(output),
            Some(b'[') => self.emit_array(output),
            Some(b'{') => self.emit_object(output),
            Some(b'-' | b'0'..=b'9') => self.emit_number(output),
            _ => Err("unexpected token in JSON document".to_owned()),
        }
    }

    fn emit_constant(&mut self, constant: &'static str, output: &mut String) -> Result<(), String> {
        if !self.source[self.cursor..].starts_with(constant) {
            return Err("unexpected token in JSON document".to_owned());
        }
        self.cursor += constant.len();
        output.push_str(constant);
        Ok(())
    }

    fn emit_string(&mut self, output: &mut String) -> Result<(), String> {
        let value = self.parse_string()?;
        let canonical = serde_json::to_string(&value).map_err(|error| error.to_string())?;
        output.push_str(&canonical);
        Ok(())
    }

    fn emit_array(&mut self, output: &mut String) -> Result<(), String> {
        self.cursor += 1;
        self.skip_whitespace();
        output.push('[');
        if self.take_byte(b']') {
            output.push(']');
            return Ok(());
        }
        let mut index = 0;
        loop {
            if index > 0 {
                output.push(',');
            }
            self.emit_canonical_value(output)?;
            self.skip_whitespace();
            if self.take_byte(b']') {
                output.push(']');
                return Ok(());
            }
            self.require_byte(b',')?;
            index += 1;
        }
    }

    fn emit_object(&mut self, output: &mut String) -> Result<(), String> {
        self.cursor += 1;
        self.skip_whitespace();
        if self.take_byte(b'}') {
            output.push_str("{}");
            return Ok(());
        }
        let mut entries: Vec<(String, String)> = Vec::new();
        loop {
            self.skip_whitespace();
            let key = self.parse_string()?;
            self.skip_whitespace();
            self.require_byte(b':')?;
            let mut value = String::new();
            self.emit_canonical_value(&mut value)?;
            // serde_json `preserve_order` semantics, emulated: a repeated
            // key keeps its first position and takes the last value.
            match entries.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, slot)) => *slot = value,
                None => entries.push((key, value)),
            }
            self.skip_whitespace();
            if self.take_byte(b'}') {
                break;
            }
            self.require_byte(b',')?;
        }
        output.push('{');
        for (index, (key, value)) in entries.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            let key_json = serde_json::to_string(key).map_err(|error| error.to_string())?;
            output.push_str(&key_json);
            output.push(':');
            output.push_str(value);
        }
        output.push('}');
        Ok(())
    }

    fn emit_number(&mut self, output: &mut String) -> Result<(), String> {
        let start = self.cursor;
        self.cursor = json_number_end(self.source.as_bytes(), start);
        let literal = &self.source[start..self.cursor];
        output.push_str(&canonical_json_number(literal)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{BinaryOp, DateTruncUnit, Expr, Metric};
    use devondb_types::value::Value;

    /// The tagged natural spelling (PLAN_IR § Literals): the one admitted
    /// object literal, canonical-form-enforced on decode.
    #[test]
    fn geo_literal_natural_json_round_trips() {
        let expr = Expr::Lit(Value::GeoPoint(
            devondb_types::GeoPoint::from_canonical(45.5, -122.625).unwrap(),
        ));
        let json = serde_json::to_string(&expr).unwrap();
        assert_eq!(
            json,
            r#"{"lit":{"geo":{"lat_deg":45.5,"lng_deg":-122.625}}}"#
        );
        let back: Expr = serde_json::from_str(&json).unwrap();
        assert_eq!(back, expr);

        for rejected in [
            // Non-canonical payload.
            r#"{"lit":{"geo":{"lat_deg":10.0,"lng_deg":180.0}}}"#,
            // Unknown payload field.
            r#"{"lit":{"geo":{"lat_deg":1.0,"lng_deg":2.0,"alt":3.0}}}"#,
            // Any other object literal stays invalid.
            r#"{"lit":{"point":{"lat_deg":1.0,"lng_deg":2.0}}}"#,
            r#"{"lit":{"geo":{"lat_deg":1.0,"lng_deg":2.0},"extra":1}}"#,
        ] {
            assert!(
                serde_json::from_str::<Expr>(rejected).is_err(),
                "accepted invalid geo literal: {rejected}"
            );
        }
    }

    #[test]
    fn scalar_v2_tagged_literals_round_trip_with_pinned_spellings() {
        let decimal: devondb_types::Decimal128 = "19.99".parse().unwrap();
        let cases = [
            (Expr::Lit(Value::Timestamp(0)), r#"{"lit":{"ts":0}}"#),
            (
                Expr::Lit(Value::Bytes(vec![0x00, 0xff, 0x1a])),
                r#"{"lit":{"bytes":"00ff1a"}}"#,
            ),
            (
                Expr::Lit(Value::Decimal(decimal)),
                r#"{"lit":{"decimal":"19.99"}}"#,
            ),
            (
                Expr::Lit(Value::Json(r#"{"k":1}"#.to_owned())),
                r#"{"lit":{"json":"{\"k\":1}"}}"#,
            ),
        ];

        for (expression, expected) in cases {
            let json = serde_json::to_string(&expression).unwrap();
            assert_eq!(json, expected);
            assert_eq!(serde_json::from_str::<Expr>(&json).unwrap(), expression);
        }

        let canonicalized: Expr =
            serde_json::from_str(r#"{"lit":{"json":"{ \"k\": 1 }"}}"#).unwrap();
        assert_eq!(
            canonicalized,
            Expr::Lit(Value::Json(r#"{"k":1}"#.to_owned()))
        );
        assert_eq!(
            serde_json::to_string(&canonicalized).unwrap(),
            r#"{"lit":{"json":"{\"k\":1}"}}"#
        );
    }

    #[test]
    fn json_literal_canonicalization_preserves_document_key_order() {
        // PLAN_IR's canonical-form law for Json requires preserved key
        // order; the workspace's serde_json build (no `preserve_order`)
        // would alphabetize keys, so decode must canonicalize in order.
        let decoded: Expr =
            serde_json::from_str(r#"{"lit":{"json":"{\"b\":1,\"a\":2}"}}"#).unwrap();
        assert_eq!(
            decoded,
            Expr::Lit(Value::Json(r#"{"b":1,"a":2}"#.to_owned()))
        );
        assert_eq!(
            serde_json::to_string(&decoded).unwrap(),
            r#"{"lit":{"json":"{\"b\":1,\"a\":2}"}}"#
        );

        let nested: Expr = serde_json::from_str(
            r#"{"lit":{"json":"{ \"z\" : { \"y\": 1, \"x\": 2 }, \"a\": [3, true, null] }"}}"#,
        )
        .unwrap();
        assert_eq!(
            nested,
            Expr::Lit(Value::Json(
                r#"{"z":{"y":1,"x":2},"a":[3,true,null]}"#.to_owned()
            ))
        );
    }

    #[test]
    fn non_finite_vector_literal_elements_are_rejected_naming_the_index() {
        // `1e39` is a finite f64 but overflows f32 to infinity on decode.
        let error = serde_json::from_str::<Expr>(r#"{"lit":[1.0,1e39]}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("vector element 1"), "{error}");
        assert!(error.contains("finite"), "{error}");
    }

    #[test]
    fn scalar_v2_tagged_literal_payload_errors_name_the_tag_and_rule() {
        let rejected = [
            (r#"{"lit":{"ts":"0"}}"#, "ts"),
            (r#"{"lit":{"ts":1.0}}"#, "ts"),
            (r#"{"lit":{"bytes":"abc"}}"#, "even"),
            (r#"{"lit":{"bytes":"00FF"}}"#, "lowercase"),
            (r#"{"lit":{"decimal":"1e5"}}"#, "decimal"),
            (r#"{"lit":{"json":"{"}}"#, "valid JSON"),
        ];
        for (json, required) in rejected {
            let error = serde_json::from_str::<Expr>(json).unwrap_err().to_string();
            assert!(error.contains(required), "{json}: {error}");
        }

        for json in [r#"{"lit":{}}"#, r#"{"lit":{"ts":0,"bytes":""}}"#] {
            let error = serde_json::from_str::<Expr>(json).unwrap_err().to_string();
            assert!(error.contains("exactly one key"), "{json}: {error}");
        }
    }

    fn operands(op: BinaryOp) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(Expr::Col("p.age".into())),
            right: Box::new(Expr::Lit(Value::Int64(30))),
        }
    }

    #[test]
    fn round_trips_every_binary_operator_key() {
        let operators = [
            (BinaryOp::Eq, "eq"),
            (BinaryOp::Ne, "ne"),
            (BinaryOp::Lt, "lt"),
            (BinaryOp::Le, "le"),
            (BinaryOp::Gt, "gt"),
            (BinaryOp::Ge, "ge"),
            (BinaryOp::And, "and"),
            (BinaryOp::Or, "or"),
            (BinaryOp::Add, "add"),
            (BinaryOp::Sub, "sub"),
            (BinaryOp::Mul, "mul"),
            (BinaryOp::Div, "div"),
        ];

        for (op, key) in operators {
            let expression = operands(op);
            let json = serde_json::to_string(&expression).unwrap();
            assert_eq!(
                json,
                format!(r#"{{"{key}":[{{"col":"p.age"}},{{"lit":30}}]}}"#)
            );
            assert_eq!(serde_json::from_str::<Expr>(&json).unwrap(), expression);
        }
    }

    #[test]
    fn natural_literals_round_trip_every_value_variant() {
        let cases = [
            (r#"{"lit":null}"#, Value::Null),
            (r#"{"lit":true}"#, Value::Bool(true)),
            (r#"{"lit":-42}"#, Value::Int64(-42)),
            (r#"{"lit":30.5}"#, Value::Float64(30.5)),
            (r#"{"lit":"devon"}"#, Value::String("devon".into())),
            (
                r#"{"lit":[1.0,2.5,-3.0]}"#,
                Value::Vector(vec![1.0, 2.5, -3.0]),
            ),
        ];

        for (json, value) in cases {
            let expression = Expr::Lit(value);
            assert_eq!(serde_json::from_str::<Expr>(json).unwrap(), expression);
            assert_eq!(serde_json::to_string(&expression).unwrap(), json);
        }
    }

    #[test]
    fn integer_and_non_integer_numbers_select_distinct_literal_types() {
        assert_eq!(
            serde_json::from_str::<Expr>(r#"{"lit":30}"#).unwrap(),
            Expr::Lit(Value::Int64(30))
        );
        assert_eq!(
            serde_json::from_str::<Expr>(r#"{"lit":30.5}"#).unwrap(),
            Expr::Lit(Value::Float64(30.5))
        );
    }

    #[test]
    fn nested_expression_round_trips() {
        let expression = Expr::Not(Box::new(Expr::Binary {
            op: BinaryOp::Or,
            left: Box::new(Expr::Distance {
                left: Box::new(Expr::Col("p.embedding".into())),
                right: Box::new(Expr::Lit(Value::Vector(vec![0.25, 0.75]))),
                metric: Metric::Cosine,
            }),
            right: Box::new(Expr::Distance {
                left: Box::new(Expr::Col("p.embedding".into())),
                right: Box::new(Expr::Lit(Value::Vector(vec![1.0, 0.0]))),
                metric: Metric::L2,
            }),
        }));

        let json = serde_json::to_string(&expression).unwrap();
        assert_eq!(serde_json::from_str::<Expr>(&json).unwrap(), expression);
    }

    #[test]
    fn a6_expression_families_round_trip_with_canonical_keys_and_field_order() {
        let cases = [
            (
                Expr::If {
                    cond: Box::new(Expr::Lit(Value::Bool(true))),
                    then_expr: Box::new(Expr::Lit(Value::Int64(1))),
                    else_expr: Box::new(Expr::Lit(Value::Null)),
                },
                r#"{"if":{"cond":{"lit":true},"then":{"lit":1},"else":{"lit":null}}}"#,
            ),
            (
                Expr::Coalesce(vec![
                    Expr::Lit(Value::Null),
                    Expr::Col("p.name".into()),
                    Expr::Lit(Value::String("unknown".into())),
                ]),
                r#"{"coalesce":[{"lit":null},{"col":"p.name"},{"lit":"unknown"}]}"#,
            ),
            (
                Expr::Least(vec![Expr::Col("p.age".into()), Expr::Lit(Value::Int64(30))]),
                r#"{"least":[{"col":"p.age"},{"lit":30}]}"#,
            ),
            (
                Expr::Greatest(vec![
                    Expr::Lit(Value::String("a".into())),
                    Expr::Lit(Value::Null),
                    Expr::Lit(Value::String("z".into())),
                ]),
                r#"{"greatest":[{"lit":"a"},{"lit":null},{"lit":"z"}]}"#,
            ),
            (
                Expr::DateTrunc {
                    unit: DateTruncUnit::Day,
                    value: Box::new(Expr::Lit(Value::Timestamp(1))),
                },
                r#"{"date_trunc":{"unit":"day","value":{"lit":{"ts":1}}}}"#,
            ),
        ];

        for (expression, expected) in cases {
            let json = serde_json::to_string(&expression).unwrap();
            assert_eq!(json, expected);
            assert_eq!(serde_json::from_str::<Expr>(&json).unwrap(), expression);
        }
    }

    #[test]
    fn a6_expression_payloads_reject_unknown_fields_and_wrong_arities() {
        let invalid_if = [
            r#"{"if":[]}"#,
            r#"{"if":{"then":{"lit":1},"else":{"lit":2}}}"#,
            r#"{"if":{"cond":{"lit":true},"else":{"lit":2}}}"#,
            r#"{"if":{"cond":{"lit":true},"then":{"lit":1}}}"#,
            r#"{"if":{"cond":{"lit":true},"then":{"lit":1},"else":{"lit":2},"extra":0}}"#,
        ];
        for json in invalid_if {
            assert!(
                serde_json::from_str::<Expr>(json).is_err(),
                "accepted invalid if payload: {json}"
            );
        }

        for key in ["coalesce", "least", "greatest"] {
            for payload in ["[]", r#"[{"lit":1}]"#, r#"{"left":{"lit":1}}"#, "1"] {
                let json = format!(r#"{{"{key}":{payload}}}"#);
                let error = serde_json::from_str::<Expr>(&json).unwrap_err().to_string();
                assert!(error.contains(key), "{json}: {error}");
                assert!(error.contains("arity"), "{json}: {error}");
            }
        }
    }

    #[test]
    fn date_trunc_payload_accepts_only_the_exact_day_shape() {
        for json in [
            r#"{"date_trunc":[]}"#,
            r#"{"date_trunc":{}}"#,
            r#"{"date_trunc":{"value":{"lit":{"ts":0}}}}"#,
            r#"{"date_trunc":{"unit":"day"}}"#,
            r#"{"date_trunc":{"unit":"week","value":{"lit":{"ts":0}}}}"#,
            r#"{"date_trunc":{"unit":"Day","value":{"lit":{"ts":0}}}}"#,
            r#"{"date_trunc":{"unit":1,"value":{"lit":{"ts":0}}}}"#,
            r#"{"date_trunc":{"unit":"day","value":{"lit":{"ts":0}},"extra":0}}"#,
        ] {
            let error = serde_json::from_str::<Expr>(json).unwrap_err().to_string();
            assert!(error.contains("date_trunc"), "{json}: {error}");
        }
    }

    #[test]
    fn a6_expressions_nest_inside_existing_composites() {
        let expression = Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(Expr::DateTrunc {
                unit: DateTruncUnit::Day,
                value: Box::new(Expr::Col("p.created_at".into())),
            }),
            right: Box::new(Expr::If {
                cond: Box::new(Expr::Col("p.active".into())),
                then_expr: Box::new(Expr::Coalesce(vec![
                    Expr::Lit(Value::Null),
                    Expr::Lit(Value::Timestamp(0)),
                ])),
                else_expr: Box::new(Expr::Least(vec![
                    Expr::Lit(Value::Timestamp(0)),
                    Expr::Lit(Value::Timestamp(86_400_000_000)),
                ])),
            }),
        };

        let json = serde_json::to_string(&expression).unwrap();
        assert_eq!(serde_json::from_str::<Expr>(&json).unwrap(), expression);
    }

    #[test]
    fn plan_ir_example_predicate_parses_and_serializes_canonically() {
        let json = r#"{"gt":[{"col":"p.age"},{"lit":30}]}"#;
        let expected = Expr::Binary {
            op: BinaryOp::Gt,
            left: Box::new(Expr::Col("p.age".into())),
            right: Box::new(Expr::Lit(Value::Int64(30))),
        };

        let parsed = serde_json::from_str::<Expr>(json).unwrap();
        assert_eq!(parsed, expected);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn unknown_and_multiple_keys_are_rejected_with_the_offending_key() {
        let unknown = serde_json::from_str::<Expr>(r#"{"mystery":1}"#)
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("mystery"));

        let multiple = serde_json::from_str::<Expr>(r#"{"col":"p.age","lit":30}"#)
            .unwrap_err()
            .to_string();
        assert!(multiple.contains("lit"));
    }

    #[test]
    fn wrong_binary_arity_is_rejected_with_the_operator_key() {
        for json in [
            r#"{"add":[{"lit":1}]}"#,
            r#"{"add":[{"lit":1},{"lit":2},{"lit":3}]}"#,
            r#"{"add":{"lit":1}}"#,
        ] {
            let error = serde_json::from_str::<Expr>(json).unwrap_err().to_string();
            assert!(error.contains("add"));
            assert!(error.contains("arity"));
        }
    }
}
