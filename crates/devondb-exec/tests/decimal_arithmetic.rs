//! Runtime semantics for exact Decimal arithmetic through the evaluator:
//! `+ - *` never round or wrap, Int64 operands
//! promote exactly, `/` and Float64 mixes stay refused, and NULL propagates
//! exactly as it does for Int64.

use std::collections::HashMap;

use devondb_exec::chunk::{Chunk, ChunkBuilder};
use devondb_exec::eval::evaluate;
use devondb_plan::expr::{BinaryOp, Expr};
use devondb_types::DevonError;
use devondb_types::decimal::Decimal128;
use devondb_types::value::Value;

fn decimal(digits: i128, scale: u8) -> Value {
    Value::Decimal(Decimal128::new(digits, scale).unwrap())
}

fn binary(op: BinaryOp, left: Value, right: Value) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(Expr::Lit(left)),
        right: Box::new(Expr::Lit(right)),
    }
}

fn one_row_chunk() -> Chunk {
    let mut builder = ChunkBuilder::new(Vec::new());
    builder.push_row(Vec::new()).unwrap();
    builder.finish()
}

fn eval(expression: &Expr) -> Value {
    evaluate(expression, &one_row_chunk(), &HashMap::new())
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}

fn eval_error(expression: &Expr) -> String {
    let DevonError::InvalidArgument { context } =
        evaluate(expression, &one_row_chunk(), &HashMap::new()).unwrap_err()
    else {
        panic!("expected InvalidArgument");
    };
    context
}

#[test]
fn decimal_add_and_sub_are_exact_including_negatives_and_carry() {
    assert_eq!(
        eval(&binary(BinaryOp::Add, decimal(12345, 2), decimal(100, 2))),
        decimal(12445, 2)
    );
    // Carry across the scale boundary: 999.99 + 0.01 = 1000.00.
    assert_eq!(
        eval(&binary(BinaryOp::Add, decimal(99999, 2), decimal(1, 2))),
        decimal(100000, 2)
    );
    assert_eq!(
        eval(&binary(BinaryOp::Add, decimal(-1999, 2), decimal(-1, 2))),
        decimal(-2000, 2)
    );
    assert_eq!(
        eval(&binary(BinaryOp::Sub, decimal(100, 2), decimal(25050, 2))),
        decimal(-24950, 2)
    );
}

#[test]
fn decimal_add_sub_require_equal_scales_with_the_existing_message() {
    assert_eq!(
        eval_error(&binary(BinaryOp::Add, decimal(1, 2), decimal(1, 3))),
        "operator `add` requires equal Decimal scales; got scales 2 and 3"
    );
    assert_eq!(
        eval_error(&binary(BinaryOp::Sub, decimal(1, 3), decimal(1, 2))),
        "operator `sub` requires equal Decimal scales; got scales 3 and 2"
    );
}

#[test]
fn decimal_add_overflow_at_38_digits_is_an_error_never_a_wrap() {
    // 38 nines + 1 cent is a 39-digit result: past Decimal(38, 2).
    let max_38 = decimal(10i128.pow(38) - 1, 2);
    assert_eq!(
        eval_error(&binary(BinaryOp::Add, max_38.clone(), decimal(1, 2))),
        "operator `add` overflowed Decimal(38, 2)"
    );
    // i128-range overflow takes the same path.
    assert_eq!(
        eval_error(&binary(BinaryOp::Add, max_38.clone(), max_38.clone())),
        "operator `add` overflowed Decimal(38, 2)"
    );
    assert_eq!(
        eval_error(&binary(BinaryOp::Sub, decimal(-10i128.pow(38), 2), max_38)),
        "operator `sub` overflowed Decimal(38, 2)"
    );
}

