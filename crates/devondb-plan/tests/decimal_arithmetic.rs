//! Typing rules for exact Decimal arithmetic.
//!
//! Pins the full operator × operand matrix: `+ - *` over Decimal operands
//! are exact (never rounded), Int64 operands promote exactly to
//! `Decimal(19, 0)`, `Float64` mixes and `/` stay refused under the
//! refuse-never-round law.

use std::collections::HashMap;

use devondb_plan::expr::{BinaryOp, Expr};
use devondb_plan::ops::Operator;
use devondb_plan::text::parser::{Parsed, parse};
use devondb_plan::typing::{ExpressionType, expression_type};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

fn columns() -> HashMap<String, LogicalType> {
    HashMap::from([
        (
            "l.price".to_owned(),
            LogicalType::Decimal {
                precision: 18,
                scale: 2,
            },
        ),
        (
            "l.small".to_owned(),
            LogicalType::Decimal {
                precision: 9,
                scale: 2,
            },
        ),
        (
            "l.other_scale".to_owned(),
            LogicalType::Decimal {
                precision: 18,
                scale: 3,
            },
        ),
        (
            "l.huge_scale".to_owned(),
            LogicalType::Decimal {
                precision: 4,
                scale: 30,
            },
        ),
        (
            "l.nine_scale".to_owned(),
            LogicalType::Decimal {
                precision: 9,
                scale: 9,
            },
        ),
        (
            "l.max_amount".to_owned(),
            LogicalType::Decimal {
                precision: 38,
                scale: 2,
            },
        ),
        (
            "l.d10".to_owned(),
            LogicalType::Decimal {
                precision: 10,
                scale: 2,
            },
        ),
        ("l.qty".to_owned(), LogicalType::Int64),
        ("l.score".to_owned(), LogicalType::Float64),
        ("l.name".to_owned(), LogicalType::String),
    ])
}

fn binary(op: BinaryOp, left: &str, right: &str) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(Expr::Col(left.into())),
        right: Box::new(Expr::Col(right.into())),
    }
}

fn decimal(precision: u8, scale: u8) -> ExpressionType {
    ExpressionType::Value(LogicalType::Decimal { precision, scale })
}

fn typed(expression: &Expr) -> ExpressionType {
    expression_type(expression, &columns()).unwrap()
}

fn typed_error(expression: &Expr) -> String {
    expression_type(expression, &columns())
        .unwrap_err()
        .to_string()
}

#[test]
fn add_and_sub_at_equal_scale_widen_precision_by_one_capped_at_38() {
    for op in [BinaryOp::Add, BinaryOp::Sub] {
        assert_eq!(typed(&binary(op, "l.price", "l.small")), decimal(19, 2));
        assert_eq!(
            typed(&binary(op, "l.max_amount", "l.price")),
            decimal(38, 2),
            "38 + 1 caps at 38"
        );
    }
}

#[test]
fn add_and_sub_at_unequal_scales_keep_the_existing_refusal_text() {
    for op in [BinaryOp::Add, BinaryOp::Sub] {
        let name = if op == BinaryOp::Add { "add" } else { "sub" };
        assert_eq!(
            typed_error(&binary(op, "l.price", "l.other_scale")),
            format!(
                "invalid argument: operator `{name}` requires equal Decimal scales; got scales 2 and 3"
            )
        );
        assert_eq!(
            typed_error(&binary(op, "l.other_scale", "l.price")),
            format!(
                "invalid argument: operator `{name}` requires equal Decimal scales; got scales 3 and 2"
            )
        );
    }
}

#[test]
fn mul_adds_scales_and_precisions_capped_at_38() {
    assert_eq!(
        typed(&binary(BinaryOp::Mul, "l.price", "l.other_scale")),
        decimal(36, 5)
    );
    assert_eq!(
        typed(&binary(BinaryOp::Mul, "l.max_amount", "l.small")),
        decimal(38, 4),
        "18 + 38 precision caps at 38"
    );
}

#[test]
fn mul_refuses_scale_sums_above_38() {
    assert_eq!(
        typed_error(&binary(BinaryOp::Mul, "l.huge_scale", "l.nine_scale")),
        "invalid argument: operator `mul` would produce Decimal scale 39 above the maximum 38; got scales 30 and 9"
    );
}

