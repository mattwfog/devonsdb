//! Canonical DevonPlan expression and aggregate typing rules.

use std::collections::HashMap;

use devondb_types::decimal::MAX_PRECISION;
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{NodeTableSchema, RelTableSchema, fold, suggestion_suffix};
use devondb_types::{DevonError, DevonResult};

use crate::expr::{BinaryOp, Expr};
use crate::ops::{AggregateFunction, KnnVectorSource, Operator};
use crate::statement::InterfaceColumn;

/// The name and declared columns of one executable ontology interface.
///
/// Implementing node tables are deliberately absent: interface-scan typing
/// depends only on the catalog declaration, and a zero-implementer interface
/// remains a legal, empty source.
#[derive(Debug, Clone, PartialEq)]
pub struct InterfaceSchema {
    name: String,
    columns: Vec<InterfaceColumn>,
}

impl InterfaceSchema {
    /// Creates an interface schema in catalog declaration order.
    #[must_use]
    pub fn new(name: String, columns: Vec<InterfaceColumn>) -> Self {
        Self { name, columns }
    }

    /// Returns the interface's catalog spelling.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the interface columns in declaration order.
    #[must_use]
    pub fn columns(&self) -> &[InterfaceColumn] {
        &self.columns
    }
}

/// Catalog schema input used to resolve and type DevonPlan operators.
///
/// The facade adapter from its persisted `SchemaSummary` is intentionally a
/// separate layer; the plan crate consumes only table schemas and the
/// interface name-to-column declarations it needs for static typing.
#[derive(Debug, Clone, Copy)]
pub struct SchemaInput<'schema> {
    node_tables: &'schema [NodeTableSchema],
    rel_tables: &'schema [RelTableSchema],
    interfaces: &'schema [InterfaceSchema],
}

impl<'schema> SchemaInput<'schema> {
    /// Creates schema input containing node tables, relationship tables, and
    /// executable ontology interfaces.
    #[must_use]
    pub const fn new(
        node_tables: &'schema [NodeTableSchema],
        rel_tables: &'schema [RelTableSchema],
        interfaces: &'schema [InterfaceSchema],
    ) -> Self {
        Self {
            node_tables,
            rel_tables,
            interfaces,
        }
    }

    /// Creates backward-compatible schema input with no interfaces.
    #[must_use]
    pub const fn tables_only(
        node_tables: &'schema [NodeTableSchema],
        rel_tables: &'schema [RelTableSchema],
    ) -> Self {
        Self::new(node_tables, rel_tables, &[])
    }

    /// Returns node tables in catalog declaration order.
    #[must_use]
    pub const fn node_tables(&self) -> &'schema [NodeTableSchema] {
        self.node_tables
    }

    /// Returns relationship tables in catalog declaration order.
    #[must_use]
    pub const fn rel_tables(&self) -> &'schema [RelTableSchema] {
        self.rel_tables
    }

    /// Returns ontology interfaces in catalog declaration order.
    #[must_use]
    pub const fn interfaces(&self) -> &'schema [InterfaceSchema] {
        self.interfaces
    }
}

/// The inferred type of an expression.
///
/// `Null` represents an expression that has no non-null value from which to
/// infer a logical column type. Every logical column type admits null values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpressionType {
    /// An expression whose only possible value is `Null`.
    Null,
    /// An expression with a known non-null logical value type.
    Value(LogicalType),
}

impl ExpressionType {
    /// Returns the logical output-column type used to carry this expression.
    ///
    /// A wholly unconstrained null expression uses `Bool` as its deterministic
    /// carrier type; null is admitted by that type just as by every other v0
    /// logical column type.
    #[must_use]
    pub const fn output_type(self) -> LogicalType {
        match self {
            Self::Null => LogicalType::Bool,
            Self::Value(ty) => ty,
        }
    }
}

/// Infers and validates an expression against referenceable input columns.
pub fn expression_type(
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
) -> DevonResult<ExpressionType> {
    expression_type_with_subqueries(expression, columns, &mut |_| {
        Err(invalid_argument(
            "operator `scalar` requires plan-validation context",
        ))
    })
}

pub(crate) fn expression_type_with_subqueries(
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
    scalar_type: &mut impl FnMut(&Operator) -> DevonResult<ExpressionType>,
) -> DevonResult<ExpressionType> {
    match expression {
        Expr::Col(reference) => column_type(reference, columns),
        Expr::ClassOf(_) => Ok(ExpressionType::Value(LogicalType::String)),
        Expr::ScoreOf(_) => Ok(ExpressionType::Value(LogicalType::Float64)),
        Expr::Lit(value) => Ok(value
            .logical_type()
            .map_or(ExpressionType::Null, ExpressionType::Value)),
        Expr::Binary { op, left, right } => {
            let left = expression_type_with_subqueries(left, columns, scalar_type)?;
            let right = expression_type_with_subqueries(right, columns, scalar_type)?;
            binary_type(*op, left, right)
        }
        Expr::Not(operand) => {
            let operand = expression_type_with_subqueries(operand, columns, scalar_type)?;
            require_boolean("not", operand)?;
            Ok(ExpressionType::Value(LogicalType::Bool))
        }
        Expr::Distance { left, right, .. } => {
            let left = expression_type_with_subqueries(left, columns, scalar_type)?;
            let right = expression_type_with_subqueries(right, columns, scalar_type)?;
            require_matching_vectors(left, right)?;
            Ok(ExpressionType::Value(LogicalType::Float64))
        }
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            let condition = expression_type_with_subqueries(cond, columns, scalar_type)?;
            require_boolean("if condition", condition)?;
            selection_type(
                "if",
                [then_expr.as_ref(), else_expr.as_ref()],
                columns,
                scalar_type,
            )
        }
        Expr::Coalesce(expressions) => variadic_type("coalesce", expressions, columns, scalar_type),
        Expr::Least(expressions) => extrema_type("least", expressions, columns, scalar_type),
        Expr::Greatest(expressions) => extrema_type("greatest", expressions, columns, scalar_type),
        Expr::DateTrunc { value, .. } => {
            let operand = expression_type_with_subqueries(value, columns, scalar_type)?;
            require_timestamp("date_trunc", operand)?;
            Ok(ExpressionType::Value(LogicalType::Timestamp))
        }
        Expr::DateAdd { value, amount, .. } => {
            let value = expression_type_with_subqueries(value, columns, scalar_type)?;
            let amount = expression_type_with_subqueries(amount, columns, scalar_type)?;
            require_timestamp("date_add value", value)?;
            require_int64("date_add amount", amount)?;
            Ok(ExpressionType::Value(LogicalType::Timestamp))
        }
        Expr::Round { value, places } => {
            let value = expression_type_with_subqueries(value, columns, scalar_type)?;
            round_type(value, *places)
        }
        Expr::RoundDiv {
            numerator,
            denominator,
            places,
        } => {
            let numerator = expression_type_with_subqueries(numerator, columns, scalar_type)?;
            let denominator = expression_type_with_subqueries(denominator, columns, scalar_type)?;
            round_div_type(numerator, denominator, *places)
        }
        Expr::Scalar { plan } => scalar_type(plan),
    }
}

