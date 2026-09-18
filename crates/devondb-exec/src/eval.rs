//! Expression evaluation: `Expr` × [`crate::chunk::Chunk`] → a column of values.
//!
//! Semantics (binding for this module):
//! - Null propagates through comparisons, arithmetic, `not`, and `distance`;
//!   `and`/`or` use Kleene three-valued logic.
//! - `Filter` semantics downstream: a row survives only when the predicate
//!   evaluates to exactly `Bool(true)` — `Null` is not true.
//! - Type errors are loud: mismatched operand types are
//!   `DevonError::InvalidArgument`, never a silent coercion (the sole
//!   coercion is Int64 → Float64 numeric promotion).
//! - `if` and `coalesce` evaluate only rows that still select an operand;
//!   extrema remain eager, skip nulls, and use the deterministic scalar order.

use std::cmp::Ordering;
use std::collections::HashMap;

use devondb_plan::{
    expr::{BinaryOp, DateTruncUnit, Expr, Metric},
    typing::{ExpressionType, expression_type},
};
use devondb_types::{
    DevonError, DevonResult,
    decimal::{Decimal128, MAX_PRECISION},
    logical_type::LogicalType,
    schema::fold,
    value::Value,
};

use crate::chunk::Chunk;

/// Evaluates `expr` against every row in `chunk`.
///
/// `columns` maps full DevonPlan column references to their zero-based column
/// index in the chunk. The returned vector always contains one value per row.
pub fn evaluate(
    expr: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
) -> DevonResult<Vec<Value>> {
    let rows = (0..chunk.row_count()).collect::<Vec<_>>();
    evaluate_selected(expr, chunk, columns, &rows)
}

fn evaluate_selected(
    expr: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    match expr {
        Expr::Col(reference) => evaluate_column(reference, chunk, columns, rows),
        Expr::Lit(value) => Ok(vec![value.clone(); rows.len()]),
        Expr::Binary { op, left, right } => {
            evaluate_binary_column(*op, left, right, chunk, columns, rows)
        }
        Expr::Not(operand) => evaluate_not_column(operand, chunk, columns, rows),
        Expr::Distance {
            left,
            right,
            metric,
        } => evaluate_distance_column(*metric, left, right, chunk, columns, rows),
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => evaluate_if_column(cond, then_expr, else_expr, chunk, columns, rows),
        Expr::Coalesce(expressions) => evaluate_coalesce_column(expressions, chunk, columns, rows),
        Expr::Least(expressions) => {
            evaluate_extrema_column("least", expressions, chunk, columns, rows)
        }
        Expr::Greatest(expressions) => {
            evaluate_extrema_column("greatest", expressions, chunk, columns, rows)
        }
        Expr::DateTrunc { unit, value } => {
            evaluate_date_trunc_column(*unit, value, chunk, columns, rows)
        }
        Expr::DateAdd {
            unit,
            value,
            amount,
        } => evaluate_date_add_column(*unit, value, amount, chunk, columns, rows),
        Expr::Round { value, places } => {
            evaluate_round_column(value, *places, chunk, columns, rows)
        }
        Expr::RoundDiv {
            numerator,
            denominator,
            places,
        } => evaluate_round_div_column(numerator, denominator, *places, chunk, columns, rows),
        Expr::Scalar { .. } => Err(invalid_argument(
            "scalar subquery evaluation is not available in this evaluator",
        )),
        Expr::ClassOf(binding) => evaluate_classof(binding, chunk, columns, rows),
        Expr::ScoreOf(binding) => evaluate_scoreof(binding, chunk, columns, rows),
    }
}

/// Returns the private column-map key carrying an interface binding's class.
///
/// The NUL-delimited spelling cannot be produced by the text form and is
/// never exposed as a result column. Facade pipelines use this key to carry
/// `classof(binding)` through ordinary executor operators.
#[must_use]
pub fn classof_column_key(binding: &str) -> String {
    format!("\0devondb-classof\0{}", fold(binding))
}

/// Private column-map key carrying a TextScan binding's frozen score.
#[must_use]
pub fn scoreof_column_key(binding: &str) -> String {
    format!("\0devondb-scoreof\0{}", fold(binding))
}

fn evaluate_scoreof(
    binding: &str,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let index = columns.get(&scoreof_column_key(binding)).ok_or_else(|| {
        invalid_argument(format!("scoreof binding `{binding}` has no TextScan score"))
    })?;
    rows.iter()
        .map(|row| match chunk.value(*row, *index) {
            Some(value @ (Value::Float64(_) | Value::Null)) => Ok(value),
            _ => Err(invalid_argument("invalid TextScan score column")),
        })
        .collect()
}

fn evaluate_classof(
    binding: &str,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let key = classof_column_key(binding);
    let index = columns.get(&key).copied().ok_or_else(|| {
        invalid_argument(format!(
            "classof binding `{binding}` has no interface discriminator"
        ))
    })?;
    let values = chunk.column(index).ok_or_else(|| {
        invalid_argument(format!(
            "classof binding `{binding}` maps to out-of-range chunk column {index}"
        ))
    })?;
    rows.iter()
        .map(|row| {
            if *row >= values.len() {
                return Err(invalid_argument(format!(
                    "selected row {row} is out of range for classof binding `{binding}`"
                )));
            }
            match values.value_at(*row) {
                Value::String(class) => Ok(Value::String(class)),
                value => Err(invalid_argument(format!(
                    "classof binding `{binding}` contains non-String discriminator {value}"
                ))),
            }
        })
        .collect()
}

fn evaluate_column(
    reference: &str,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let index = columns
        .get(reference)
        .copied()
        .ok_or_else(|| invalid_argument(format!("unknown column reference `{reference}`")))?;
    let values = chunk.column(index).ok_or_else(|| {
        invalid_argument(format!(
            "column reference `{reference}` maps to out-of-range chunk column {index}"
        ))
    })?;
    rows.iter()
        .map(|row| {
            if *row >= values.len() {
                return Err(invalid_argument(format!(
                    "selected row {row} is out of range for column reference `{reference}`"
                )));
            }
            Ok(values.value_at(*row))
        })
        .collect()
}

fn evaluate_binary_column(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let left_values = evaluate_selected(left, chunk, columns, rows)?;
    let right_values = evaluate_selected(right, chunk, columns, rows)?;
    left_values
        .iter()
        .zip(&right_values)
        .map(|(left, right)| evaluate_binary(op, left, right))
        .collect()
}

fn evaluate_not_column(
    operand: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    evaluate_selected(operand, chunk, columns, rows)?
        .iter()
        .map(evaluate_not)
        .collect()
}

fn evaluate_distance_column(
    metric: Metric,
    left: &Expr,
    right: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let left_values = evaluate_selected(left, chunk, columns, rows)?;
    let right_values = evaluate_selected(right, chunk, columns, rows)?;
    left_values
        .iter()
        .zip(&right_values)
        .map(|(left, right)| evaluate_distance(metric, left, right))
        .collect()
}

fn evaluate_if_column(
    cond: &Expr,
    then_expr: &Expr,
    else_expr: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let conditions = evaluate_selected(cond, chunk, columns, rows)?;
    let mut then_rows = Vec::new();
    let mut then_positions = Vec::new();
    let mut else_rows = Vec::new();
    let mut else_positions = Vec::new();

    for (position, (row, condition)) in rows.iter().zip(conditions).enumerate() {
        match condition {
            Value::Bool(true) => {
                then_rows.push(*row);
                then_positions.push(position);
            }
            Value::Bool(false) | Value::Null => {
                else_rows.push(*row);
                else_positions.push(position);
            }
            value => return Err(invalid_if_condition(&value)),
        }
    }

    let mut result = vec![Value::Null; rows.len()];
    evaluate_branch_into(
        then_expr,
        chunk,
        columns,
        &then_rows,
        &then_positions,
        &mut result,
    )?;
    evaluate_branch_into(
        else_expr,
        chunk,
        columns,
        &else_rows,
        &else_positions,
        &mut result,
    )?;
    Ok(result)
}

fn evaluate_branch_into(
    expr: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
    positions: &[usize],
    result: &mut [Value],
) -> DevonResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let values = evaluate_selected(expr, chunk, columns, rows)?;
    for (position, value) in positions.iter().zip(values) {
        result[*position] = value;
    }
    Ok(())
}

fn evaluate_coalesce_column(
    expressions: &[Expr],
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    require_variadic_arity("coalesce", expressions)?;
    let mut result = vec![Value::Null; rows.len()];
    let mut pending_rows = rows.to_vec();
    let mut pending_positions = (0..rows.len()).collect::<Vec<_>>();

    for expression in expressions {
        if pending_rows.is_empty() {
            break;
        }
        let values = evaluate_selected(expression, chunk, columns, &pending_rows)?;
        let mut next_rows = Vec::new();
        let mut next_positions = Vec::new();
        for ((row, position), value) in pending_rows.iter().zip(&pending_positions).zip(values) {
            if matches!(value, Value::Null) {
                next_rows.push(*row);
                next_positions.push(*position);
            } else {
                result[*position] = value;
            }
        }
        pending_rows = next_rows;
        pending_positions = next_positions;
    }
    Ok(result)
}