#[test]
fn div_over_decimal_stays_refused_and_points_at_round_div() {
    for (left, right) in [
        ("l.price", "l.small"),
        ("l.price", "l.other_scale"),
        ("l.price", "l.qty"),
    ] {
        assert_eq!(
            typed_error(&binary(BinaryOp::Div, left, right)),
            "invalid argument: div over Decimal would round; use \
             round_div(numerator, denominator, places) to name the rounding explicitly"
        );
    }
}

#[test]
fn int64_operands_promote_exactly_to_decimal_19_0() {
    for op in [BinaryOp::Add, BinaryOp::Sub] {
        // max(10, 19 + 2) + 1 = 22 at the Decimal operand's scale.
        assert_eq!(typed(&binary(op, "l.d10", "l.qty")), decimal(22, 2));
        assert_eq!(typed(&binary(op, "l.qty", "l.d10")), decimal(22, 2));
        // A large scale can push the promoted Int64 past 38 digits; typing
        // caps at 38 and runtime scaling is overflow-checked.
        assert_eq!(typed(&binary(op, "l.huge_scale", "l.qty")), decimal(38, 30));
    }
    // min(10 + 19, 38) at the Decimal operand's scale.
    assert_eq!(
        typed(&binary(BinaryOp::Mul, "l.d10", "l.qty")),
        decimal(29, 2)
    );
    assert_eq!(
        typed(&binary(BinaryOp::Mul, "l.qty", "l.d10")),
        decimal(29, 2)
    );
}

#[test]
fn float64_decimal_mixes_stay_refused_because_floats_would_round() {
    for op in [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul, BinaryOp::Div] {
        let name = match op {
            BinaryOp::Add => "add",
            BinaryOp::Sub => "sub",
            BinaryOp::Mul => "mul",
            _ => "div",
        };
        for (left, right) in [("l.price", "l.score"), ("l.score", "l.price")] {
            assert_eq!(
                typed_error(&binary(op, left, right)),
                format!(
                    "invalid argument: operator `{name}` over Decimal and Float64 would round; \
                     exact decimal arithmetic refuses Float64 operands"
                )
            );
        }
    }
}

#[test]
fn null_operands_mirror_int64_arithmetic() {
    let null = || Expr::Lit(Value::Null);
    for op in [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul] {
        for (left, right) in [
            (Expr::Col("l.price".into()), null()),
            (null(), Expr::Col("l.price".into())),
        ] {
            assert_eq!(
                typed(&Expr::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                }),
                decimal(18, 2),
                "a Null operand yields the Decimal operand's type, as Int64 does"
            );
        }
    }
    // Int64 + Null yields Int64 today; Decimal `div` with a Null operand
    // stays refused (the operand could be a Decimal at runtime).
    assert_eq!(
        typed(&Expr::Binary {
            op: BinaryOp::Add,
            left: Box::new(Expr::Col("l.qty".into())),
            right: Box::new(null()),
        }),
        ExpressionType::Value(LogicalType::Int64)
    );
    assert_eq!(
        typed_error(&Expr::Binary {
            op: BinaryOp::Div,
            left: Box::new(Expr::Col("l.price".into())),
            right: Box::new(null()),
        }),
        "invalid argument: div over Decimal would round; use \
         round_div(numerator, denominator, places) to name the rounding explicitly"
    );
}

#[test]
fn non_numeric_operands_keep_the_numeric_requirement_message() {
    assert_eq!(
        typed_error(&binary(BinaryOp::Add, "l.price", "l.name")),
        "invalid argument: operator `add` requires a numeric operand; got String"
    );
    assert_eq!(
        typed_error(&binary(BinaryOp::Add, "l.name", "l.price")),
        "invalid argument: operator `add` requires a numeric operand; got String"
    );
}

#[test]
fn text_spelling_price_times_qty_types_end_to_end() {
    let Parsed::Query(plan) =
        parse("nodes(LineItem) as l | project l.price * l.qty as total").unwrap()
    else {
        panic!("expected a query plan");
    };
    let Operator::Project { exprs, .. } = plan.plan else {
        panic!("expected a project root");
    };
    assert_eq!(
        expression_type(&exprs[0].expr, &columns()).unwrap(),
        decimal(37, 2),
        "Decimal(18, 2) * Int64 promotes to min(18 + 19, 38) at scale 2"
    );
}