pub(crate) fn knn_vector_source_type(
    source: &KnnVectorSource,
    scalar_type: &mut impl FnMut(&Operator) -> DevonResult<ExpressionType>,
) -> DevonResult<LogicalType> {
    match source {
        KnnVectorSource::Literal(vector) => {
            let dim = u32::try_from(vector.len()).map_err(|_| {
                invalid_argument(format!(
                    "KNN literal vector dimension {} exceeds u32",
                    vector.len()
                ))
            })?;
            Ok(LogicalType::Vector { dim })
        }
        KnnVectorSource::Scalar { plan } => scalar_type(plan).map(ExpressionType::output_type),
    }
}

/// Infers and validates one aggregate result type.
pub fn aggregate_type(
    function: AggregateFunction,
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
) -> DevonResult<LogicalType> {
    aggregate_type_with_subqueries(function, expression, columns, &mut |_| {
        Err(invalid_argument(
            "operator `scalar` requires plan-validation context",
        ))
    })
}

pub(crate) fn aggregate_type_with_subqueries(
    function: AggregateFunction,
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
    scalar_type: &mut impl FnMut(&Operator) -> DevonResult<ExpressionType>,
) -> DevonResult<LogicalType> {
    let operand = expression_type_with_subqueries(expression, columns, scalar_type)?;
    match function {
        AggregateFunction::Count => Ok(LogicalType::Int64),
        AggregateFunction::Sum => {
            if !matches!(operand, ExpressionType::Value(LogicalType::Decimal { .. })) {
                require_numeric("sum", operand)?;
            }
            Ok(match operand {
                ExpressionType::Null => LogicalType::Int64,
                ExpressionType::Value(ty) => ty,
            })
        }
        AggregateFunction::Avg => {
            if matches!(operand, ExpressionType::Value(LogicalType::Decimal { .. })) {
                return Err(invalid_argument(
                    "avg over Decimal would round; compute sum and count and divide consumer-side",
                ));
            }
            require_numeric("avg", operand)?;
            Ok(LogicalType::Float64)
        }
        AggregateFunction::Min | AggregateFunction::Max => {
            reject_unordered_type(aggregate_name(function), operand)?;
            Ok(operand.output_type())
        }
        AggregateFunction::PercentileCont => {
            require_decimal("percentile_cont", operand)?;
            Ok(operand.output_type())
        }
    }
}

fn round_type(operand: ExpressionType, places: u8) -> DevonResult<ExpressionType> {
    require_decimal("round", operand)?;
    require_places_within_scale("round", operand, places)?;
    Ok(operand)
}

fn round_div_type(
    numerator: ExpressionType,
    denominator: ExpressionType,
    places: u8,
) -> DevonResult<ExpressionType> {
    require_decimal("round_div numerator", numerator)?;
    require_decimal("round_div denominator", denominator)?;
    let result = if !matches!(numerator, ExpressionType::Null) {
        numerator
    } else {
        denominator
    };
    require_places_within_scale("round_div", result, places)?;
    Ok(result)
}

fn require_places_within_scale(
    operator: &str,
    operand: ExpressionType,
    places: u8,
) -> DevonResult<()> {
    let ExpressionType::Value(LogicalType::Decimal { scale, .. }) = operand else {
        return Ok(());
    };
    if places <= scale {
        return Ok(());
    }
    Err(invalid_argument(format!(
        "operator `{operator}` requires places <= Decimal scale {scale}; got {places}"
    )))
}

fn column_type(
    reference: &str,
    columns: &HashMap<String, LogicalType>,
) -> DevonResult<ExpressionType> {
    let Some((binding, column)) = reference.split_once('.') else {
        return Err(invalid_argument(format!(
            "column reference `{reference}` must be written as `binding.column`"
        )));
    };
    if binding.is_empty() || column.is_empty() {
        return Err(invalid_argument(format!(
            "column reference `{reference}` must name a binding and column"
        )));
    }
    columns
        .get(reference)
        .copied()
        .map(LogicalType::value_type)
        .map(ExpressionType::Value)
        .ok_or_else(|| {
            let mut candidates = columns.keys().map(String::as_str).collect::<Vec<_>>();
            candidates.sort_unstable_by(|left, right| {
                fold(left).cmp(&fold(right)).then_with(|| left.cmp(right))
            });
            invalid_argument(format!(
                "column reference `{reference}` is not in the input schema{}",
                suggestion_suffix(reference, candidates)
            ))
        })
}

fn binary_type(
    operator: BinaryOp,
    left: ExpressionType,
    right: ExpressionType,
) -> DevonResult<ExpressionType> {
    match operator {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
            arithmetic_type(operator, left, right)
        }
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            comparison_type(operator, left, right)
        }
        BinaryOp::And | BinaryOp::Or => {
            require_boolean(binary_name(operator), left)?;
            require_boolean(binary_name(operator), right)?;
            Ok(ExpressionType::Value(LogicalType::Bool))
        }
    }
}