fn evaluate_extrema_column(
    operator: &'static str,
    expressions: &[Expr],
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    require_variadic_arity(operator, expressions)?;
    let mut result = vec![Value::Null; rows.len()];
    for expression in expressions {
        let values = evaluate_selected(expression, chunk, columns, rows)?;
        for (current, candidate) in result.iter_mut().zip(values) {
            if matches!(candidate, Value::Null) {
                continue;
            }
            let replace = match current {
                Value::Null => true,
                _ => {
                    let ordering = compare_extrema_values(operator, &candidate, current)?;
                    ordering == extrema_replacement_order(operator)
                }
            };
            if replace {
                *current = candidate;
            }
        }
    }
    Ok(result)
}

fn require_variadic_arity(operator: &str, expressions: &[Expr]) -> DevonResult<()> {
    if expressions.len() < 2 {
        return Err(invalid_argument(format!(
            "operator `{operator}` requires at least two operands"
        )));
    }
    Ok(())
}

fn extrema_replacement_order(operator: &str) -> Ordering {
    match operator {
        "least" => Ordering::Less,
        "greatest" => Ordering::Greater,
        _ => unreachable!("extrema evaluator received an unknown operator"),
    }
}

fn compare_extrema_values(operator: &str, left: &Value, right: &Value) -> DevonResult<Ordering> {
    match (left, right) {
        (Value::Bool(left), Value::Bool(right)) => Ok(left.cmp(right)),
        (Value::Int64(left), Value::Int64(right)) => Ok(left.cmp(right)),
        (Value::Float64(left), Value::Float64(right)) => Ok(left.total_cmp(right)),
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        (Value::Timestamp(left), Value::Timestamp(right)) => Ok(left.cmp(right)),
        (Value::Decimal(left), Value::Decimal(right)) => {
            if left.scale() != right.scale() {
                return Err(invalid_argument(format!(
                    "operator `{operator}` requires equal Decimal scales; got scales {} and {}",
                    left.scale(),
                    right.scale()
                )));
            }
            Ok(left.digits().cmp(&right.digits()))
        }
        _ => Err(invalid_argument(format!(
            "operator `{operator}` cannot order operand types {} and {}",
            value_type(left),
            value_type(right)
        ))),
    }
}

fn evaluate_date_trunc_column(
    unit: DateTruncUnit,
    value: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let values = evaluate_selected(value, chunk, columns, rows)?;
    values
        .into_iter()
        .map(|value| evaluate_date_trunc(unit, value))
        .collect()
}

fn evaluate_date_trunc(unit: DateTruncUnit, value: Value) -> DevonResult<Value> {
    const MICROS_PER_DAY: i64 = 86_400_000_000;

    match (unit, value) {
        (_, Value::Null) => Ok(Value::Null),
        (DateTruncUnit::Day, Value::Timestamp(micros)) => micros
            .div_euclid(MICROS_PER_DAY)
            .checked_mul(MICROS_PER_DAY)
            .map(Value::Timestamp)
            .ok_or_else(|| {
                invalid_argument(format!(
                    "operator `date_trunc(day)` cannot floor Timestamp {micros} within the i64 Timestamp domain"
                ))
            }),
        (DateTruncUnit::Day, value) => Err(invalid_argument(format!(
            "operator `date_trunc(day)` requires a Timestamp operand; got {}",
            value_type(&value)
        ))),
    }
}

fn evaluate_date_add_column(
    unit: DateTruncUnit,
    value: &Expr,
    amount: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let values = evaluate_selected(value, chunk, columns, rows)?;
    let amounts = evaluate_selected(amount, chunk, columns, rows)?;
    values
        .into_iter()
        .zip(amounts)
        .map(|(value, amount)| evaluate_date_add(unit, value, amount))
        .collect()
}

fn evaluate_date_add(unit: DateTruncUnit, value: Value, amount: Value) -> DevonResult<Value> {
    const MICROS_PER_DAY: i64 = 86_400_000_000;

    match (unit, value, amount) {
        (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
        (DateTruncUnit::Day, Value::Timestamp(micros), Value::Int64(days)) => {
            let offset = days.checked_mul(MICROS_PER_DAY).ok_or_else(|| {
                invalid_argument(format!(
                    "operator `date_add(day)` overflowed while multiplying {days} days by {MICROS_PER_DAY} microseconds"
                ))
            })?;
            micros.checked_add(offset).map(Value::Timestamp).ok_or_else(|| {
                invalid_argument(format!(
                    "operator `date_add(day)` overflowed while adding offset {offset} to Timestamp {micros}"
                ))
            })
        }
        (DateTruncUnit::Day, value, amount) => Err(invalid_argument(format!(
            "operator `date_add(day)` requires Timestamp and Int64 operands; got {} and {}",
            value_type(&value),
            value_type(&amount)
        ))),
    }
}

fn evaluate_round_column(
    value: &Expr,
    places: u8,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let values = evaluate_selected(value, chunk, columns, rows)?;
    let precision = decimal_precision(value, chunk, columns, "round")?;
    values
        .into_iter()
        .map(|value| evaluate_round(value, places, precision))
        .collect()
}

fn evaluate_round(value: Value, places: u8, precision: Option<u8>) -> DevonResult<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Decimal(value) => value
            .round_to_places(places, require_decimal_precision("round", precision)?)
            .map(Value::Decimal),
        value => Err(invalid_argument(format!(
            "operator `round` requires a Decimal operand; got {}",
            value_type(&value)
        ))),
    }
}

fn evaluate_round_div_column(
    numerator: &Expr,
    denominator: &Expr,
    places: u8,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    rows: &[usize],
) -> DevonResult<Vec<Value>> {
    let numerators = evaluate_selected(numerator, chunk, columns, rows)?;
    let denominators = evaluate_selected(denominator, chunk, columns, rows)?;
    let precision = decimal_precision(numerator, chunk, columns, "round_div numerator")?.or(
        decimal_precision(denominator, chunk, columns, "round_div denominator")?,
    );
    numerators
        .into_iter()
        .zip(denominators)
        .map(|(numerator, denominator)| {
            evaluate_round_div(numerator, denominator, places, precision)
        })
        .collect()
}

fn evaluate_round_div(
    numerator: Value,
    denominator: Value,
    places: u8,
    precision: Option<u8>,
) -> DevonResult<Value> {
    match (numerator, denominator) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Decimal(numerator), Value::Decimal(denominator)) => numerator
            .round_div_to_places(
                denominator,
                places,
                require_decimal_precision("round_div", precision)?,
            )
            .map(Value::Decimal),
        (numerator, denominator) => Err(invalid_argument(format!(
            "operator `round_div` requires Decimal operands; got {} and {}",
            value_type(&numerator),
            value_type(&denominator)
        ))),
    }
}

fn decimal_precision(
    expression: &Expr,
    chunk: &Chunk,
    columns: &HashMap<String, usize>,
    operator: &str,
) -> DevonResult<Option<u8>> {
    let mut types = HashMap::with_capacity(columns.len());
    for (reference, index) in columns {
        if let Some(ty) = chunk.types().get(*index) {
            types.insert(reference.clone(), *ty);
        }
    }
    match expression_type(expression, &types)? {
        ExpressionType::Null => Ok(None),
        ExpressionType::Value(LogicalType::Decimal { precision, .. }) => Ok(Some(precision)),
        ExpressionType::Value(ty) => Err(invalid_argument(format!(
            "operator `{operator}` requires a Decimal operand; got {ty}"
        ))),
    }
}

fn require_decimal_precision(operator: &str, precision: Option<u8>) -> DevonResult<u8> {
    precision.ok_or_else(|| {
        invalid_argument(format!(
            "operator `{operator}` produced a Decimal without a declared precision"
        ))
    })
}

fn invalid_if_condition(value: &Value) -> DevonError {
    invalid_argument(format!(
        "operator `if` requires a Bool or Null condition; got {}",
        value_type(value)
    ))
}

fn evaluate_binary(op: BinaryOp, left: &Value, right: &Value) -> DevonResult<Value> {
    match op {
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            evaluate_comparison(op, left, right)
        }
        BinaryOp::And => evaluate_and(left, right),
        BinaryOp::Or => evaluate_or(left, right),
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
            evaluate_arithmetic(op, left, right)
        }
    }
}