#[test]
fn decimal_mul_adds_scales_and_checks_overflow() {
    // 1.23 × 2.0 = 2.460: exact product digits 123 × 20 = 2460, scale 2 + 1.
    assert_eq!(
        eval(&binary(BinaryOp::Mul, decimal(123, 2), decimal(20, 1))),
        decimal(2460, 3)
    );
    assert_eq!(
        eval(&binary(BinaryOp::Mul, decimal(-5, 0), decimal(25, 1))),
        decimal(-125, 1)
    );

    // 10^19 × 10^19 digits is a 39-digit product at scale 4.
    let big = decimal(10i128.pow(19), 2);
    assert_eq!(
        eval_error(&binary(BinaryOp::Mul, big.clone(), big)),
        "operator `mul` overflowed Decimal(38, 4)"
    );

    // Scale sums above 38 are refused even when the digits would fit.
    assert_eq!(
        eval_error(&binary(BinaryOp::Mul, decimal(1, 30), decimal(1, 9))),
        "operator `mul` would produce Decimal scale 39 above the maximum 38; got scales 30 and 9"
    );
}

#[test]
fn int64_operands_promote_exactly_to_the_decimal_scale() {
    // 1.50 + 2: the Int64 promotes to 200 at scale 2, then adds exactly.
    assert_eq!(
        eval(&binary(BinaryOp::Add, decimal(150, 2), Value::Int64(2))),
        decimal(350, 2)
    );
    assert_eq!(
        eval(&binary(BinaryOp::Sub, decimal(150, 2), Value::Int64(2))),
        decimal(-50, 2)
    );
    assert_eq!(
        eval(&binary(BinaryOp::Sub, Value::Int64(2), decimal(150, 2))),
        decimal(50, 2)
    );
    assert_eq!(
        eval(&binary(BinaryOp::Mul, Value::Int64(-7), decimal(150, 2))),
        decimal(-1050, 2)
    );
    // i64::MAX promotes exactly at scale 19 (19 + 19 = 38 digits).
    assert_eq!(
        eval(&binary(
            BinaryOp::Add,
            decimal(1, 19),
            Value::Int64(i64::MAX)
        )),
        decimal(i128::from(i64::MAX) * 10i128.pow(19) + 1, 19)
    );
}

#[test]
fn int64_promotion_scaling_overflow_is_an_error() {
    // i64::MAX scaled by 10^20 needs 39 digits: exactness cannot hold.
    assert_eq!(
        eval_error(&binary(
            BinaryOp::Add,
            decimal(1, 20),
            Value::Int64(i64::MAX)
        )),
        "operator `add` cannot promote Int64 operand 9223372036854775807 to Decimal scale 20 exactly; the 10^20 scaling overflowed"
    );
}

#[test]
fn float64_and_div_over_decimal_stay_refused() {
    assert_eq!(
        eval_error(&binary(BinaryOp::Add, decimal(1, 2), Value::Float64(1.0))),
        "operator `add` does not accept operand types Decimal(1, 2) and Float64"
    );
    assert_eq!(
        eval_error(&binary(BinaryOp::Mul, Value::Float64(1.0), decimal(1, 2))),
        "operator `mul` does not accept operand types Float64 and Decimal(1, 2)"
    );
    for (left, right) in [
        (decimal(100, 2), decimal(4, 2)),
        (decimal(100, 2), Value::Int64(4)),
        (Value::Int64(4), decimal(100, 2)),
    ] {
        assert_eq!(
            eval_error(&binary(BinaryOp::Div, left, right)),
            "div over Decimal would round; use round_div(numerator, denominator, places) \
             to name the rounding explicitly"
        );
    }
}

#[test]
fn null_propagation_mirrors_int64_arithmetic() {
    // Observed Int64 behavior: any NULL operand makes the result NULL.
    assert_eq!(
        eval(&binary(BinaryOp::Add, Value::Int64(1), Value::Null)),
        Value::Null
    );
    // Decimal arithmetic rides the same guard.
    for op in [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul] {
        assert_eq!(eval(&binary(op, decimal(150, 2), Value::Null)), Value::Null);
        assert_eq!(eval(&binary(op, Value::Null, decimal(150, 2))), Value::Null);
        assert_eq!(eval(&binary(op, decimal(150, 2), Value::Null)), Value::Null);
    }
}