/// Types `+ - * /` under the refuse-never-round law:
///
/// - Int64/Float64/Null operands keep the historical rules: Float64 wins,
///   then Int64, and a wholly untyped null expression stays `Null`.
/// - `Decimal(p1,s) + Decimal(p2,s)` and `-` require equal scales (the
///   existing comparison message) and yield
///   `Decimal(max(p1,p2)+1 capped at 38, s)`.
/// - `Decimal(p1,s1) * Decimal(p2,s2)` yields `Decimal(min(p1+p2, 38),
///   s1+s2)`; `s1+s2 > 38` is a typing error.
/// - `Decimal / Decimal` stays REFUSED: exact division does not exist in
///   general, so the message points at `round_div` (mirroring the `avg`
///   refusal style).
/// - Mixed `Decimal ∘ Int64` for `+ - *` promotes the Int64 operand EXACTLY:
///   an i64 always fits `Decimal(19, 0)`, and for `+`/`-` it is then scaled
///   by 10^s to the Decimal operand's scale (overflow-checked at runtime),
///   so the promotion is exact by construction and never rounds.
/// - `Float64 ∘ Decimal` stays refused: a float cannot represent Decimal
///   values exactly, so mixing would round.
/// - A `Null` operand propagates exactly as Int64 arithmetic does: the
///   result type is the other operand's type.
fn arithmetic_type(
    operator: BinaryOp,
    left: ExpressionType,
    right: ExpressionType,
) -> DevonResult<ExpressionType> {
    if is_decimal_value(left) || is_decimal_value(right) {
        return decimal_arithmetic_type(operator, left, right);
    }
    require_numeric(binary_name(operator), left)?;
    require_numeric(binary_name(operator), right)?;
    if matches!(left, ExpressionType::Value(LogicalType::Float64))
        || matches!(right, ExpressionType::Value(LogicalType::Float64))
    {
        Ok(ExpressionType::Value(LogicalType::Float64))
    } else if matches!(left, ExpressionType::Value(LogicalType::Int64))
        || matches!(right, ExpressionType::Value(LogicalType::Int64))
    {
        Ok(ExpressionType::Value(LogicalType::Int64))
    } else {
        Ok(ExpressionType::Null)
    }
}

const fn is_decimal_value(ty: ExpressionType) -> bool {
    matches!(ty, ExpressionType::Value(LogicalType::Decimal { .. }))
}

/// Applies the decimal arithmetic rules documented on [`arithmetic_type`].
fn decimal_arithmetic_type(
    operator: BinaryOp,
    left: ExpressionType,
    right: ExpressionType,
) -> DevonResult<ExpressionType> {
    require_decimal_arithmetic_operand(operator, left)?;
    require_decimal_arithmetic_operand(operator, right)?;
    refuse_float_decimal_mix(operator, left, right)?;
    if operator == BinaryOp::Div {
        return Err(invalid_argument(
            "div over Decimal would round; use round_div(numerator, denominator, places) to name the rounding explicitly",
        ));
    }
    match (left, right) {
        (ExpressionType::Value(LogicalType::Decimal { precision, scale }), other) => {
            decimal_mixed_type(operator, precision, scale, other)
        }
        (other, ExpressionType::Value(LogicalType::Decimal { precision, scale })) => {
            decimal_mixed_type(operator, precision, scale, other)
        }
        _ => unreachable!("decimal arithmetic typing requires a Decimal operand"),
    }
}

/// The operand mix accepted beside a Decimal: `Null`, `Int64` (promoted
/// exactly), or another `Decimal`. Float64 is refused separately.
fn require_decimal_arithmetic_operand(
    operator: BinaryOp,
    operand: ExpressionType,
) -> DevonResult<()> {
    if matches!(operand, ExpressionType::Null)
        || is_decimal_value(operand)
        || matches!(
            operand,
            ExpressionType::Value(LogicalType::Int64 | LogicalType::Float64)
        )
    {
        Ok(())
    } else {
        Err(invalid_argument(format!(
            "operator `{}` requires a numeric operand; got {}",
            binary_name(operator),
            type_name(operand)
        )))
    }
}

fn refuse_float_decimal_mix(
    operator: BinaryOp,
    left: ExpressionType,
    right: ExpressionType,
) -> DevonResult<()> {
    if matches!(left, ExpressionType::Value(LogicalType::Float64))
        || matches!(right, ExpressionType::Value(LogicalType::Float64))
    {
        return Err(invalid_argument(format!(
            "operator `{}` over Decimal and Float64 would round; exact decimal arithmetic refuses Float64 operands",
            binary_name(operator)
        )));
    }
    Ok(())
}

fn decimal_mixed_type(
    operator: BinaryOp,
    precision: u8,
    scale: u8,
    other: ExpressionType,
) -> DevonResult<ExpressionType> {
    match other {
        ExpressionType::Null => Ok(ExpressionType::Value(LogicalType::Decimal {
            precision,
            scale,
        })),
        ExpressionType::Value(LogicalType::Int64) => decimal_int64_type(operator, precision, scale),
        ExpressionType::Value(LogicalType::Decimal {
            precision: other_precision,
            scale: other_scale,
        }) => decimal_pair_result(operator, precision, scale, other_precision, other_scale),
        _ => unreachable!("decimal arithmetic operand kinds are validated above"),
    }
}

/// Types `Decimal(p,s) ∘ Int64`: the Int64 promotes exactly to
/// `Decimal(19, 0)` (every i64 fits), and for `+`/`-` it is then scaled by
/// 10^s — overflow-checked at runtime — because scales must be equal.
fn decimal_int64_type(operator: BinaryOp, precision: u8, scale: u8) -> DevonResult<ExpressionType> {
    const INT64_DIGITS: u16 = 19;
    let result_precision = match operator {
        BinaryOp::Add | BinaryOp::Sub => {
            u16::from(precision).max(INT64_DIGITS + u16::from(scale)) + 1
        }
        BinaryOp::Mul => u16::from(precision) + INT64_DIGITS,
        _ => unreachable!("decimal division is refused above"),
    };
    Ok(ExpressionType::Value(LogicalType::Decimal {
        precision: capped_precision(result_precision),
        scale,
    }))
}