fn evaluate_comparison(op: BinaryOp, left: &Value, right: &Value) -> DevonResult<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }

    let result = match (left, right) {
        (Value::Bool(left), Value::Bool(right)) => compare_values(op, *left, *right),
        (Value::Int64(left), Value::Int64(right)) => compare_values(op, *left, *right),
        (Value::Float64(left), Value::Float64(right)) => compare_values(op, *left, *right),
        (Value::String(left), Value::String(right)) => {
            compare_values(op, left.as_str(), right.as_str())
        }
        (Value::Int64(left), Value::Float64(right)) => compare_int_float(op, *left, *right),
        (Value::Float64(left), Value::Int64(right)) => compare_float_int(op, *left, *right),
        (Value::Timestamp(left), Value::Timestamp(right)) => compare_values(op, *left, *right),
        (Value::Decimal(left), Value::Decimal(right)) => {
            if left.scale() != right.scale() {
                return Err(invalid_decimal_scales(op, left.scale(), right.scale()));
            }
            compare_values(op, left.digits(), right.digits())
        }
        (Value::Bytes(left), Value::Bytes(right)) if is_equality_comparison(op) => {
            compare_equality(op, left.as_slice(), right.as_slice())
        }
        (Value::Json(left), Value::Json(right)) if is_equality_comparison(op) => {
            compare_equality(op, left.as_str(), right.as_str())
        }
        _ => return Err(invalid_binary_types(op, left, right)),
    };
    Ok(Value::Bool(result))
}

fn compare_values<T: PartialEq + PartialOrd>(op: BinaryOp, left: T, right: T) -> bool {
    match op {
        BinaryOp::Eq => left == right,
        BinaryOp::Ne => left != right,
        BinaryOp::Lt => left < right,
        BinaryOp::Le => left <= right,
        BinaryOp::Gt => left > right,
        BinaryOp::Ge => left >= right,
        _ => unreachable!("comparison helper received a non-comparison operator"),
    }
}

/// Exact Int64 ∘ Float64 comparison with no lossy `as f64` conversion
/// (PLAN_IR.md expression typing allows the cross-comparison but pins no
/// implicit promotion; the expression spec forbids rounding the Int64). NaN
/// follows the same IEEE partial order as Float64 ∘ Float64: unordered, so
/// every operator but `!=` is false.
fn compare_int_float(op: BinaryOp, integer: i64, float: f64) -> bool {
    if float.is_nan() {
        return nan_comparison(op);
    }
    compare_ordering(op, int_float_ordering(integer, float))
}

/// The operand-reversed mirror of [`compare_int_float`]; NaN stays
/// unordered rather than mirrored.
fn compare_float_int(op: BinaryOp, float: f64, integer: i64) -> bool {
    if float.is_nan() {
        return nan_comparison(op);
    }
    compare_ordering(op, int_float_ordering(integer, float).reverse())
}

fn nan_comparison(op: BinaryOp) -> bool {
    match op {
        BinaryOp::Ne => true,
        BinaryOp::Eq | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => false,
        _ => unreachable!("comparison helper received a non-comparison operator"),
    }
}

fn int_float_ordering(integer: i64, float: f64) -> Ordering {
    if float.is_infinite() {
        return if float.is_sign_negative() {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    let truncated = float.trunc();
    if truncated >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    if truncated < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    integer
        .cmp(&(truncated as i64))
        .then_with(|| 0.0_f64.total_cmp(&(float - truncated)))
}

fn compare_ordering(op: BinaryOp, ordering: Ordering) -> bool {
    match op {
        BinaryOp::Eq => ordering == Ordering::Equal,
        BinaryOp::Ne => ordering != Ordering::Equal,
        BinaryOp::Lt => ordering == Ordering::Less,
        BinaryOp::Le => ordering != Ordering::Greater,
        BinaryOp::Gt => ordering == Ordering::Greater,
        BinaryOp::Ge => ordering != Ordering::Less,
        _ => unreachable!("comparison helper received a non-comparison operator"),
    }
}

fn compare_equality<T: PartialEq + ?Sized>(op: BinaryOp, left: &T, right: &T) -> bool {
    match op {
        BinaryOp::Eq => left == right,
        BinaryOp::Ne => left != right,
        _ => unreachable!("equality helper received an ordering operator"),
    }
}

const fn is_equality_comparison(op: BinaryOp) -> bool {
    matches!(op, BinaryOp::Eq | BinaryOp::Ne)
}

fn evaluate_and(left: &Value, right: &Value) -> DevonResult<Value> {
    match (left, right) {
        (Value::Bool(left), Value::Bool(right)) => Ok(Value::Bool(*left && *right)),
        (Value::Bool(false), Value::Null) | (Value::Null, Value::Bool(false)) => {
            Ok(Value::Bool(false))
        }
        (Value::Bool(true), Value::Null)
        | (Value::Null, Value::Bool(true))
        | (Value::Null, Value::Null) => Ok(Value::Null),
        _ => Err(invalid_binary_types(BinaryOp::And, left, right)),
    }
}

fn evaluate_or(left: &Value, right: &Value) -> DevonResult<Value> {
    match (left, right) {
        (Value::Bool(left), Value::Bool(right)) => Ok(Value::Bool(*left || *right)),
        (Value::Bool(true), Value::Null) | (Value::Null, Value::Bool(true)) => {
            Ok(Value::Bool(true))
        }
        (Value::Bool(false), Value::Null)
        | (Value::Null, Value::Bool(false))
        | (Value::Null, Value::Null) => Ok(Value::Null),
        _ => Err(invalid_binary_types(BinaryOp::Or, left, right)),
    }
}

fn evaluate_not(value: &Value) -> DevonResult<Value> {
    match value {
        Value::Bool(value) => Ok(Value::Bool(!value)),
        Value::Null => Ok(Value::Null),
        _ => Err(invalid_argument(format!(
            "operator `not` does not accept operand type {} (value {value})",
            value_type(value)
        ))),
    }
}

fn evaluate_arithmetic(op: BinaryOp, left: &Value, right: &Value) -> DevonResult<Value> {
    // NULL propagates out of arithmetic — Int64 established this rule and
    // the Decimal cases below ride the same guard.
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }

    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => integer_arithmetic(op, *left, *right),
        (Value::Int64(left), Value::Float64(right)) => {
            Ok(Value::Float64(float_arithmetic(op, *left as f64, *right)))
        }
        (Value::Float64(left), Value::Int64(right)) => {
            Ok(Value::Float64(float_arithmetic(op, *left, *right as f64)))
        }
        (Value::Float64(left), Value::Float64(right)) => {
            Ok(Value::Float64(float_arithmetic(op, *left, *right)))
        }
        (Value::Decimal(left), Value::Decimal(right)) => decimal_arithmetic(op, *left, *right),
        (Value::Decimal(decimal), Value::Int64(integer)) => {
            decimal_int64_arithmetic(op, *decimal, *integer, true)
        }
        (Value::Int64(integer), Value::Decimal(decimal)) => {
            decimal_int64_arithmetic(op, *decimal, *integer, false)
        }
        // Float64 ∘ Decimal falls through to a loud type error: mixing
        // would round (refuse-never-round law).
        _ => Err(invalid_binary_types(op, left, right)),
    }
}

/// Evaluates `Decimal ∘ Decimal` arithmetic exactly.
/// Typing caps every result precision at [`MAX_PRECISION`], so an overflowed
/// result always belongs to a declared `Decimal(38, scale)` — the error
/// names that type.
fn decimal_arithmetic(op: BinaryOp, left: Decimal128, right: Decimal128) -> DevonResult<Value> {
    let result = match op {
        BinaryOp::Add => {
            require_equal_decimal_scales(op, left, right)?;
            left.checked_add_same_scale(right)
                .map_err(|_| decimal_overflow_error(op, left.scale()))
        }
        BinaryOp::Sub => {
            require_equal_decimal_scales(op, left, right)?;
            left.checked_sub_same_scale(right)
                .map_err(|_| decimal_overflow_error(op, left.scale()))
        }
        BinaryOp::Mul => decimal_product(op, left, right),
        BinaryOp::Div => return Err(decimal_division_refused()),
        _ => unreachable!("decimal arithmetic received a non-arithmetic operator"),
    };
    result.map(Value::Decimal)
}

fn decimal_product(op: BinaryOp, left: Decimal128, right: Decimal128) -> DevonResult<Decimal128> {
    let scale = left.scale() + right.scale();
    if scale > MAX_PRECISION {
        return Err(invalid_argument(format!(
            "operator `mul` would produce Decimal scale {scale} above the maximum {MAX_PRECISION}; got scales {} and {}",
            left.scale(),
            right.scale()
        )));
    }
    left.checked_mul(right)
        .map_err(|_| decimal_overflow_error(op, scale))
}

