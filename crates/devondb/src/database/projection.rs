//! Referenced-columns analysis for projection pushdown.
//!
//! The IR carries no column list on `Operator::ScanNodes` (docs/PLAN_IR.md
//! is unchanged); the facade derives the set of table columns a plan can
//! ever read for one scan binding, and the scan decodes only those (plus
//! the primary key and the hidden offset, which `view.rs` overlay shadow
//! checks and PK resolution always need).
//!
//! Safety rule: a variant this module cannot classify means "all columns",
//! never "none". Over-collection only costs decode time; under-collection
//! would be a correctness bug, so every walk below is exhaustive over
//! [`Expr`] and [`Operator`] and falls back to `None` (all columns).

use std::collections::{BTreeSet, HashMap};

use devondb_plan::{
    expr::Expr,
    ops::{KnnVectorSource, Operator},
};
use devondb_types::schema::{NodeTableSchema, fold};

/// Returns the table column indices a plan reads for one node-table scan
/// binding, or `None` when every column is required (the binding reaches
/// the query result through pass-through operators, or anything the
/// analysis cannot classify appears).
///
/// The primary key is NOT force-added here; callers fold it (and any
/// operator-internal columns such as a knn vector source) into the set
/// with [`with_required_columns`].
pub(super) fn referenced_columns(
    plan: &Operator,
    binding: &str,
    schema: &NodeTableSchema,
) -> Option<BTreeSet<usize>> {
    let names = referenced_names(plan, binding)?;
    let index_by_name = schema
        .columns()
        .iter()
        .enumerate()
        .map(|(index, column)| (fold(&column.name).into_owned(), index))
        .collect::<HashMap<_, _>>();
    let mut indices = BTreeSet::new();
    for name in names {
        // A reference this schema cannot place (a shadowing alias from a
        // project/aggregate, or an inner subplan's coincidental binding)
        // is unclassifiable here: decode everything.
        indices.insert(*index_by_name.get(&name)?);
    }
    Some(indices)
}

/// Returns the folded column names referenced under `binding` anywhere in
/// the plan, or `None` when all columns are required.
///
/// Interface scans use this directly: their emitted columns are interface
/// declarations mapped per implementing table, not one node schema.
pub(super) fn referenced_names(plan: &Operator, binding: &str) -> Option<BTreeSet<String>> {
    if binding_reaches_result(plan, binding) {
        return None;
    }
    let mut names = BTreeSet::new();
    collect_operator_names(plan, binding, &mut names)?;
    Some(names)
}

/// Adds columns a scan always or additionally needs (primary key, knn
/// vector source, within geo column) to a referenced set, normalizing a
/// set that ended up covering the whole schema to `None` (full decode).
pub(super) fn with_required_columns(
    referenced: Option<BTreeSet<usize>>,
    schema: &NodeTableSchema,
    required: impl IntoIterator<Item = usize>,
) -> Option<BTreeSet<usize>> {
    let mut set = referenced?;
    set.extend(required);
    (set.len() < schema.columns().len()).then_some(set)
}

/// Returns the position of `column` inside a referenced set — the physical
/// chunk index the narrowed scan gives it — or `None` when the column is
/// not in the set.
pub(super) fn position_in(referenced: &BTreeSet<usize>, column: usize) -> Option<usize> {
    referenced.iter().position(|index| *index == column)
}