fn decimal_pair_result(
    operator: BinaryOp,
    left_precision: u8,
    left_scale: u8,
    right_precision: u8,
    right_scale: u8,
) -> DevonResult<ExpressionType> {
    match operator {
        BinaryOp::Add | BinaryOp::Sub => {
            if left_scale != right_scale {
                return Err(invalid_argument(format!(
                    "operator `{}` requires equal Decimal scales; got scales {left_scale} and {right_scale}",
                    binary_name(operator)
                )));
            }
            Ok(ExpressionType::Value(LogicalType::Decimal {
                precision: capped_precision(u16::from(left_precision.max(right_precision)) + 1),
                scale: left_scale,
            }))
        }
        BinaryOp::Mul => {
            let scale = left_scale + right_scale;
            if scale > MAX_PRECISION {
                return Err(invalid_argument(format!(
                    "operator `mul` would produce Decimal scale {scale} above the maximum {MAX_PRECISION}; got scales {left_scale} and {right_scale}"
                )));
            }
            Ok(ExpressionType::Value(LogicalType::Decimal {
                precision: capped_precision(u16::from(left_precision) + u16::from(right_precision)),
                scale,
            }))
        }
        _ => unreachable!("decimal division is refused above"),
    }
}

fn capped_precision(digits: u16) -> u8 {
    u8::try_from(digits.min(u16::from(MAX_PRECISION))).unwrap_or(MAX_PRECISION)
}
fn comparison_type(
    operator: BinaryOp,
    left: ExpressionType,
    right: ExpressionType,
) -> DevonResult<ExpressionType> {
    if let Some((left_scale, right_scale)) = unequal_decimal_scales(left, right) {
        return Err(invalid_argument(format!(
            "operator `{}` requires equal Decimal scales; got scales {left_scale} and {right_scale}",
            binary_name(operator)
        )));
    }
    if let Some(ty) = equality_only_type(operator, left, right) {
        return Err(invalid_argument(format!(
            "operator `{}` does not accept {ty} operands; {ty} comparisons admit equality only",
            binary_name(operator)
        )));
    }
    if comparable(operator, left, right) {
        Ok(ExpressionType::Value(LogicalType::Bool))
    } else {
        Err(invalid_argument(format!(
            "operator `{}` requires comparable operands; got {} and {}",
            binary_name(operator),
            type_name(left),
            type_name(right)
        )))
    }
}

const fn unequal_decimal_scales(left: ExpressionType, right: ExpressionType) -> Option<(u8, u8)> {
    match (left, right) {
        (
            ExpressionType::Value(LogicalType::Decimal {
                scale: left_scale, ..
            }),
            ExpressionType::Value(LogicalType::Decimal {
                scale: right_scale, ..
            }),
        ) if left_scale != right_scale => Some((left_scale, right_scale)),
        _ => None,
    }
}

fn equality_only_type(
    operator: BinaryOp,
    left: ExpressionType,
    right: ExpressionType,
) -> Option<LogicalType> {
    if is_equality_comparison(operator) {
        return None;
    }
    match (left, right) {
        (ExpressionType::Null, ExpressionType::Value(ty))
        | (ExpressionType::Value(ty), ExpressionType::Null)
            if equality_only_scalar(ty) =>
        {
            Some(ty)
        }
        (ExpressionType::Value(left), ExpressionType::Value(right))
            if left == right && equality_only_scalar(left) =>
        {
            Some(left)
        }
        _ => None,
    }
}

fn comparable(operator: BinaryOp, left: ExpressionType, right: ExpressionType) -> bool {
    match (left, right) {
        (ExpressionType::Null, ExpressionType::Null) => true,
        (ExpressionType::Null, ExpressionType::Value(ty))
        | (ExpressionType::Value(ty), ExpressionType::Null) => scalar_comparable(operator, ty),
        (
            ExpressionType::Value(LogicalType::Decimal {
                scale: left_scale, ..
            }),
            ExpressionType::Value(LogicalType::Decimal {
                scale: right_scale, ..
            }),
        ) => left_scale == right_scale,
        (ExpressionType::Value(left), ExpressionType::Value(right)) => {
            (left == right && scalar_comparable(operator, left))
                || (numeric_type(left) && numeric_type(right))
        }
    }
}

const fn scalar_comparable(operator: BinaryOp, ty: LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::Bool
            | LogicalType::Int64
            | LogicalType::Float64
            | LogicalType::String
            | LogicalType::Timestamp
            | LogicalType::Decimal { .. }
    ) || (is_equality_comparison(operator) && equality_only_scalar(ty))
}

const fn equality_only_scalar(ty: LogicalType) -> bool {
    matches!(ty, LogicalType::Bytes | LogicalType::Json)
}

const fn is_equality_comparison(operator: BinaryOp) -> bool {
    matches!(operator, BinaryOp::Eq | BinaryOp::Ne)
}

fn require_numeric(operator: &str, operand: ExpressionType) -> DevonResult<()> {
    if matches!(operand, ExpressionType::Null)
        || matches!(operand, ExpressionType::Value(ty) if numeric_type(ty))
    {
        Ok(())
    } else {
        Err(invalid_argument(format!(
            "operator `{operator}` requires a numeric operand; got {}",
            type_name(operand)
        )))
    }
}

fn reject_unordered_type(operator: &str, operand: ExpressionType) -> DevonResult<()> {
    let ExpressionType::Value(ty) = operand else {
        return Ok(());
    };
    // PLAN_IR § Type system: Bytes and Json admit equality only; Vector and
    // GeoPoint are not scalar-comparable. None is an ordered scalar.
    if matches!(
        ty,
        LogicalType::Vector { .. } | LogicalType::GeoPoint | LogicalType::Bytes | LogicalType::Json
    ) {
        return Err(invalid_argument(format!(
            "operator `{operator}` requires an ordered scalar; got {}",
            type_name(operand)
        )));
    }
    Ok(())
}

const fn aggregate_name(function: AggregateFunction) -> &'static str {
    match function {
        AggregateFunction::Count => "count",
        AggregateFunction::Sum => "sum",
        AggregateFunction::Min => "min",
        AggregateFunction::Max => "max",
        AggregateFunction::Avg => "avg",
        AggregateFunction::PercentileCont => "percentile_cont",
    }
}

fn require_int64(operator: &str, operand: ExpressionType) -> DevonResult<()> {
    if matches!(
        operand,
        ExpressionType::Null | ExpressionType::Value(LogicalType::Int64)
    ) {
        Ok(())
    } else {
        Err(invalid_argument(format!(
            "operator `{operator}` requires an Int64 operand; got {}",
            type_name(operand)
        )))
    }
}