/// Evaluates `Decimal ∘ Int64`: the Int64 operand promotes exactly to
/// `Decimal(19, 0)` (every i64 fits) and, for `+`/`-`, is then scaled by
/// 10^s to the Decimal operand's scale — overflow-checked, because scales
/// must be equal. `decimal_first` preserves the operand order for `-`.
fn decimal_int64_arithmetic(
    op: BinaryOp,
    decimal: Decimal128,
    integer: i64,
    decimal_first: bool,
) -> DevonResult<Value> {
    if op == BinaryOp::Div {
        return Err(decimal_division_refused());
    }
    let scale = if op == BinaryOp::Mul {
        0
    } else {
        decimal.scale()
    };
    let promoted = Decimal128::from_i64_scaled(integer, scale).map_err(|_| {
        invalid_argument(format!(
            "operator `{}` cannot promote Int64 operand {integer} to Decimal scale {scale} exactly; the 10^{scale} scaling overflowed",
            binary_op_name(op)
        ))
    })?;
    let (left, right) = if decimal_first {
        (decimal, promoted)
    } else {
        (promoted, decimal)
    };
    decimal_arithmetic(op, left, right)
}

fn require_equal_decimal_scales(
    op: BinaryOp,
    left: Decimal128,
    right: Decimal128,
) -> DevonResult<()> {
    if left.scale() == right.scale() {
        return Ok(());
    }
    Err(invalid_decimal_scales(op, left.scale(), right.scale()))
}

fn decimal_overflow_error(op: BinaryOp, scale: u8) -> DevonError {
    invalid_argument(format!(
        "operator `{}` overflowed Decimal({MAX_PRECISION}, {scale})",
        binary_op_name(op)
    ))
}

fn decimal_division_refused() -> DevonError {
    invalid_argument(
        "div over Decimal would round; use round_div(numerator, denominator, places) to name the rounding explicitly",
    )
}

fn integer_arithmetic(op: BinaryOp, left: i64, right: i64) -> DevonResult<Value> {
    if op == BinaryOp::Div && right == 0 {
        return Err(invalid_argument(format!(
            "operator `div` failed for Int64 values {left} and {right}: division by zero"
        )));
    }

    let result = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Sub => left.checked_sub(right),
        BinaryOp::Mul => left.checked_mul(right),
        BinaryOp::Div => left.checked_div(right),
        _ => unreachable!("integer arithmetic received a non-arithmetic operator"),
    };
    result.map(Value::Int64).ok_or_else(|| {
        invalid_argument(format!(
            "operator `{}` overflowed for Int64 values {left} and {right}",
            binary_op_name(op)
        ))
    })
}

fn float_arithmetic(op: BinaryOp, left: f64, right: f64) -> f64 {
    match op {
        BinaryOp::Add => left + right,
        BinaryOp::Sub => left - right,
        BinaryOp::Mul => left * right,
        BinaryOp::Div => left / right,
        _ => unreachable!("float arithmetic received a non-arithmetic operator"),
    }
}

fn evaluate_distance(metric: Metric, left: &Value, right: &Value) -> DevonResult<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }

    let (Value::Vector(left), Value::Vector(right)) = (left, right) else {
        return Err(invalid_distance_types(metric, left, right));
    };
    if left.len() != right.len() {
        return Err(invalid_argument(format!(
            "operator `distance({})` requires equal-length Vector operands; got Vector({}) and Vector({})",
            metric_name(metric),
            left.len(),
            right.len()
        )));
    }

    match metric {
        Metric::L2 => Ok(Value::Float64(l2_distance(left, right))),
        Metric::Cosine => cosine_distance(left, right).map(Value::Float64),
    }
}

fn l2_distance(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            let difference = f64::from(*left) - f64::from(*right);
            difference * difference
        })
        .sum::<f64>()
        .sqrt()
}

fn cosine_distance(left: &[f32], right: &[f32]) -> DevonResult<f64> {
    let (dot, left_magnitude, right_magnitude) = left.iter().zip(right).fold(
        (0.0, 0.0, 0.0),
        |(dot, left_magnitude, right_magnitude), (left, right)| {
            let left = f64::from(*left);
            let right = f64::from(*right);
            (
                dot + left * right,
                left_magnitude + left * left,
                right_magnitude + right * right,
            )
        },
    );
    if left_magnitude == 0.0 || right_magnitude == 0.0 {
        return Err(invalid_argument(
            "operator `distance(cosine)` received a zero-magnitude Vector operand",
        ));
    }
    Ok(1.0 - dot / (left_magnitude.sqrt() * right_magnitude.sqrt()))
}

fn invalid_binary_types(op: BinaryOp, left: &Value, right: &Value) -> DevonError {
    invalid_argument(format!(
        "operator `{}` does not accept operand types {} and {}",
        binary_op_name(op),
        value_type(left),
        value_type(right)
    ))
}

fn invalid_decimal_scales(op: BinaryOp, left: u8, right: u8) -> DevonError {
    invalid_argument(format!(
        "operator `{}` requires equal Decimal scales; got scales {left} and {right}",
        binary_op_name(op)
    ))
}

fn invalid_distance_types(metric: Metric, left: &Value, right: &Value) -> DevonError {
    invalid_argument(format!(
        "operator `distance({})` requires equal-length Vector operands; got {} and {}",
        metric_name(metric),
        value_type(left),
        value_type(right)
    ))
}

fn value_type(value: &Value) -> String {
    match value {
        Value::Null => "Null".to_owned(),
        Value::Bool(_) => "Bool".to_owned(),
        Value::Int64(_) => "Int64".to_owned(),
        Value::Float64(_) => "Float64".to_owned(),
        Value::String(_) => "String".to_owned(),
        Value::Vector(values) => format!("Vector({})", values.len()),
        Value::GeoPoint(_) => "GeoPoint".to_owned(),
        Value::Timestamp(_) => "Timestamp".to_owned(),
        Value::Bytes(_) => "Bytes".to_owned(),
        Value::Decimal(value) => format!("Decimal({}, {})", value.precision(), value.scale()),
        Value::Json(_) => "Json".to_owned(),
    }
}