/// Whether the scan output for `binding` reaches the query result
/// unprojected: every operator on the path from the root to the scan
/// passes its input columns through (`Filter`/`Sort`/`Limit`/`Expand`/
/// `HashJoin` do; `Project`/`Aggregate` rename their outputs and stop the
/// binding). `nodes(T) as t` with no projection therefore means all
/// columns, as do bare `KnnScan`/`WithinScan` roots, whose result exposes
/// the whole table.
fn binding_reaches_result(operator: &Operator, binding: &str) -> bool {
    let wanted = fold(binding);
    match operator {
        Operator::TextScan { binding: leaf, .. }
        | Operator::ScanNodes { binding: leaf, .. }
        | Operator::ScanInterface { binding: leaf, .. } => *wanted == fold(leaf),
        // KnnScan and WithinScan bind their table under the table name.
        Operator::KnnScan { table, .. } | Operator::WithinScan { table, .. } => {
            *wanted == fold(table)
        }
        Operator::Filter { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. } => binding_reaches_result(input, binding),
        Operator::Expand {
            input,
            binding: introduced,
            ..
        } => *wanted == fold(introduced) || binding_reaches_result(input, binding),
        Operator::ExpandRel {
            input,
            binding: introduced,
            rel_binding,
            ..
        } => {
            *wanted == fold(introduced)
                || *wanted == fold(rel_binding)
                || binding_reaches_result(input, binding)
        }
        Operator::HashJoin { left, right, .. } => {
            binding_reaches_result(left, binding) || binding_reaches_result(right, binding)
        }
        Operator::Project { .. } | Operator::Aggregate { .. } => false,
    }
}

/// Collects every `binding.column` reference an operator tree contains,
/// descending into scalar-subquery subplans (correlated references to the
/// outer binding live there) and knn scalar vector sources. Returns `None`
/// the moment anything unclassifiable names the binding — currently
/// `classof(binding)`, which the interface pipeline must answer for every
/// implementing class regardless of projected columns.
fn collect_operator_names(
    operator: &Operator,
    binding: &str,
    names: &mut BTreeSet<String>,
) -> Option<()> {
    match operator {
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. } => Some(()),
        Operator::KnnScan { query, .. } => match query {
            KnnVectorSource::Literal(_) => Some(()),
            KnnVectorSource::Scalar { plan } => collect_operator_names(plan, binding, names),
        },
        Operator::ExpandRel { input, .. }
        | Operator::Expand { input, .. }
        | Operator::Limit { input, .. } => collect_operator_names(input, binding, names),
        Operator::Filter { predicate, input } => {
            collect_expression_names(predicate, binding, names)?;
            collect_operator_names(input, binding, names)
        }
        Operator::Project { exprs, input } => {
            for item in exprs {
                collect_expression_names(&item.expr, binding, names)?;
            }
            collect_operator_names(input, binding, names)
        }
        Operator::Sort { keys, input } => {
            for key in keys {
                collect_expression_names(&key.expr, binding, names)?;
            }
            collect_operator_names(input, binding, names)
        }
        Operator::Aggregate {
            group_by,
            aggs,
            input,
        } => {
            for expression in group_by {
                collect_expression_names(expression, binding, names)?;
            }
            for aggregate in aggs {
                collect_expression_names(&aggregate.expr, binding, names)?;
            }
            collect_operator_names(input, binding, names)
        }
        Operator::HashJoin {
            on, left, right, ..
        } => {
            for key in on {
                collect_expression_names(&key.left, binding, names)?;
                collect_expression_names(&key.right, binding, names)?;
            }
            collect_operator_names(left, binding, names)?;
            collect_operator_names(right, binding, names)
        }
    }
}