fn require_decimal(operator: &str, operand: ExpressionType) -> DevonResult<()> {
    if matches!(operand, ExpressionType::Null)
        || matches!(operand, ExpressionType::Value(LogicalType::Decimal { .. }))
    {
        Ok(())
    } else {
        Err(invalid_argument(format!(
            "operator `{operator}` requires a Decimal operand; got {}",
            type_name(operand)
        )))
    }
}

fn require_boolean(operator: &str, operand: ExpressionType) -> DevonResult<()> {
    if matches!(
        operand,
        ExpressionType::Null | ExpressionType::Value(LogicalType::Bool)
    ) {
        Ok(())
    } else {
        Err(invalid_argument(format!(
            "operator `{operator}` requires a Bool operand; got {}",
            type_name(operand)
        )))
    }
}

fn require_matching_vectors(left: ExpressionType, right: ExpressionType) -> DevonResult<()> {
    match (left, right) {
        (ExpressionType::Null, ExpressionType::Null) => Ok(()),
        (ExpressionType::Null, ExpressionType::Value(LogicalType::Vector { .. }))
        | (ExpressionType::Value(LogicalType::Vector { .. }), ExpressionType::Null) => Ok(()),
        (
            ExpressionType::Value(LogicalType::Vector { dim: left }),
            ExpressionType::Value(LogicalType::Vector { dim: right }),
        ) if left == right => Ok(()),
        _ => Err(invalid_argument(format!(
            "operator `distance` requires equal-dimension Vector operands; got {} and {}",
            type_name(left),
            type_name(right)
        ))),
    }
}

fn selection_type<'a>(
    operator: &str,
    expressions: impl IntoIterator<Item = &'a Expr>,
    columns: &HashMap<String, LogicalType>,
    scalar_type: &mut impl FnMut(&Operator) -> DevonResult<ExpressionType>,
) -> DevonResult<ExpressionType> {
    let mut result = ExpressionType::Null;
    for expression in expressions {
        let operand = expression_type_with_subqueries(expression, columns, scalar_type)?;
        result = unify_selection_type(operator, result, operand)?;
    }
    Ok(result)
}

fn variadic_type(
    operator: &str,
    expressions: &[Expr],
    columns: &HashMap<String, LogicalType>,
    scalar_type: &mut impl FnMut(&Operator) -> DevonResult<ExpressionType>,
) -> DevonResult<ExpressionType> {
    require_variadic_arity(operator, expressions)?;
    selection_type(operator, expressions, columns, scalar_type)
}

fn require_variadic_arity(operator: &str, expressions: &[Expr]) -> DevonResult<()> {
    if expressions.len() >= 2 {
        return Ok(());
    }
    Err(invalid_argument(format!(
        "operator `{operator}` requires at least 2 operands; got {}",
        expressions.len()
    )))
}

fn unify_selection_type(
    operator: &str,
    left: ExpressionType,
    right: ExpressionType,
) -> DevonResult<ExpressionType> {
    match (left, right) {
        (ExpressionType::Null, other) | (other, ExpressionType::Null) => Ok(other),
        (
            ExpressionType::Value(LogicalType::Decimal {
                precision: left_precision,
                scale: left_scale,
            }),
            ExpressionType::Value(LogicalType::Decimal {
                precision: right_precision,
                scale: right_scale,
            }),
        ) if left_scale == right_scale => Ok(ExpressionType::Value(LogicalType::Decimal {
            precision: left_precision.max(right_precision),
            scale: left_scale,
        })),
        (ExpressionType::Value(left), ExpressionType::Value(right)) if left == right => {
            Ok(ExpressionType::Value(left))
        }
        _ => Err(invalid_argument(format!(
            "operator `{operator}` requires exactly matching operand types; got {} and {}",
            type_name(left),
            type_name(right)
        ))),
    }
}

fn extrema_type(
    operator: &str,
    expressions: &[Expr],
    columns: &HashMap<String, LogicalType>,
    scalar_type: &mut impl FnMut(&Operator) -> DevonResult<ExpressionType>,
) -> DevonResult<ExpressionType> {
    let result = variadic_type(operator, expressions, columns, scalar_type)?;
    match result {
        ExpressionType::Null => Ok(ExpressionType::Null),
        ExpressionType::Value(ty) if ordered_scalar(ty) => Ok(result),
        _ => Err(invalid_argument(format!(
            "operator `{operator}` requires ordered scalar operands; got {}",
            type_name(result)
        ))),
    }
}

const fn ordered_scalar(ty: LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::Bool
            | LogicalType::Int64
            | LogicalType::Float64
            | LogicalType::String
            | LogicalType::Timestamp
            | LogicalType::Decimal { .. }
    )
}

fn require_timestamp(operator: &str, operand: ExpressionType) -> DevonResult<()> {
    if matches!(
        operand,
        ExpressionType::Null | ExpressionType::Value(LogicalType::Timestamp)
    ) {
        Ok(())
    } else {
        Err(invalid_argument(format!(
            "operator `{operator}` requires a Timestamp operand; got {}",
            type_name(operand)
        )))
    }
}

const fn numeric_type(ty: LogicalType) -> bool {
    matches!(ty, LogicalType::Int64 | LogicalType::Float64)
}

fn type_name(ty: ExpressionType) -> String {
    match ty {
        ExpressionType::Null => "Null".to_owned(),
        ExpressionType::Value(ty) => ty.to_string(),
    }
}