fn binary_op_name(op: BinaryOp) -> &'static str {
    match op {
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

const fn metric_name(metric: Metric) -> &'static str {
    match metric {
        Metric::Cosine => "cosine",
        Metric::L2 => "l2",
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

    use devondb_plan::expr::{BinaryOp, DateTruncUnit, Expr, Metric};
    use devondb_types::{DevonError, decimal::Decimal128, logical_type::LogicalType, value::Value};

    use super::{classof_column_key, evaluate};
    use crate::chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder};

    fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn distance(metric: Metric, left: Value, right: Value) -> Expr {
        Expr::Distance {
            left: Box::new(Expr::Lit(left)),
            right: Box::new(Expr::Lit(right)),
            metric,
        }
    }

    fn if_expr(cond: Expr, then_expr: Expr, else_expr: Expr) -> Expr {
        Expr::If {
            cond: Box::new(cond),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
        }
    }

    fn date_trunc(value: Expr) -> Expr {
        Expr::DateTrunc {
            unit: DateTruncUnit::Day,
            value: Box::new(value),
        }
    }

    fn date_add(value: Expr, amount: Expr) -> Expr {
        Expr::DateAdd {
            unit: DateTruncUnit::Day,
            value: Box::new(value),
            amount: Box::new(amount),
        }
    }

    fn round(value: Expr, places: u8) -> Expr {
        Expr::Round {
            value: Box::new(value),
            places,
        }
    }

    fn round_div(numerator: Expr, denominator: Expr, places: u8) -> Expr {
        Expr::RoundDiv {
            numerator: Box::new(numerator),
            denominator: Box::new(denominator),
            places,
        }
    }

    fn empty_chunk(row_count: usize) -> Chunk {
        let mut builder = ChunkBuilder::new(Vec::new());
        for _ in 0..row_count {
            builder.push_row(Vec::new()).unwrap();
        }
        builder.finish()
    }

    fn single_column(logical_type: LogicalType, values: Vec<Value>) -> Chunk {
        let mut builder = ChunkBuilder::new(vec![logical_type]);
        for value in values {
            builder.push_row(vec![value]).unwrap();
        }
        builder.finish()
    }

    fn column_map() -> HashMap<String, usize> {
        HashMap::from([("p.value".to_owned(), 0)])
    }

    #[test]
    fn scan_interface_classof_reads_the_private_discriminator_column() {
        let chunk = single_column(
            LogicalType::String,
            vec![
                Value::String("Person".into()),
                Value::String("Project".into()),
            ],
        );
        let columns = HashMap::from([(classof_column_key("ENTITY"), 0)]);

        assert_eq!(
            evaluate(&Expr::ClassOf("entity".into()), &chunk, &columns).unwrap(),
            vec![
                Value::String("Person".into()),
                Value::String("Project".into())
            ]
        );
    }

    fn decimal(digits: i128, scale: u8) -> Value {
        Value::Decimal(Decimal128::new(digits, scale).unwrap())
    }

    fn invalid_context(result: Result<Vec<Value>, DevonError>) -> String {
        let DevonError::InvalidArgument { context } = result.unwrap_err() else {
            panic!("expected InvalidArgument");
        };
        context
    }

    #[test]
    fn evaluates_every_int_float_comparison_exactly() {
        let chunk = single_column(
            LogicalType::Int64,
            vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)],
        );
        let cases = [
            (BinaryOp::Eq, [false, true, false]),
            (BinaryOp::Ne, [true, false, true]),
            (BinaryOp::Lt, [true, false, false]),
            (BinaryOp::Le, [true, true, false]),
            (BinaryOp::Gt, [false, false, true]),
            (BinaryOp::Ge, [false, true, true]),
        ];

        for (op, expected) in cases {
            let expr = binary(
                op,
                Expr::Col("p.value".into()),
                Expr::Lit(Value::Float64(2.0)),
            );
            let actual = evaluate(&expr, &chunk, &column_map()).unwrap();
            assert_eq!(actual, expected.map(Value::Bool), "operator {op:?}");
        }
    }

    #[test]
    fn comparisons_use_string_bool_and_float_ordering() {
        let strings = single_column(
            LogicalType::String,
            vec!["apple", "zebra", "éclair"]
                .into_iter()
                .map(|value| Value::String(value.into()))
                .collect(),
        );
        let string_lt = binary(
            BinaryOp::Lt,
            Expr::Col("p.value".into()),
            Expr::Lit(Value::String("zoo".into())),
        );
        assert_eq!(
            evaluate(&string_lt, &strings, &column_map()).unwrap(),
            vec![Value::Bool(true), Value::Bool(true), Value::Bool(false)]
        );

        let bools = single_column(
            LogicalType::Bool,
            vec![Value::Bool(false), Value::Bool(true)],
        );
        let bool_lt = binary(
            BinaryOp::Lt,
            Expr::Col("p.value".into()),
            Expr::Lit(Value::Bool(true)),
        );
        assert_eq!(
            evaluate(&bool_lt, &bools, &column_map()).unwrap(),
            vec![Value::Bool(true), Value::Bool(false)]
        );

        let floats = single_column(
            LogicalType::Float64,
            vec![Value::Float64(1.5), Value::Float64(2.5)],
        );
        let float_ge = binary(
            BinaryOp::Ge,
            Expr::Col("p.value".into()),
            Expr::Lit(Value::Float64(2.0)),
        );
        assert_eq!(
            evaluate(&float_ge, &floats, &column_map()).unwrap(),
            vec![Value::Bool(false), Value::Bool(true)]
        );
    }

    #[test]
    fn timestamp_and_equal_scale_decimal_support_every_comparison() {
        let chunk = empty_chunk(1);
        let cases = [
            (BinaryOp::Eq, false),
            (BinaryOp::Ne, true),
            (BinaryOp::Lt, true),
            (BinaryOp::Le, true),
            (BinaryOp::Gt, false),
            (BinaryOp::Ge, false),
        ];

        for (op, expected) in cases {
            for (left, right) in [
                (Value::Timestamp(-1), Value::Timestamp(0)),
                (decimal(-250, 2), decimal(-125, 2)),
            ] {
                let expr = binary(op, Expr::Lit(left), Expr::Lit(right));
                assert_eq!(
                    evaluate(&expr, &chunk, &HashMap::new()).unwrap(),
                    vec![Value::Bool(expected)],
                    "operator {op:?}"
                );
            }
        }
    }

    #[test]
    fn bytes_and_json_support_exact_equality_and_inequality_only() {
        let chunk = empty_chunk(1);
        let cases = [
            (
                BinaryOp::Eq,
                Value::Bytes(Vec::new()),
                Value::Bytes(Vec::new()),
                true,
            ),
            (
                BinaryOp::Ne,
                Value::Bytes(Vec::new()),
                Value::Bytes(vec![0]),
                true,
            ),
            (
                BinaryOp::Eq,
                Value::Json(r#"{"a":1}"#.into()),
                Value::Json(r#"{"a":1}"#.into()),
                true,
            ),
            (
                BinaryOp::Ne,
                Value::Json(r#"{"a":1}"#.into()),
                Value::Json(r#"{"a":2}"#.into()),
                true,
            ),
        ];

        for (op, left, right, expected) in cases {
            let expr = binary(op, Expr::Lit(left), Expr::Lit(right));
            assert_eq!(
                evaluate(&expr, &chunk, &HashMap::new()).unwrap(),
                vec![Value::Bool(expected)]
            );
        }
    }

    #[test]
    fn scalar_v2_comparison_rejections_name_the_incompatible_types_or_scales() {
        let chunk = empty_chunk(1);
        let cases = [
            (
                binary(
                    BinaryOp::Lt,
                    Expr::Lit(decimal(100, 2)),
                    Expr::Lit(decimal(100, 3)),
                ),
                "operator `lt` requires equal Decimal scales; got scales 2 and 3",
            ),
            (
                binary(
                    BinaryOp::Eq,
                    Expr::Lit(Value::Timestamp(0)),
                    Expr::Lit(Value::Int64(0)),
                ),
                "operator `eq` does not accept operand types Timestamp and Int64",
            ),
            (
                binary(
                    BinaryOp::Lt,
                    Expr::Lit(Value::Bytes(Vec::new())),
                    Expr::Lit(Value::Bytes(vec![0])),
                ),
                "operator `lt` does not accept operand types Bytes and Bytes",
            ),
        ];

        for (expr, expected) in cases {
            assert_eq!(
                invalid_context(evaluate(&expr, &chunk, &HashMap::new())),
                expected
            );
        }
    }

    #[test]
    fn scalar_v2_comparisons_propagate_null_for_every_admitted_operator() {
        let chunk = empty_chunk(1);
        let ordered = [Value::Timestamp(-1), decimal(-125, 2)];
        for operator in [
            BinaryOp::Eq,
            BinaryOp::Ne,
            BinaryOp::Lt,
            BinaryOp::Le,
            BinaryOp::Gt,
            BinaryOp::Ge,
        ] {
            for value in &ordered {
                for (left, right) in [(Value::Null, value.clone()), (value.clone(), Value::Null)] {
                    let expression = binary(operator, Expr::Lit(left), Expr::Lit(right));
                    assert_eq!(
                        evaluate(&expression, &chunk, &HashMap::new()).unwrap(),
                        vec![Value::Null]
                    );
                }
            }
        }

        for operator in [BinaryOp::Eq, BinaryOp::Ne] {
            for value in [Value::Bytes(vec![0]), Value::Json(r#"{"a":1}"#.into())] {
                let expression = binary(operator, Expr::Lit(Value::Null), Expr::Lit(value));
                assert_eq!(
                    evaluate(&expression, &chunk, &HashMap::new()).unwrap(),
                    vec![Value::Null]
                );
            }
        }
    }

    #[test]
    fn and_or_follow_complete_kleene_truth_tables() {
        let inputs = [Value::Bool(true), Value::Bool(false), Value::Null];
        let mut builder = ChunkBuilder::new(vec![LogicalType::Bool, LogicalType::Bool]);
        for left in &inputs {
            for right in &inputs {
                builder.push_row(vec![left.clone(), right.clone()]).unwrap();
            }
        }
        let chunk = builder.finish();
        let columns = HashMap::from([("p.left".into(), 0), ("p.right".into(), 1)]);
        let operands = || (Expr::Col("p.left".into()), Expr::Col("p.right".into()));
        let (left, right) = operands();
        let and = evaluate(&binary(BinaryOp::And, left, right), &chunk, &columns).unwrap();
        let (left, right) = operands();
        let or = evaluate(&binary(BinaryOp::Or, left, right), &chunk, &columns).unwrap();

        assert_eq!(
            and,
            vec![
                Value::Bool(true),
                Value::Bool(false),
                Value::Null,
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(false),
                Value::Null,
                Value::Bool(false),
                Value::Null,
            ]
        );
        assert_eq!(
            or,
            vec![
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(false),
                Value::Null,
                Value::Bool(true),
                Value::Null,
                Value::Null,
            ]
        );
    }

    #[test]
    fn not_handles_true_false_and_null() {
        let chunk = single_column(
            LogicalType::Bool,
            vec![Value::Bool(true), Value::Bool(false), Value::Null],
        );
        let expr = Expr::Not(Box::new(Expr::Col("p.value".into())));

        assert_eq!(
            evaluate(&expr, &chunk, &column_map()).unwrap(),
            vec![Value::Bool(false), Value::Bool(true), Value::Null]
        );
    }

    #[test]
    fn null_propagates_through_nested_arithmetic_and_comparison() {
        let chunk = single_column(
            LogicalType::Int64,
            vec![Value::Int64(1), Value::Null, Value::Int64(-1)],
        );
        let add_null = binary(
            BinaryOp::Add,
            Expr::Col("p.value".into()),
            Expr::Lit(Value::Null),
        );
        let expr = binary(BinaryOp::Gt, add_null, Expr::Lit(Value::Int64(0)));

        assert_eq!(
            evaluate(&expr, &chunk, &column_map()).unwrap(),
            vec![Value::Null; 3]
        );
    }

    #[test]
    fn arithmetic_is_checked_and_promotes_only_to_float() {
        let chunk = single_column(LogicalType::Int64, vec![Value::Int64(-7), Value::Int64(4)]);
        let cases = [
            (
                BinaryOp::Add,
                Value::Int64(2),
                vec![Value::Int64(-5), Value::Int64(6)],
            ),
            (
                BinaryOp::Sub,
                Value::Int64(2),
                vec![Value::Int64(-9), Value::Int64(2)],
            ),
            (
                BinaryOp::Mul,
                Value::Int64(2),
                vec![Value::Int64(-14), Value::Int64(8)],
            ),
            (
                BinaryOp::Div,
                Value::Int64(2),
                vec![Value::Int64(-3), Value::Int64(2)],
            ),
            (
                BinaryOp::Add,
                Value::Float64(0.5),
                vec![Value::Float64(-6.5), Value::Float64(4.5)],
            ),
        ];

        for (op, right, expected) in cases {
            let expr = binary(op, Expr::Col("p.value".into()), Expr::Lit(right));
            assert_eq!(evaluate(&expr, &chunk, &column_map()).unwrap(), expected);
        }

        let overflow = binary(
            BinaryOp::Add,
            Expr::Lit(Value::Int64(i64::MAX)),
            Expr::Lit(Value::Int64(1)),
        );
        let context = invalid_context(evaluate(&overflow, &empty_chunk(1), &HashMap::new()));
        assert!(context.contains("add") && context.contains("overflow"));
    }

    #[test]
    fn integer_division_by_zero_errors_but_float_division_is_infinite() {
        let integer = binary(
            BinaryOp::Div,
            Expr::Lit(Value::Int64(7)),
            Expr::Lit(Value::Int64(0)),
        );
        let chunk = empty_chunk(1);
        let context = invalid_context(evaluate(&integer, &chunk, &HashMap::new()));
        assert!(context.contains("div") && context.contains("zero"));

        let float = binary(
            BinaryOp::Div,
            Expr::Lit(Value::Float64(7.0)),
            Expr::Lit(Value::Float64(0.0)),
        );
        assert_eq!(
            evaluate(&float, &chunk, &HashMap::new()).unwrap(),
            vec![Value::Float64(f64::INFINITY)]
        );
    }

    #[test]
    fn computes_l2_and_cosine_distance_in_f64() {
        let chunk = empty_chunk(1);
        let l2 = distance(
            Metric::L2,
            Value::Vector(vec![0.0, 3.0]),
            Value::Vector(vec![4.0, 0.0]),
        );
        let cosine = distance(
            Metric::Cosine,
            Value::Vector(vec![1.0, 0.0]),
            Value::Vector(vec![0.0, 1.0]),
        );

        assert_eq!(
            evaluate(&l2, &chunk, &HashMap::new()).unwrap(),
            vec![Value::Float64(5.0)]
        );
        assert_eq!(
            evaluate(&cosine, &chunk, &HashMap::new()).unwrap(),
            vec![Value::Float64(1.0)]
        );
    }

    #[test]
    fn distance_rejects_dimension_mismatch_and_zero_cosine_vector() {
        let chunk = empty_chunk(1);
        let mismatch = distance(
            Metric::L2,
            Value::Vector(vec![1.0, 2.0]),
            Value::Vector(vec![1.0]),
        );
        let context = invalid_context(evaluate(&mismatch, &chunk, &HashMap::new()));
        assert!(context.contains("distance(l2)"));
        assert!(context.contains("Vector(2)") && context.contains("Vector(1)"));

        let zero = distance(
            Metric::Cosine,
            Value::Vector(vec![0.0, 0.0]),
            Value::Vector(vec![1.0, 0.0]),
        );
        let context = invalid_context(evaluate(&zero, &chunk, &HashMap::new()));
        assert!(context.contains("distance(cosine)") && context.contains("zero-magnitude"));
    }

    #[test]
    fn type_errors_name_operator_and_operand_types() {
        let cases = [
            (
                binary(
                    BinaryOp::Eq,
                    Expr::Lit(Value::Vector(vec![1.0])),
                    Expr::Lit(Value::Vector(vec![1.0])),
                ),
                ["eq", "Vector"],
            ),
            (
                binary(
                    BinaryOp::Add,
                    Expr::Lit(Value::String("a".into())),
                    Expr::Lit(Value::String("b".into())),
                ),
                ["add", "String"],
            ),
            (
                binary(
                    BinaryOp::And,
                    Expr::Lit(Value::Int64(1)),
                    Expr::Lit(Value::Bool(true)),
                ),
                ["and", "Int64"],
            ),
        ];
        let chunk = empty_chunk(1);

        for (expr, fragments) in cases {
            let context = invalid_context(evaluate(&expr, &chunk, &HashMap::new()));
            for fragment in fragments {
                assert!(
                    context.contains(fragment),
                    "missing {fragment:?} in {context:?}"
                );
            }
        }
    }

    #[test]
    fn unknown_and_out_of_range_column_references_name_the_reference() {
        let chunk = single_column(LogicalType::Int64, vec![Value::Int64(1)]);
        let expr = Expr::Col("p.missing".into());
        let context = invalid_context(evaluate(&expr, &chunk, &HashMap::new()));
        assert!(context.contains("p.missing"));

        let columns = HashMap::from([("p.missing".into(), 2)]);
        let context = invalid_context(evaluate(&expr, &chunk, &columns));
        assert!(context.contains("p.missing") && context.contains("out-of-range"));
    }

    #[test]
    fn literal_replication_matches_row_count() {
        let chunk = single_column(
            LogicalType::Int64,
            vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)],
        );
        let values = evaluate(
            &Expr::Lit(Value::String("same".into())),
            &chunk,
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(values.len(), chunk.row_count());
        assert_eq!(values, vec![Value::String("same".into()); 3]);
    }

    #[test]
    fn if_uses_sql_case_null_semantics_and_preserves_selected_nulls() {
        let chunk = single_column(
            LogicalType::Bool,
            vec![Value::Bool(true), Value::Bool(false), Value::Null],
        );
        let condition = || Expr::Col("p.value".into());
        let null_then = if_expr(
            condition(),
            Expr::Lit(Value::Null),
            Expr::Lit(Value::Int64(7)),
        );
        let null_else = if_expr(
            condition(),
            Expr::Lit(Value::Int64(7)),
            Expr::Lit(Value::Null),
        );

        assert_eq!(
            evaluate(&null_then, &chunk, &column_map()).unwrap(),
            vec![Value::Null, Value::Int64(7), Value::Int64(7)]
        );
        assert_eq!(
            evaluate(&null_else, &chunk, &column_map()).unwrap(),
            vec![Value::Int64(7), Value::Null, Value::Null]
        );
    }

    #[test]
    fn if_true_rows_sever_unselected_else_division_errors() {
        let chunk = single_column(
            LogicalType::Bool,
            vec![Value::Bool(true), Value::Bool(true)],
        );
        let division_by_zero = binary(
            BinaryOp::Div,
            Expr::Lit(Value::Int64(1)),
            Expr::Lit(Value::Int64(0)),
        );
        let expr = if_expr(
            Expr::Col("p.value".into()),
            Expr::Lit(Value::Int64(11)),
            division_by_zero,
        );

        assert_eq!(
            evaluate(&expr, &chunk, &column_map()).unwrap(),
            vec![Value::Int64(11), Value::Int64(11)]
        );
    }

    #[test]
    fn if_false_and_null_rows_sever_unselected_then_division_errors() {
        let chunk = single_column(LogicalType::Bool, vec![Value::Bool(false), Value::Null]);
        let division_by_zero = binary(
            BinaryOp::Div,
            Expr::Lit(Value::Int64(1)),
            Expr::Lit(Value::Int64(0)),
        );
        let expr = if_expr(
            Expr::Col("p.value".into()),
            division_by_zero,
            Expr::Lit(Value::Int64(13)),
        );

        assert_eq!(
            evaluate(&expr, &chunk, &column_map()).unwrap(),
            vec![Value::Int64(13), Value::Int64(13)]
        );
    }

    #[test]
    fn if_laziness_is_row_selective_with_errors_in_both_branches() {
        let mut builder = ChunkBuilder::new(vec![
            LogicalType::Bool,
            LogicalType::Int64,
            LogicalType::Int64,
        ]);
        for row in [
            vec![Value::Bool(true), Value::Int64(2), Value::Int64(0)],
            vec![Value::Bool(false), Value::Int64(0), Value::Int64(4)],
            vec![Value::Null, Value::Int64(0), Value::Int64(5)],
        ] {
            builder.push_row(row).unwrap();
        }
        let chunk = builder.finish();
        let columns = HashMap::from([
            ("p.cond".into(), 0),
            ("p.then_denominator".into(), 1),
            ("p.else_denominator".into(), 2),
        ]);
        let expr = if_expr(
            Expr::Col("p.cond".into()),
            binary(
                BinaryOp::Div,
                Expr::Lit(Value::Int64(20)),
                Expr::Col("p.then_denominator".into()),
            ),
            binary(
                BinaryOp::Div,
                Expr::Lit(Value::Int64(20)),
                Expr::Col("p.else_denominator".into()),
            ),
        );

        assert_eq!(
            evaluate(&expr, &chunk, &columns).unwrap(),
            vec![Value::Int64(10), Value::Int64(5), Value::Int64(4)]
        );
    }

    #[test]
    fn coalesce_skips_nulls_left_to_right_and_returns_null_when_all_are_null() {
        let mut builder = ChunkBuilder::new(vec![LogicalType::Int64, LogicalType::Int64]);
        for row in [
            vec![Value::Null, Value::Null],
            vec![Value::Int64(1), Value::Null],
            vec![Value::Null, Value::Int64(2)],
        ] {
            builder.push_row(row).unwrap();
        }
        let chunk = builder.finish();
        let columns = HashMap::from([("p.first".into(), 0), ("p.second".into(), 1)]);
        let expr = Expr::Coalesce(vec![
            Expr::Col("p.first".into()),
            Expr::Col("p.second".into()),
            Expr::Lit(Value::Null),
        ]);

        assert_eq!(
            evaluate(&expr, &chunk, &columns).unwrap(),
            vec![Value::Null, Value::Int64(1), Value::Int64(2)]
        );
    }

    #[test]
    fn coalesce_laziness_severs_errors_right_of_first_non_null_per_row() {
        let mut builder = ChunkBuilder::new(vec![LogicalType::Int64, LogicalType::Int64]);
        builder
            .push_row(vec![Value::Int64(7), Value::Int64(0)])
            .unwrap();
        builder
            .push_row(vec![Value::Null, Value::Int64(2)])
            .unwrap();
        let chunk = builder.finish();
        let columns = HashMap::from([("p.first".into(), 0), ("p.denominator".into(), 1)]);
        let expr = Expr::Coalesce(vec![
            Expr::Col("p.first".into()),
            binary(
                BinaryOp::Div,
                Expr::Lit(Value::Int64(10)),
                Expr::Col("p.denominator".into()),
            ),
        ]);

        assert_eq!(
            evaluate(&expr, &chunk, &columns).unwrap(),
            vec![Value::Int64(7), Value::Int64(5)]
        );
    }

    #[test]
    fn extrema_skip_nulls_and_return_null_only_for_all_null_rows() {
        let mut builder = ChunkBuilder::new(vec![
            LogicalType::Int64,
            LogicalType::Int64,
            LogicalType::Int64,
        ]);
        for row in [
            vec![Value::Null, Value::Null, Value::Null],
            vec![Value::Null, Value::Int64(3), Value::Int64(1)],
            vec![Value::Int64(2), Value::Null, Value::Int64(2)],
        ] {
            builder.push_row(row).unwrap();
        }
        let chunk = builder.finish();
        let columns = HashMap::from([("p.a".into(), 0), ("p.b".into(), 1), ("p.c".into(), 2)]);
        let operands = || {
            vec![
                Expr::Col("p.a".into()),
                Expr::Col("p.b".into()),
                Expr::Col("p.c".into()),
            ]
        };

        assert_eq!(
            evaluate(&Expr::Least(operands()), &chunk, &columns).unwrap(),
            vec![Value::Null, Value::Int64(1), Value::Int64(2)]
        );
        assert_eq!(
            evaluate(&Expr::Greatest(operands()), &chunk, &columns).unwrap(),
            vec![Value::Null, Value::Int64(3), Value::Int64(2)]
        );
    }

    #[test]
    fn extrema_use_the_pinned_order_for_every_non_decimal_scalar() {
        let chunk = empty_chunk(1);
        let cases = [
            (
                Expr::Least(vec![
                    Expr::Lit(Value::Bool(true)),
                    Expr::Lit(Value::Bool(false)),
                ]),
                Value::Bool(false),
            ),
            (
                Expr::Greatest(vec![
                    Expr::Lit(Value::Int64(-2)),
                    Expr::Lit(Value::Int64(4)),
                ]),
                Value::Int64(4),
            ),
            (
                Expr::Least(vec![
                    Expr::Lit(Value::String("zebra".into())),
                    Expr::Lit(Value::String("apple".into())),
                ]),
                Value::String("apple".into()),
            ),
            (
                Expr::Greatest(vec![
                    Expr::Lit(Value::Timestamp(-1)),
                    Expr::Lit(Value::Timestamp(0)),
                ]),
                Value::Timestamp(0),
            ),
        ];
        for (expr, expected) in cases {
            assert_eq!(
                evaluate(&expr, &chunk, &HashMap::new()).unwrap(),
                vec![expected]
            );
        }

        let signed_zero = Expr::Least(vec![
            Expr::Lit(Value::Float64(0.0)),
            Expr::Lit(Value::Float64(-0.0)),
        ]);
        let Value::Float64(actual) = evaluate(&signed_zero, &chunk, &HashMap::new())
            .unwrap()
            .remove(0)
        else {
            panic!("expected Float64 extremum");
        };
        assert_eq!(actual.to_bits(), (-0.0_f64).to_bits());
    }

    #[test]
    fn decimal_extrema_compare_unscaled_digits_and_keep_leftmost_ties() {
        let chunk = empty_chunk(1);
        let operands = || {
            vec![
                Expr::Lit(decimal(250, 2)),
                Expr::Lit(decimal(-100, 2)),
                Expr::Lit(decimal(250, 2)),
                Expr::Lit(decimal(75, 2)),
            ]
        };

        assert_eq!(
            evaluate(&Expr::Least(operands()), &chunk, &HashMap::new()).unwrap(),
            vec![decimal(-100, 2)]
        );
        assert_eq!(
            evaluate(&Expr::Greatest(operands()), &chunk, &HashMap::new()).unwrap(),
            vec![decimal(250, 2)]
        );
    }

    #[test]
    fn extrema_do_not_short_circuit_later_operand_errors() {
        let division_by_zero = || {
            binary(
                BinaryOp::Div,
                Expr::Lit(Value::Int64(1)),
                Expr::Lit(Value::Int64(0)),
            )
        };
        let chunk = empty_chunk(1);
        for expr in [
            Expr::Least(vec![Expr::Lit(Value::Int64(i64::MIN)), division_by_zero()]),
            Expr::Greatest(vec![Expr::Lit(Value::Int64(i64::MAX)), division_by_zero()]),
        ] {
            let context = invalid_context(evaluate(&expr, &chunk, &HashMap::new()));
            assert!(context.contains("division by zero"), "{context}");
        }
    }

    #[test]
    fn date_trunc_day_propagates_null_and_floors_pre_epoch_to_earlier_midnight() {
        const DAY: i64 = 86_400_000_000;
        let chunk = single_column(
            LogicalType::Timestamp,
            vec![
                Value::Null,
                Value::Timestamp(-1),
                Value::Timestamp(0),
                Value::Timestamp(DAY - 1),
                Value::Timestamp(DAY),
                Value::Timestamp(DAY + 1),
            ],
        );

        assert_eq!(
            evaluate(
                &date_trunc(Expr::Col("p.value".into())),
                &chunk,
                &column_map()
            )
            .unwrap(),
            vec![
                Value::Null,
                Value::Timestamp(-DAY),
                Value::Timestamp(0),
                Value::Timestamp(0),
                Value::Timestamp(DAY),
                Value::Timestamp(DAY),
            ]
        );
    }

    #[test]
    fn date_trunc_day_errors_when_the_floor_is_outside_timestamp_domain() {
        let expr = date_trunc(Expr::Lit(Value::Timestamp(i64::MIN)));
        let context = invalid_context(evaluate(&expr, &empty_chunk(1), &HashMap::new()));

        assert!(context.contains("date_trunc(day)"), "{context}");
        assert!(context.contains("i64 Timestamp domain"), "{context}");
    }

    #[test]
    fn date_add_day_uses_exact_utc_micros_and_propagates_null() {
        const DAY: i64 = 86_400_000_000;
        let mut builder = ChunkBuilder::new(vec![LogicalType::Timestamp, LogicalType::Int64]);
        for row in [
            vec![Value::Timestamp(-1), Value::Int64(1)],
            vec![Value::Timestamp(0), Value::Int64(-1)],
            vec![Value::Timestamp(DAY), Value::Int64(29)],
            vec![Value::Null, Value::Int64(1)],
            vec![Value::Timestamp(0), Value::Null],
        ] {
            builder.push_row(row).unwrap();
        }
        let chunk = builder.finish();
        let columns = HashMap::from([("p.time".into(), 0), ("p.days".into(), 1)]);
        let expression = date_add(Expr::Col("p.time".into()), Expr::Col("p.days".into()));

        assert_eq!(
            evaluate(&expression, &chunk, &columns).unwrap(),
            vec![
                Value::Timestamp(DAY - 1),
                Value::Timestamp(-DAY),
                Value::Timestamp(30 * DAY),
                Value::Null,
                Value::Null,
            ]
        );
    }

    #[test]
    fn date_add_day_checks_multiplication_and_addition_separately() {
        let multiplication = date_add(
            Expr::Lit(Value::Timestamp(0)),
            Expr::Lit(Value::Int64(i64::MAX)),
        );
        let addition = date_add(
            Expr::Lit(Value::Timestamp(i64::MAX)),
            Expr::Lit(Value::Int64(1)),
        );
        let chunk = empty_chunk(1);

        let context = invalid_context(evaluate(&multiplication, &chunk, &HashMap::new()));
        assert!(context.contains("date_add(day)"), "{context}");
        assert!(context.contains("multiplying"), "{context}");

        let context = invalid_context(evaluate(&addition, &chunk, &HashMap::new()));
        assert!(context.contains("date_add(day)"), "{context}");
        assert!(context.contains("adding"), "{context}");
    }

    #[test]
    fn decimal_rounding_propagates_null_and_checks_declared_precision() {
        let null = round(Expr::Lit(Value::Null), 0);
        assert_eq!(
            evaluate(&null, &empty_chunk(1), &HashMap::new()).unwrap(),
            vec![Value::Null]
        );

        let chunk = single_column(
            LogicalType::Decimal {
                precision: 3,
                scale: 2,
            },
            vec![decimal(999, 2)],
        );
        let expression = round(Expr::Col("p.value".into()), 0);
        let context = invalid_context(evaluate(&expression, &chunk, &column_map()));
        assert!(context.contains("Decimal(3, 2)"), "{context}");
    }

    #[test]
    fn round_div_propagates_null_before_zero_and_handles_different_scales() {
        let chunk = empty_chunk(1);
        for expression in [
            round_div(Expr::Lit(Value::Null), Expr::Lit(decimal(0, 2)), 1),
            round_div(Expr::Lit(decimal(100, 2)), Expr::Lit(Value::Null), 1),
        ] {
            assert_eq!(
                evaluate(&expression, &chunk, &HashMap::new()).unwrap(),
                vec![Value::Null]
            );
        }

        let different_scales = round_div(Expr::Lit(decimal(100, 2)), Expr::Lit(decimal(2, 1)), 1);
        assert_eq!(
            evaluate(&different_scales, &chunk, &HashMap::new()).unwrap(),
            vec![decimal(500, 2)]
        );

        let zero = round_div(Expr::Lit(decimal(100, 2)), Expr::Lit(decimal(0, 2)), 1);
        let context = invalid_context(evaluate(&zero, &chunk, &HashMap::new()));
        assert!(context.contains("division by zero"), "{context}");
    }

    #[test]
    fn if_true_rows_sever_unselected_round_div_errors() {
        let chunk = single_column(
            LogicalType::Bool,
            vec![Value::Bool(true), Value::Bool(true)],
        );
        let erroring = round_div(Expr::Lit(decimal(100, 2)), Expr::Lit(decimal(0, 2)), 1);
        let expression = if_expr(
            Expr::Col("p.value".into()),
            Expr::Lit(decimal(700, 2)),
            erroring,
        );

        assert_eq!(
            evaluate(&expression, &chunk, &column_map()).unwrap(),
            vec![decimal(700, 2), decimal(700, 2)]
        );
    }

    #[test]
    fn decimal_finishing_known_answers_are_stable_across_multiple_chunks() {
        fn build_chunk(start: usize, len: usize) -> Chunk {
            let ty = LogicalType::Decimal {
                precision: 6,
                scale: 2,
            };
            let mut builder = ChunkBuilder::new(vec![ty, ty]);
            for row in start..start + len {
                let (cost, denominator) = match row % 3 {
                    0 => (1249, 500),
                    1 => (1250, 1000),
                    _ => (-1250, 1000),
                };
                builder
                    .push_row(vec![decimal(cost, 2), decimal(denominator, 2)])
                    .unwrap();
            }
            builder.finish()
        }

        let chunks = [
            build_chunk(0, CHUNK_CAPACITY),
            build_chunk(CHUNK_CAPACITY, 3),
        ];
        let columns = HashMap::from([("cost.value".into(), 0), ("cost.baseline".into(), 1)]);
        let rounded_cost = round(Expr::Col("cost.value".into()), 0);
        let ratio = round_div(
            Expr::Col("cost.value".into()),
            Expr::Col("cost.baseline".into()),
            1,
        );

        for (chunk_index, chunk) in chunks.iter().enumerate() {
            let rounded = evaluate(&rounded_cost, chunk, &columns).unwrap();
            let ratios = evaluate(&ratio, chunk, &columns).unwrap();
            for row in 0..chunk.row_count() {
                let global_row = chunk_index * CHUNK_CAPACITY + row;
                let (expected_round, expected_ratio) = match global_row % 3 {
                    0 => (decimal(1200, 2), decimal(250, 2)),
                    1 => (decimal(1300, 2), decimal(130, 2)),
                    _ => (decimal(-1300, 2), decimal(-130, 2)),
                };
                assert_eq!(rounded[row], expected_round, "rounded row {global_row}");
                assert_eq!(ratios[row], expected_ratio, "ratio row {global_row}");
            }
        }
    }

    #[test]
    fn nested_if_inside_coalesce_inside_binary_arithmetic_is_lazy() {
        let mut builder = ChunkBuilder::new(vec![LogicalType::Bool, LogicalType::Int64]);
        for row in [
            vec![Value::Bool(true), Value::Int64(2)],
            vec![Value::Bool(false), Value::Int64(0)],
            vec![Value::Null, Value::Int64(0)],
        ] {
            builder.push_row(row).unwrap();
        }
        let chunk = builder.finish();
        let columns = HashMap::from([("p.cond".into(), 0), ("p.denominator".into(), 1)]);
        let conditional = if_expr(
            Expr::Col("p.cond".into()),
            binary(
                BinaryOp::Div,
                Expr::Lit(Value::Int64(20)),
                Expr::Col("p.denominator".into()),
            ),
            Expr::Lit(Value::Null),
        );
        let expr = binary(
            BinaryOp::Add,
            Expr::Coalesce(vec![conditional, Expr::Lit(Value::Int64(5))]),
            Expr::Lit(Value::Int64(10)),
        );

        assert_eq!(
            evaluate(&expr, &chunk, &columns).unwrap(),
            vec![Value::Int64(20), Value::Int64(15), Value::Int64(15)]
        );
    }

    #[test]
    fn a6_row_selection_is_stable_across_multiple_chunks() {
        fn build_chunk(start: usize, len: usize) -> Chunk {
            let mut builder = ChunkBuilder::new(vec![LogicalType::Bool, LogicalType::Int64]);
            for value in start..start + len {
                builder
                    .push_row(vec![
                        Value::Bool(value % 2 == 0),
                        Value::Int64(value as i64),
                    ])
                    .unwrap();
            }
            builder.finish()
        }

        let chunks = [
            build_chunk(0, CHUNK_CAPACITY),
            build_chunk(CHUNK_CAPACITY, 3),
        ];
        let columns = HashMap::from([("p.cond".into(), 0), ("p.value".into(), 1)]);
        let expr = Expr::Coalesce(vec![
            if_expr(
                Expr::Col("p.cond".into()),
                Expr::Col("p.value".into()),
                Expr::Lit(Value::Null),
            ),
            Expr::Lit(Value::Int64(-1)),
        ]);
        let actual = chunks
            .iter()
            .map(|chunk| evaluate(&expr, chunk, &columns).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(actual[0].len(), CHUNK_CAPACITY);
        assert_eq!(actual[0][CHUNK_CAPACITY - 2], Value::Int64(2046));
        assert_eq!(actual[0][CHUNK_CAPACITY - 1], Value::Int64(-1));
        assert_eq!(
            actual[1],
            vec![Value::Int64(2048), Value::Int64(-1), Value::Int64(2050)]
        );
    }
}