fn collect_expression_names(
    expression: &Expr,
    binding: &str,
    names: &mut BTreeSet<String>,
) -> Option<()> {
    let wanted = fold(binding);
    match expression {
        Expr::Col(reference) => {
            let folded = fold(reference);
            if let Some(name) = folded.strip_prefix(format!("{wanted}.").as_str()) {
                names.insert(name.to_owned());
            }
            Some(())
        }
        Expr::ScoreOf(_) => Some(()),
        Expr::ClassOf(class_binding) => {
            if *wanted == fold(class_binding) {
                None
            } else {
                Some(())
            }
        }
        Expr::Lit(_) => Some(()),
        Expr::Binary { left, right, .. } | Expr::Distance { left, right, .. } => {
            collect_expression_names(left, binding, names)?;
            collect_expression_names(right, binding, names)
        }
        Expr::Not(operand) => collect_expression_names(operand, binding, names),
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            collect_expression_names(cond, binding, names)?;
            collect_expression_names(then_expr, binding, names)?;
            collect_expression_names(else_expr, binding, names)
        }
        Expr::Coalesce(expressions) | Expr::Least(expressions) | Expr::Greatest(expressions) => {
            for expression in expressions {
                collect_expression_names(expression, binding, names)?;
            }
            Some(())
        }
        Expr::DateTrunc { value, .. } | Expr::Round { value, .. } => {
            collect_expression_names(value, binding, names)
        }
        Expr::DateAdd { value, amount, .. } => {
            collect_expression_names(value, binding, names)?;
            collect_expression_names(amount, binding, names)
        }
        Expr::RoundDiv {
            numerator,
            denominator,
            ..
        } => {
            collect_expression_names(numerator, binding, names)?;
            collect_expression_names(denominator, binding, names)
        }
        Expr::Scalar { plan } => collect_operator_names(plan, binding, names),
    }
}

#[cfg(test)]
mod tests {
    use devondb_plan::{
        expr::{BinaryOp, DateTruncUnit, Expr, Metric},
        ops::{
            AggregateFunction, AggregateItem, Direction, JoinKey, JoinType, KnnMode,
            KnnVectorSource, Operator, ProjectionItem, SortKey, SortOrder,
        },
    };
    use devondb_types::{
        GeoPoint,
        logical_type::LogicalType,
        schema::{Column, NodeTableSchema},
        value::Value,
    };

    use super::{position_in, referenced_columns, referenced_names, with_required_columns};