const fn binary_name(operator: BinaryOp) -> &'static str {
    match operator {
        BinaryOp::Eq => "eq",
        BinaryOp::Ne => "ne",
        BinaryOp::Lt => "lt",
        BinaryOp::Le => "le",
        BinaryOp::Gt => "gt",
        BinaryOp::Ge => "ge",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
        BinaryOp::Add => "add",
        BinaryOp::Sub => "sub",
        BinaryOp::Mul => "mul",
        BinaryOp::Div => "div",
    }
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use devondb_types::{DevonError, logical_type::LogicalType, value::Value};

    use super::{ExpressionType, aggregate_type, expression_type};
    use crate::expr::{BinaryOp, DateTruncUnit, Expr, Metric};
    use crate::ops::AggregateFunction;

    fn columns() -> HashMap<String, LogicalType> {
        HashMap::from([
            ("p.active".to_owned(), LogicalType::Bool),
            ("p.age".to_owned(), LogicalType::Int64),
            (
                "p.amount".to_owned(),
                LogicalType::Decimal {
                    precision: 18,
                    scale: 4,
                },
            ),
            (
                "p.small_amount".to_owned(),
                LogicalType::Decimal {
                    precision: 9,
                    scale: 4,
                },
            ),
            (
                "p.other_scale".to_owned(),
                LogicalType::Decimal {
                    precision: 18,
                    scale: 2,
                },
            ),
            ("p.score".to_owned(), LogicalType::Float64),
            ("p.name".to_owned(), LogicalType::String),
            ("p.v3".to_owned(), LogicalType::Vector { dim: 3 }),
            ("p.v4".to_owned(), LogicalType::Vector { dim: 4 }),
            ("p.location".to_owned(), LogicalType::GeoPoint),
            ("p.blob".to_owned(), LogicalType::Bytes),
            ("p.doc".to_owned(), LogicalType::Json),
            ("p.created_at".to_owned(), LogicalType::Timestamp),
            ("p.payload".to_owned(), LogicalType::Bytes),
            ("p.document".to_owned(), LogicalType::Json),
        ])
    }

    fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn conditional(cond: Expr, then_expr: Expr, else_expr: Expr) -> Expr {
        Expr::If {
            cond: Box::new(cond),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
        }
    }

    fn trunc(value: Expr) -> Expr {
        Expr::DateTrunc {
            unit: DateTruncUnit::Day,
            value: Box::new(value),
        }
    }

    #[test]
    fn arithmetic_promotes_only_for_float_operands_and_pins_integer_division() {
        let integer_division = binary(
            BinaryOp::Div,
            Expr::Col("p.age".into()),
            Expr::Lit(Value::Int64(2)),
        );
        let promoted = binary(
            BinaryOp::Add,
            Expr::Col("p.age".into()),
            Expr::Col("p.score".into()),
        );

        assert_eq!(
            expression_type(&integer_division, &columns()).unwrap(),
            ExpressionType::Value(LogicalType::Int64)
        );
        assert_eq!(
            expression_type(&promoted, &columns()).unwrap(),
            ExpressionType::Value(LogicalType::Float64)
        );
    }

    #[test]
    fn invalid_arithmetic_boolean_comparison_and_distance_types_are_rejected() {
        let cases = [
            binary(
                BinaryOp::Add,
                Expr::Col("p.name".into()),
                Expr::Col("p.name".into()),
            ),
            binary(
                BinaryOp::And,
                Expr::Col("p.age".into()),
                Expr::Col("p.active".into()),
            ),
            binary(
                BinaryOp::Eq,
                Expr::Col("p.name".into()),
                Expr::Col("p.age".into()),
            ),
            Expr::Distance {
                left: Box::new(Expr::Col("p.v3".into())),
                right: Box::new(Expr::Col("p.v4".into())),
                metric: Metric::L2,
            },
        ];

        for expression in cases {
            assert!(expression_type(&expression, &columns()).is_err());
        }
    }

    #[test]
    fn null_is_admitted_in_every_operand_position() {
        let null = Expr::Lit(Value::Null);
        let cases = [
            binary(BinaryOp::Add, null.clone(), Expr::Col("p.age".into())),
            binary(BinaryOp::Eq, null.clone(), Expr::Col("p.name".into())),
            binary(BinaryOp::And, null.clone(), Expr::Col("p.active".into())),
            Expr::Not(Box::new(null.clone())),
            Expr::Distance {
                left: Box::new(null),
                right: Box::new(Expr::Col("p.v3".into())),
                metric: Metric::Cosine,
            },
        ];

        for expression in cases {
            assert!(expression_type(&expression, &columns()).is_ok());
        }
    }

    #[test]
    fn scalar_v2_ordered_types_accept_every_comparison_with_values_and_null() {
        let operators = [
            BinaryOp::Eq,
            BinaryOp::Ne,
            BinaryOp::Lt,
            BinaryOp::Le,
            BinaryOp::Gt,
            BinaryOp::Ge,
        ];
        for operator in operators {
            for (left, right) in [
                ("p.created_at", "p.created_at"),
                ("p.amount", "p.small_amount"),
            ] {
                let expression = binary(operator, Expr::Col(left.into()), Expr::Col(right.into()));
                assert_eq!(
                    expression_type(&expression, &columns()).unwrap(),
                    ExpressionType::Value(LogicalType::Bool)
                );
            }
            for reference in ["p.created_at", "p.amount"] {
                let expression = binary(
                    operator,
                    Expr::Col(reference.into()),
                    Expr::Lit(Value::Null),
                );
                assert!(expression_type(&expression, &columns()).is_ok());
            }
        }
    }

    #[test]
    fn bytes_and_json_accept_equality_and_null_but_reject_ordering() {
        for operator in [BinaryOp::Eq, BinaryOp::Ne] {
            for reference in ["p.payload", "p.document"] {
                for right in [Expr::Col(reference.into()), Expr::Lit(Value::Null)] {
                    let expression = binary(operator, Expr::Col(reference.into()), right);
                    assert_eq!(
                        expression_type(&expression, &columns()).unwrap(),
                        ExpressionType::Value(LogicalType::Bool)
                    );
                }
            }
        }

        for (reference, ty) in [("p.payload", "Bytes"), ("p.document", "Json")] {
            for operator in [BinaryOp::Lt, BinaryOp::Le, BinaryOp::Gt, BinaryOp::Ge] {
                let expression = binary(
                    operator,
                    Expr::Col(reference.into()),
                    Expr::Col(reference.into()),
                );
                let error = expression_type(&expression, &columns())
                    .unwrap_err()
                    .to_string();
                assert_eq!(
                    error,
                    format!(
                        "invalid argument: operator `{}` does not accept {ty} operands; {ty} comparisons admit equality only",
                        super::binary_name(operator)
                    )
                );
            }
        }
    }

    #[test]
    fn decimal_scale_mismatch_and_disallowed_comparisons_are_rejected() {
        let unequal_scale = binary(
            BinaryOp::Lt,
            Expr::Col("p.amount".into()),
            Expr::Col("p.other_scale".into()),
        );
        assert_eq!(
            expression_type(&unequal_scale, &columns())
                .unwrap_err()
                .to_string(),
            "invalid argument: operator `lt` requires equal Decimal scales; got scales 4 and 2"
        );

        for operator in [
            BinaryOp::Eq,
            BinaryOp::Ne,
            BinaryOp::Lt,
            BinaryOp::Le,
            BinaryOp::Gt,
            BinaryOp::Ge,
        ] {
            for (left, right) in [
                ("p.created_at", "p.age"),
                ("p.amount", "p.age"),
                ("p.v3", "p.v3"),
                ("p.location", "p.location"),
            ] {
                let expression = binary(operator, Expr::Col(left.into()), Expr::Col(right.into()));
                let error = expression_type(&expression, &columns())
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("requires comparable operands"), "{error}");
            }
        }
    }

    #[test]
    fn conditional_and_coalesce_infer_exact_types_and_all_null() {
        let null = || Expr::Lit(Value::Null);
        let accepted = [
            (
                conditional(
                    Expr::Col("p.active".into()),
                    Expr::Col("p.age".into()),
                    null(),
                ),
                ExpressionType::Value(LogicalType::Int64),
            ),
            (
                conditional(null(), null(), Expr::Col("p.score".into())),
                ExpressionType::Value(LogicalType::Float64),
            ),
            (
                Expr::Coalesce(vec![null(), Expr::Col("p.v3".into()), null()]),
                ExpressionType::Value(LogicalType::Vector { dim: 3 }),
            ),
            (
                Expr::Coalesce(vec![Expr::Col("p.payload".into()), null()]),
                ExpressionType::Value(LogicalType::Bytes),
            ),
            (
                Expr::Coalesce(vec![Expr::Col("p.document".into()), null()]),
                ExpressionType::Value(LogicalType::Json),
            ),
            (
                Expr::Coalesce(vec![Expr::Col("p.location".into()), null()]),
                ExpressionType::Value(LogicalType::GeoPoint),
            ),
            (conditional(null(), null(), null()), ExpressionType::Null),
            (
                Expr::Coalesce(vec![null(), null(), null()]),
                ExpressionType::Null,
            ),
        ];

        for (expression, expected) in accepted {
            assert_eq!(expression_type(&expression, &columns()).unwrap(), expected);
        }
        assert_eq!(ExpressionType::Null.output_type(), LogicalType::Bool);
    }

    #[test]
    fn conditional_and_selection_mismatches_are_rejected_without_promotion() {
        let cases = [
            conditional(
                Expr::Col("p.age".into()),
                Expr::Lit(Value::Int64(1)),
                Expr::Lit(Value::Int64(2)),
            ),
            conditional(
                Expr::Col("p.active".into()),
                Expr::Col("p.age".into()),
                Expr::Col("p.score".into()),
            ),
            Expr::Coalesce(vec![Expr::Col("p.v3".into()), Expr::Col("p.v4".into())]),
            Expr::Least(vec![Expr::Col("p.age".into()), Expr::Col("p.score".into())]),
            Expr::Greatest(vec![
                Expr::Col("p.name".into()),
                Expr::Col("p.created_at".into()),
            ]),
        ];

        for expression in cases {
            assert!(expression_type(&expression, &columns()).is_err());
        }
    }

    #[test]
    fn decimal_selection_widens_precision_only_at_equal_scale() {
        let expected = ExpressionType::Value(LogicalType::Decimal {
            precision: 18,
            scale: 4,
        });
        let same_scale = || {
            vec![
                Expr::Col("p.small_amount".into()),
                Expr::Col("p.amount".into()),
            ]
        };
        let accepted = [
            conditional(
                Expr::Col("p.active".into()),
                Expr::Col("p.small_amount".into()),
                Expr::Col("p.amount".into()),
            ),
            Expr::Coalesce(same_scale()),
            Expr::Least(same_scale()),
            Expr::Greatest(same_scale()),
        ];
        for expression in accepted {
            assert_eq!(expression_type(&expression, &columns()).unwrap(), expected);
        }

        for expression in [
            Expr::Coalesce(vec![
                Expr::Col("p.amount".into()),
                Expr::Col("p.other_scale".into()),
            ]),
            Expr::Greatest(vec![
                Expr::Col("p.amount".into()),
                Expr::Col("p.other_scale".into()),
            ]),
        ] {
            let error = expression_type(&expression, &columns())
                .unwrap_err()
                .to_string();
            assert!(error.contains("Decimal(18, 4)"), "{error}");
            assert!(error.contains("Decimal(18, 2)"), "{error}");
        }
    }

    #[test]
    fn least_and_greatest_accept_exactly_ordered_scalars_and_all_null() {
        let ordered = [
            ("p.active", LogicalType::Bool),
            ("p.age", LogicalType::Int64),
            ("p.score", LogicalType::Float64),
            ("p.name", LogicalType::String),
            ("p.created_at", LogicalType::Timestamp),
            (
                "p.amount",
                LogicalType::Decimal {
                    precision: 18,
                    scale: 4,
                },
            ),
        ];
        for (reference, ty) in ordered {
            let operands = || vec![Expr::Lit(Value::Null), Expr::Col(reference.to_owned())];
            for expression in [Expr::Least(operands()), Expr::Greatest(operands())] {
                assert_eq!(
                    expression_type(&expression, &columns()).unwrap(),
                    ExpressionType::Value(ty)
                );
            }
        }

        for expression in [
            Expr::Least(vec![Expr::Lit(Value::Null), Expr::Lit(Value::Null)]),
            Expr::Greatest(vec![Expr::Lit(Value::Null), Expr::Lit(Value::Null)]),
        ] {
            assert_eq!(
                expression_type(&expression, &columns()).unwrap(),
                ExpressionType::Null
            );
        }
    }

    #[test]
    fn extrema_rejections_name_each_non_ordered_type() {
        for (reference, type_name) in [
            ("p.v3", "Vector(3)"),
            ("p.location", "GeoPoint"),
            ("p.payload", "Bytes"),
            ("p.document", "Json"),
        ] {
            let operands = || vec![Expr::Col(reference.to_owned()), Expr::Lit(Value::Null)];
            for expression in [Expr::Least(operands()), Expr::Greatest(operands())] {
                let error = expression_type(&expression, &columns())
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(type_name), "{error}");
                assert!(error.contains("ordered scalar"), "{error}");
            }
        }
    }

    #[test]
    fn variadic_expressions_reject_programmatic_wrong_arities() {
        for expression in [
            Expr::Coalesce(Vec::new()),
            Expr::Coalesce(vec![Expr::Lit(Value::Null)]),
            Expr::Least(Vec::new()),
            Expr::Greatest(vec![Expr::Lit(Value::Int64(1))]),
        ] {
            let error = expression_type(&expression, &columns())
                .unwrap_err()
                .to_string();
            assert!(error.contains("at least 2 operands"), "{error}");
            assert!(serde_json::to_string(&expression).is_err());
        }
    }

    #[test]
    fn date_trunc_accepts_only_timestamp_or_null_and_always_returns_timestamp() {
        for operand in [
            Expr::Col("p.created_at".into()),
            Expr::Lit(Value::Timestamp(0)),
            Expr::Lit(Value::Null),
        ] {
            assert_eq!(
                expression_type(&trunc(operand), &columns()).unwrap(),
                ExpressionType::Value(LogicalType::Timestamp)
            );
        }

        for (reference, type_name) in [
            ("p.active", "Bool"),
            ("p.age", "Int64"),
            ("p.score", "Float64"),
            ("p.name", "String"),
            ("p.v3", "Vector(3)"),
            ("p.location", "GeoPoint"),
            ("p.payload", "Bytes"),
            ("p.amount", "Decimal(18, 4)"),
            ("p.document", "Json"),
        ] {
            let error = expression_type(&trunc(Expr::Col(reference.into())), &columns())
                .unwrap_err()
                .to_string();
            assert!(error.contains(type_name), "{error}");
        }
    }

    #[test]
    fn new_expressions_type_when_nested_in_existing_composites() {
        let conditional_vector = conditional(
            Expr::Col("p.active".into()),
            Expr::Col("p.v3".into()),
            Expr::Lit(Value::Null),
        );
        let distance = Expr::Distance {
            left: Box::new(conditional_vector),
            right: Box::new(Expr::Coalesce(vec![
                Expr::Lit(Value::Null),
                Expr::Col("p.v3".into()),
            ])),
            metric: Metric::Cosine,
        };
        assert_eq!(
            expression_type(&distance, &columns()).unwrap(),
            ExpressionType::Value(LogicalType::Float64)
        );

        let all_active = Expr::Not(Box::new(Expr::Least(vec![
            Expr::Col("p.active".into()),
            Expr::Lit(Value::Bool(false)),
        ])));
        assert_eq!(
            expression_type(&all_active, &columns()).unwrap(),
            ExpressionType::Value(LogicalType::Bool)
        );

        let timestamp = Expr::Coalesce(vec![
            trunc(Expr::Col("p.created_at".into())),
            Expr::Col("p.created_at".into()),
        ]);
        assert_eq!(
            expression_type(&timestamp, &columns()).unwrap(),
            ExpressionType::Value(LogicalType::Timestamp)
        );
    }

    #[test]
    fn aggregates_share_expression_rules_and_require_numeric_sum_and_avg() {
        assert!(
            aggregate_type(
                AggregateFunction::Sum,
                &Expr::Col("p.name".into()),
                &columns()
            )
            .is_err()
        );
        assert!(
            aggregate_type(
                AggregateFunction::Avg,
                &Expr::Col("p.active".into()),
                &columns()
            )
            .is_err()
        );
        assert_eq!(
            aggregate_type(
                AggregateFunction::Avg,
                &Expr::Col("p.age".into()),
                &columns()
            )
            .unwrap(),
            LogicalType::Float64
        );
        assert_eq!(
            aggregate_type(
                AggregateFunction::Count,
                &Expr::Lit(Value::Null),
                &columns()
            )
            .unwrap(),
            LogicalType::Int64
        );
    }

    #[test]
    fn decimal_aggregate_typing_accepts_exact_operations_and_rejects_avg() {
        let amount = Expr::Col("p.amount".into());
        let decimal_type = LogicalType::Decimal {
            precision: 18,
            scale: 4,
        };

        for function in [
            AggregateFunction::Sum,
            AggregateFunction::Min,
            AggregateFunction::Max,
        ] {
            assert_eq!(
                aggregate_type(function, &amount, &columns()).unwrap(),
                decimal_type
            );
        }
        assert_eq!(
            aggregate_type(AggregateFunction::Count, &amount, &columns()).unwrap(),
            LogicalType::Int64
        );

        let DevonError::InvalidArgument { context } =
            aggregate_type(AggregateFunction::Avg, &amount, &columns()).unwrap_err()
        else {
            panic!("expected InvalidArgument");
        };
        assert_eq!(
            context,
            "avg over Decimal would round; compute sum and count and divide consumer-side"
        );
    }

    #[test]
    fn min_and_max_reject_unordered_types_naming_type_and_operator() {
        for (expression, ty) in [
            (Expr::Col("p.v3".into()), "Vector"),
            (Expr::Col("p.location".into()), "GeoPoint"),
            (Expr::Col("p.blob".into()), "Bytes"),
            (Expr::Col("p.doc".into()), "Json"),
        ] {
            for function in [AggregateFunction::Min, AggregateFunction::Max] {
                let DevonError::InvalidArgument { context } =
                    aggregate_type(function, &expression, &columns()).unwrap_err()
                else {
                    panic!("expected InvalidArgument");
                };
                assert!(
                    context.contains(&format!("`{}`", function_name(function))),
                    "{context}"
                );
                assert!(context.contains(ty), "{context}");
                assert!(context.contains("ordered scalar"), "{context}");
            }
        }
    }

    fn function_name(function: AggregateFunction) -> &'static str {
        match function {
            AggregateFunction::Count => "count",
            AggregateFunction::Sum => "sum",
            AggregateFunction::Min => "min",
            AggregateFunction::Max => "max",
            AggregateFunction::Avg => "avg",
            AggregateFunction::PercentileCont => "percentile_cont",
        }
    }
}