    fn schema() -> NodeTableSchema {
        NodeTableSchema::new(
            "T".to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column("score", LogicalType::Float64, false),
                column("name", LogicalType::String, false),
                column("v", LogicalType::Vector { dim: 2 }, false),
                column("ts", LogicalType::Timestamp, false),
                column(
                    "dec",
                    LogicalType::Decimal {
                        precision: 12,
                        scale: 2,
                    },
                    false,
                ),
            ],
        )
        .unwrap()
    }

    fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
        Column {
            name: name.to_owned(),
            ty,
            primary_key,
        }
    }

    fn scan() -> Operator {
        Operator::ScanNodes {
            table: "T".to_owned(),
            binding: "t".to_owned(),
        }
    }

    fn col(name: &str) -> Expr {
        Expr::Col(name.to_owned())
    }

    fn indices(set: Option<std::collections::BTreeSet<usize>>) -> Vec<usize> {
        set.unwrap().into_iter().collect()
    }

    #[test]
    fn bare_scan_means_all_columns() {
        assert_eq!(referenced_columns(&scan(), "t", &schema()), None);
    }

    #[test]
    fn pass_through_operators_keep_all_columns() {
        let plan = Operator::Limit {
            count: 5,
            offset: None,
            input: Box::new(Operator::Filter {
                predicate: col("t.score"),
                input: Box::new(scan()),
            }),
        };
        assert_eq!(referenced_columns(&plan, "t", &schema()), None);
    }

    #[test]
    fn projection_narrows_to_the_referenced_subset() {
        let plan = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: col("t.name"),
                alias: "name".to_owned(),
            }],
            input: Box::new(Operator::Filter {
                predicate: Expr::Binary {
                    op: BinaryOp::Gt,
                    left: Box::new(col("t.score")),
                    right: Box::new(Expr::Lit(Value::Float64(1.0))),
                },
                input: Box::new(scan()),
            }),
        };
        assert_eq!(indices(referenced_columns(&plan, "t", &schema())), [1, 2]);
    }

    #[test]
    fn case_is_folded_and_unknown_columns_mean_all() {
        let plan = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: col("T.NAME"),
                alias: "name".to_owned(),
            }],
            input: Box::new(scan()),
        };
        assert_eq!(indices(referenced_columns(&plan, "t", &schema())), [2]);

        let shadowed = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: col("t.no_such_column"),
                alias: "x".to_owned(),
            }],
            input: Box::new(scan()),
        };
        assert_eq!(referenced_columns(&shadowed, "t", &schema()), None);
    }

    #[test]
    fn classof_and_correlated_scalar_references_are_classified() {
        let classof = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::ClassOf("t".to_owned()),
                alias: "class".to_owned(),
            }],
            input: Box::new(scan()),
        };
        assert_eq!(referenced_names(&classof, "t"), None);

        let correlated = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: col("t.score"),
                alias: "score".to_owned(),
            }],
            input: Box::new(Operator::Filter {
                predicate: Expr::Binary {
                    op: BinaryOp::Gt,
                    left: Box::new(col("t.score")),
                    right: Box::new(Expr::Scalar {
                        plan: Box::new(Operator::Project {
                            exprs: vec![ProjectionItem {
                                expr: col("t.dec"),
                                alias: "d".to_owned(),
                            }],
                            input: Box::new(Operator::ScanNodes {
                                table: "U".to_owned(),
                                binding: "u".to_owned(),
                            }),
                        }),
                    }),
                },
                input: Box::new(scan()),
            }),
        };
        // The correlated `t.dec` inside the subquery is collected even
        // though the subplan's own scan binds another table.
        assert_eq!(
            indices(referenced_columns(&correlated, "t", &schema())),
            [1, 5]
        );
    }

    /// Every `Expr` and `Operator` variant in one plan: the analysis must
    /// classify all of them (exhaustive matches above are the compile-time
    /// half of this guard) and still narrow to exactly the referenced
    /// columns of the target binding.
    #[test]
    fn every_expression_and_operator_variant_is_exhaustively_classified() {
        let inner_scalar = Expr::Scalar {
            plan: Box::new(Operator::Limit {
                count: 1,
                offset: Some(0),
                input: Box::new(Operator::Aggregate {
                    group_by: vec![col("t.id")],
                    aggs: vec![AggregateItem {
                        function: AggregateFunction::Max,
                        expr: col("t.score"),
                        alias: "m".to_owned(),
                    }],
                    input: Box::new(scan()),
                }),
            }),
        };
        let kitchen_sink = Expr::If {
            cond: Box::new(Expr::Not(Box::new(Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(Expr::Binary {
                    op: BinaryOp::Ge,
                    left: Box::new(col("t.score")),
                    right: Box::new(Expr::Lit(Value::Float64(0.0))),
                }),
                right: Box::new(Expr::Binary {
                    op: BinaryOp::Or,
                    left: Box::new(Expr::Binary {
                        op: BinaryOp::Add,
                        left: Box::new(col("t.id")),
                        right: Box::new(Expr::Lit(Value::Int64(1))),
                    }),
                    right: Box::new(Expr::Binary {
                        op: BinaryOp::Mul,
                        left: Box::new(Expr::Binary {
                            op: BinaryOp::Sub,
                            left: Box::new(col("t.id")),
                            right: Box::new(Expr::Lit(Value::Int64(2))),
                        }),
                        right: Box::new(Expr::Binary {
                            op: BinaryOp::Div,
                            left: Box::new(col("t.id")),
                            right: Box::new(Expr::Lit(Value::Int64(3))),
                        }),
                    }),
                }),
            }))),
            then_expr: Box::new(Expr::Coalesce(vec![
                col("t.name"),
                Expr::Least(vec![col("t.score"), Expr::Lit(Value::Float64(9.0))]),
                Expr::Greatest(vec![col("t.score"), Expr::Lit(Value::Float64(1.0))]),
                Expr::DateTrunc {
                    unit: DateTruncUnit::Day,
                    value: Box::new(col("t.ts")),
                },
                Expr::DateAdd {
                    unit: DateTruncUnit::Day,
                    value: Box::new(col("t.ts")),
                    amount: Box::new(Expr::Lit(Value::Int64(1))),
                },
                Expr::Round {
                    value: Box::new(col("t.dec")),
                    places: 1,
                },
                Expr::RoundDiv {
                    numerator: Box::new(col("t.dec")),
                    denominator: Box::new(Expr::Lit(Value::Int64(2))),
                    places: 1,
                },
                Expr::Distance {
                    left: Box::new(col("t.v")),
                    right: Box::new(Expr::Lit(Value::Vector(vec![0.0, 0.0]))),
                    metric: Metric::L2,
                },
                inner_scalar,
            ])),
            else_expr: Box::new(Expr::Lit(Value::Null)),
        };
        let plan = Operator::Project {
            exprs: vec![
                ProjectionItem {
                    expr: kitchen_sink,
                    alias: "sink".to_owned(),
                },
                ProjectionItem {
                    expr: col("t.name"),
                    alias: "name".to_owned(),
                },
            ],
            input: Box::new(Operator::HashJoin {
                join: JoinType::Left,
                on: vec![JoinKey {
                    left: col("t.id"),
                    right: col("u.id"),
                }],
                left: Box::new(Operator::Sort {
                    keys: vec![SortKey {
                        expr: col("t.score"),
                        order: SortOrder::Desc,
                    }],
                    input: Box::new(Operator::Expand {
                        rel: "R".to_owned(),
                        direction: Direction::Out,
                        from_binding: "t".to_owned(),
                        binding: "n".to_owned(),
                        input: Box::new(scan()),
                    }),
                }),
                right: Box::new(Operator::ScanNodes {
                    table: "U".to_owned(),
                    binding: "u".to_owned(),
                }),
            }),
        };
        // Collected: id (0) via binary/scalar/join key, score (1), name (2),
        // v (3) via distance, ts (4) via date functions, dec (5) via
        // round/rounddiv — every column of the schema.
        assert_eq!(
            indices(referenced_columns(&plan, "t", &schema())),
            [0, 1, 2, 3, 4, 5]
        );
        // The other join side's binding is analyzed independently.
        assert_eq!(indices(referenced_columns(&plan, "u", &schema())), [0]);

        // KnnScan and WithinScan leaves classify as leaves; their scalar
        // knn source subplan is walked.
        let knn = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: col("t.name"),
                alias: "name".to_owned(),
            }],
            input: Box::new(Operator::KnnScan {
                table: "T".to_owned(),
                column: "v".to_owned(),
                query: KnnVectorSource::Scalar {
                    plan: Box::new(Operator::Project {
                        exprs: vec![ProjectionItem {
                            expr: col("t.v"),
                            alias: "q".to_owned(),
                        }],
                        input: Box::new(scan()),
                    }),
                },
                k: 3,
                metric: Metric::Cosine,
                mode: KnnMode::Exact,
            }),
        };
        assert_eq!(indices(referenced_columns(&knn, "t", &schema())), [2, 3]);

        let within = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: col("t.name"),
                alias: "name".to_owned(),
            }],
            input: Box::new(Operator::WithinScan {
                table: "T".to_owned(),
                column: "g".to_owned(),
                center: GeoPoint::new(0.0, 0.0).unwrap(),
                meters: 1_000.0,
            }),
        };
        assert_eq!(indices(referenced_columns(&within, "t", &schema())), [2]);
    }

    #[test]
    fn required_columns_normalize_full_sets_to_none() {
        let schema = schema();
        let partial =
            with_required_columns(Some(std::collections::BTreeSet::from([1, 2])), &schema, [0]);
        assert_eq!(indices(partial), [0, 1, 2]);
        let full = with_required_columns(
            Some(std::collections::BTreeSet::from([0, 1, 2, 3, 4])),
            &schema,
            [5],
        );
        assert_eq!(full, None);
        assert_eq!(with_required_columns(None, &schema, [0]), None);
        let set = std::collections::BTreeSet::from([0, 2, 5]);
        assert_eq!(position_in(&set, 2), Some(1));
        assert_eq!(position_in(&set, 3), None);
    }
}
