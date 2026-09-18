//! Canonical text-form printer (`docs/PLAN_IR.md` § Defaults (sugar) and
//! canonical printing, binding).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use crate::expr::{BinaryOp, DateTruncUnit, Expr, Metric};
use crate::ops::{
    AggregateFunction, AggregateItem, Direction, JoinKey, JoinType, KnnMode, KnnVectorSource,
    Operator, PLAN_VERSION, Plan, ProjectionItem, SortKey, SortOrder,
};
use crate::statement::{InterfaceColumn, RelRow, Statement, StatementEnvelope};
use devondb_types::{
    DevonError, DevonResult,
    logical_type::LogicalType,
    schema::{Column, fold},
    value::Value,
};

/// Prints a query plan in canonical DevonPlan text form.
pub fn print_plan(plan: &Plan) -> DevonResult<String> {
    ensure_current_version(plan.v, "query plan")?;
    let mut output = String::new();
    push_query(&mut output, &plan.plan)?;
    Ok(output)
}

fn push_query(output: &mut String, root: &Operator) -> DevonResult<()> {
    let layout = JoinLayout::new(root);
    for join in &layout.joins {
        let Operator::HashJoin { right, .. } = join else {
            return Err(print_error("join layout contains a non-join operator"));
        };
        output.push_str("let ");
        output.push_str(layout.name(join)?);
        output.push_str(" = ");
        print_operator(right, output, &layout)?;
        output.push_str(";\n");
    }
    print_operator(root, output, &layout)?;
    Ok(())
}

/// Prints a statement envelope in canonical DevonPlan text form.
pub fn print_statement(envelope: &StatementEnvelope) -> DevonResult<String> {
    ensure_current_version(envelope.v, "statement")?;
    let mut output = String::new();
    match &envelope.stmt {
        Statement::CreateNodeTable { name, columns } => {
            if columns.is_empty() {
                return Err(print_error("a node table requires at least one column"));
            }
            output.push_str("create node table ");
            push_identifier(&mut output, name)?;
            output.push_str(" (");
            push_columns(&mut output, columns)?;
            output.push(')');
        }
        Statement::CreateRelTable {
            name,
            from,
            to,
            columns,
        } => print_create_rel(&mut output, name, from, to, columns)?,
        Statement::InsertNode { table, rows } => {
            output.push_str("insert into ");
            push_identifier(&mut output, table)?;
            output.push_str(" values ");
            push_node_rows(&mut output, rows)?;
        }
        Statement::InsertRel { table, rows } => {
            output.push_str("insert rel into ");
            push_identifier(&mut output, table)?;
            output.push_str(" values ");
            push_rel_rows(&mut output, rows)?;
        }
        // Canonical upsert spelling; the parser accepts the same grammar.
        Statement::UpsertNode { table, rows } => {
            output.push_str("upsert ");
            push_identifier(&mut output, table)?;
            output.push_str(" values ");
            push_node_rows(&mut output, rows)?;
        }
        Statement::UpdateNode {
            table,
            set,
            key_column,
            key,
        } => {
            if set.is_empty() {
                return Err(print_error("an update requires at least one set item"));
            }
            output.push_str("update ");
            push_identifier(&mut output, table)?;
            output.push_str(" set ");
            for (index, item) in set.iter().enumerate() {
                if index > 0 {
                    output.push_str(", ");
                }
                push_identifier(&mut output, &item.column)?;
                output.push_str(" = ");
                push_value(&mut output, &item.value)?;
            }
            output.push_str(" where ");
            push_identifier(&mut output, key_column)?;
            output.push_str(" = ");
            push_value(&mut output, key)?;
        }
        Statement::DeleteNode {
            table,
            key_column,
            key,
        } => {
            output.push_str("delete from ");
            push_identifier(&mut output, table)?;
            output.push_str(" where ");
            push_identifier(&mut output, key_column)?;
            output.push_str(" = ");
            push_value(&mut output, key)?;
        }
        Statement::DetachDeleteNode {
            table,
            key_column,
            key,
        } => {
            output.push_str("detach delete from ");
            push_identifier(&mut output, table)?;
            output.push_str(" where ");
            push_identifier(&mut output, key_column)?;
            output.push_str(" = ");
            push_value(&mut output, key)?;
        }
        Statement::CopyNode {
            table,
            path,
            sort_by,
        } => {
            output.push_str("copy ");
            push_identifier(&mut output, table)?;
            output.push_str(" from ");
            push_string(&mut output, path);
            if let Some(column) = sort_by {
                output.push_str(" sort by ");
                push_identifier(&mut output, column)?;
            }
        }
        Statement::CreateInterface { name, columns } => {
            print_create_interface(&mut output, name, columns)?;
        }
        Statement::CreateClass {
            table,
            display,
            plural,
            label,
            summary,
            color,
            description,
            verb,
            inverse,
            implements,
        } => print_create_class(
            &mut output,
            table,
            display.as_deref(),
            plural.as_deref(),
            label.as_deref(),
            summary,
            color.as_deref(),
            description.as_deref(),
            verb.as_deref(),
            inverse.as_deref(),
            implements,
        )?,
        Statement::CreateHnswIndex {
            name,
            table,
            column,
            metric,
        } => {
            output.push_str("create hnsw index ");
            push_identifier(&mut output, name)?;
            output.push_str(" on ");
            push_identifier(&mut output, table)?;
            output.push('.');
            push_identifier(&mut output, column)?;
            output.push_str(" metric ");
            output.push_str(metric_text(*metric));
        }
        Statement::PinPlan { name, plan, .. } => {
            output.push_str("pin ");
            push_string(&mut output, name);
            output.push_str(" as ");
            output.push_str(&print_plan(plan)?);
        }
        Statement::UnpinPlan { name } => {
            output.push_str("unpin ");
            push_string(&mut output, name);
        }
    }
    Ok(output)
}

/// Prints an expression using the canonical precedence and escaping rules.
pub fn print_expression(expression: &Expr) -> DevonResult<String> {
    let mut output = String::new();
    push_expression(&mut output, expression, None)?;
    Ok(output)
}

fn ensure_current_version(version: u32, kind: &str) -> DevonResult<()> {
    if version != PLAN_VERSION {
        return Err(print_error(format!(
            "cannot print {kind} version {version}; text parsing produces version {PLAN_VERSION}"
        )));
    }
    Ok(())
}

struct JoinLayout<'operator> {
    joins: Vec<&'operator Operator>,
    names: HashMap<*const Operator, String>,
}

impl<'operator> JoinLayout<'operator> {
    fn new(root: &'operator Operator) -> Self {
        let mut occupied = HashSet::new();
        collect_declared_bindings(root, &mut occupied);
        let mut layout = Self {
            joins: Vec::new(),
            names: HashMap::new(),
        };
        let mut next = 1_u64;
        collect_join_layout(root, &mut layout, &occupied, &mut next);
        layout
    }

    fn name(&self, operator: &Operator) -> DevonResult<&str> {
        self.names
            .get(&std::ptr::from_ref(operator))
            .map(String::as_str)
            .ok_or_else(|| print_error("join has no generated let name"))
    }
}

fn collect_join_layout<'operator>(
    operator: &'operator Operator,
    layout: &mut JoinLayout<'operator>,
    occupied: &HashSet<String>,
    next: &mut u64,
) {
    if matches!(operator, Operator::HashJoin { .. }) {
        let name = next_join_name(occupied, next);
        layout.names.insert(std::ptr::from_ref(operator), name);
        layout.joins.push(operator);
    }
    match operator {
        Operator::ExpandRel { input, .. }
        | Operator::Expand { input, .. }
        | Operator::Filter { input, .. }
        | Operator::Project { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. }
        | Operator::Aggregate { input, .. } => {
            collect_join_layout(input, layout, occupied, next);
        }
        Operator::HashJoin { left, right, .. } => {
            collect_join_layout(left, layout, occupied, next);
            collect_join_layout(right, layout, occupied, next);
        }
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::KnnScan { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. } => {}
    }
}

fn next_join_name(occupied: &HashSet<String>, next: &mut u64) -> String {
    loop {
        let name = format!("j{next}");
        *next += 1;
        if !occupied.contains(fold(&name).as_ref()) {
            return name;
        }
    }
}

fn collect_declared_bindings(operator: &Operator, bindings: &mut HashSet<String>) {
    match operator {
        Operator::ExpandRel {
            binding,
            rel_binding,
            ..
        } => {
            bindings.insert(fold(binding).into_owned());
            bindings.insert(fold(rel_binding).into_owned());
        }

        Operator::TextScan { binding, .. }
        | Operator::ScanNodes { binding, .. }
        | Operator::ScanInterface { binding, .. }
        | Operator::Expand { binding, .. } => {
            bindings.insert(fold(binding).into_owned());
        }
        Operator::KnnScan { table, .. } | Operator::WithinScan { table, .. } => {
            bindings.insert(fold(table).into_owned());
        }
        _ => {}
    }
    match operator {
        Operator::ExpandRel { input, .. }
        | Operator::Expand { input, .. }
        | Operator::Filter { input, .. }
        | Operator::Project { input, .. }
        | Operator::Sort { input, .. }
        | Operator::Limit { input, .. }
        | Operator::Aggregate { input, .. } => collect_declared_bindings(input, bindings),
        Operator::HashJoin { left, right, .. } => {
            collect_declared_bindings(left, bindings);
            collect_declared_bindings(right, bindings);
        }
        Operator::ScanNodes { .. }
        | Operator::ScanInterface { .. }
        | Operator::KnnScan { .. }
        | Operator::TextScan { .. }
        | Operator::WithinScan { .. } => {}
    }
}

fn print_operator(
    operator: &Operator,
    output: &mut String,
    joins: &JoinLayout<'_>,
) -> DevonResult<Option<String>> {
    match operator {
        Operator::ScanNodes { table, binding } => {
            output.push_str("nodes(");
            push_identifier(output, table)?;
            output.push_str(") as ");
            push_identifier(output, binding)?;
            Ok(Some(binding.clone()))
        }
        Operator::ScanInterface { interface, binding } => {
            output.push_str("nodes(");
            push_identifier(output, interface)?;
            output.push_str(") as ");
            push_identifier(output, binding)?;
            Ok(Some(binding.clone()))
        }
        Operator::TextScan {
            table,
            column,
            query,
            k,
            binding,
        } => {
            output.push_str("textscan(");
            push_identifier(output, table)?;
            output.push('.');
            push_identifier(output, column)?;
            output.push_str(", ");
            push_string(output, query);
            output.push_str(", k=");
            push_count(output, *k, "textscan k")?;
            output.push_str(") as ");
            push_identifier(output, binding)?;
            Ok(Some(binding.clone()))
        }
        Operator::KnnScan {
            table,
            column,
            query,
            k,
            metric,
            mode,
        } => {
            push_knn(output, table, column, query, *k, *metric, *mode)?;
            Ok(None)
        }
        Operator::HashJoin { join, on, left, .. } => {
            let nearest = print_operator(left, output, joins)?;
            push_join(output, *join, joins.name(operator)?, on)?;
            Ok(nearest)
        }
        Operator::WithinScan {
            table,
            column,
            center,
            meters,
        } => {
            push_within(output, table, column, center, *meters)?;
            Ok(None)
        }
        Operator::ExpandRel {
            rel,
            direction,
            from_binding,
            binding,
            rel_binding,
            input,
        } => {
            let nearest = print_operator(input, output, joins)?;
            let mut stage = String::new();
            push_expand(
                &mut stage,
                rel,
                *direction,
                from_binding,
                binding,
                nearest.as_deref(),
            )?;
            output.push_str(&stage.replacen(" | expand ", " | expand_rel ", 1));
            output.push_str(" via ");
            push_identifier(output, rel_binding)?;
            Ok(Some(binding.clone()))
        }
        Operator::Expand {
            rel,
            direction,
            from_binding,
            binding,
            input,
        } => {
            let nearest = print_operator(input, output, joins)?;
            push_expand(
                output,
                rel,
                *direction,
                from_binding,
                binding,
                nearest.as_deref(),
            )?;
            Ok(Some(binding.clone()))
        }
        Operator::Filter { predicate, input } => {
            let nearest = print_operator(input, output, joins)?;
            output.push_str(" | filter ");
            push_expression(output, predicate, None)?;
            Ok(nearest)
        }
        Operator::Project { exprs, input } => {
            let nearest = print_operator(input, output, joins)?;
            push_project(output, exprs)?;
            Ok(nearest)
        }
        Operator::Sort { keys, input } => {
            let nearest = print_operator(input, output, joins)?;
            push_sort(output, keys)?;
            Ok(nearest)
        }
        Operator::Limit {
            count,
            offset,
            input,
        } => {
            let nearest = print_operator(input, output, joins)?;
            output.push_str(" | limit ");
            push_count(output, *count, "limit count")?;
            if let Some(offset) = offset {
                output.push_str(" offset ");
                push_count(output, *offset, "limit offset")?;
            }
            Ok(nearest)
        }
        Operator::Aggregate {
            group_by,
            aggs,
            input,
        } => {
            let nearest = print_operator(input, output, joins)?;
            push_aggregate(output, aggs, group_by)?;
            Ok(nearest)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_within(
    output: &mut String,
    table: &str,
    column: &str,
    center: &devondb_types::GeoPoint,
    meters: f64,
) -> DevonResult<()> {
    output.push_str("within(");
    push_identifier(output, table)?;
    output.push('.');
    push_identifier(output, column)?;
    output.push_str(", geo(");
    output.push_str(&float64_text(center.lat_deg())?);
    output.push_str(", ");
    output.push_str(&float64_text(center.lng_deg())?);
    output.push_str("), ");
    output.push_str(&float64_text(meters)?);
    output.push(')');
    Ok(())
}

fn push_knn(
    output: &mut String,
    table: &str,
    column: &str,
    query: &KnnVectorSource,
    k: u64,
    metric: Metric,
    mode: KnnMode,
) -> DevonResult<()> {
    output.push_str("knn(");
    push_identifier(output, table)?;
    output.push('.');
    push_identifier(output, column)?;
    output.push_str(", ");
    match query {
        KnnVectorSource::Literal(vector) => push_vector(output, vector)?,
        KnnVectorSource::Scalar { plan } => push_scalar(output, plan)?,
    }
    output.push_str(", ");
    push_count(output, k, "KNN `k`")?;
    output.push_str(", ");
    output.push_str(metric_text(metric));
    // Exact mode prints nothing so exact plans round-trip byte-identically.
    if !mode.is_exact() {
        output.push_str(", approximate");
    }
    output.push(')');
    Ok(())
}

fn push_expand(
    output: &mut String,
    rel: &str,
    direction: Direction,
    from_binding: &str,
    binding: &str,
    nearest: Option<&str>,
) -> DevonResult<()> {
    output.push_str(" | expand ");
    push_identifier(output, rel)?;
    output.push(' ');
    output.push_str(direction_text(direction));
    if nearest != Some(from_binding) {
        output.push_str(" from ");
        push_identifier(output, from_binding)?;
    }
    output.push_str(" as ");
    push_identifier(output, binding)?;
    Ok(())
}

fn push_join(output: &mut String, join: JoinType, name: &str, keys: &[JoinKey]) -> DevonResult<()> {
    if keys.is_empty() {
        return Err(print_error("join requires at least one equality key"));
    }
    output.push_str(" | ");
    if join == JoinType::Left {
        output.push_str("left ");
    }
    output.push_str("join ");
    push_identifier(output, name)?;
    output.push_str(" on ");
    for (index, key) in keys.iter().enumerate() {
        if index > 0 {
            output.push_str(" and ");
        }
        push_expression(
            output,
            &key.left,
            Some(ParentExpression {
                precedence: 4,
                side: ChildSide::Left,
                comparison: true,
            }),
        )?;
        output.push_str(" = ");
        push_expression(
            output,
            &key.right,
            Some(ParentExpression {
                precedence: 4,
                side: ChildSide::Right,
                comparison: true,
            }),
        )?;
    }
    Ok(())
}

fn push_project(output: &mut String, expressions: &[ProjectionItem]) -> DevonResult<()> {
    if expressions.is_empty() {
        return Err(print_error("project requires at least one expression"));
    }
    output.push_str(" | project ");
    for (index, item) in expressions.iter().enumerate() {
        push_separator(output, index);
        let canonical = print_expression(&item.expr)?;
        output.push_str(&canonical);
        if item.alias != canonical {
            output.push_str(" as ");
            push_identifier(output, &item.alias)?;
        }
    }
    Ok(())
}

fn push_sort(output: &mut String, keys: &[SortKey]) -> DevonResult<()> {
    if keys.is_empty() {
        return Err(print_error("sort requires at least one key"));
    }
    output.push_str(" | sort ");
    for (index, key) in keys.iter().enumerate() {
        push_separator(output, index);
        push_expression(output, &key.expr, None)?;
        if key.order == SortOrder::Desc {
            output.push_str(" desc");
        }
    }
    Ok(())
}

fn push_aggregate(
    output: &mut String,
    aggregates: &[AggregateItem],
    groups: &[Expr],
) -> DevonResult<()> {
    if aggregates.is_empty() {
        return Err(print_error(
            "aggregate requires at least one aggregate expression",
        ));
    }
    output.push_str(" | aggregate ");
    for (index, aggregate) in aggregates.iter().enumerate() {
        push_separator(output, index);
        output.push_str(aggregate_function_text(aggregate.function)?);
        output.push('(');
        if aggregate.function == AggregateFunction::PercentileCont {
            output.push_str("decimal(\"0.5\"), ");
        }
        push_expression(output, &aggregate.expr, None)?;
        output.push_str(") as ");
        push_identifier(output, &aggregate.alias)?;
    }
    if !groups.is_empty() {
        output.push_str(" by ");
        for (index, group) in groups.iter().enumerate() {
            push_separator(output, index);
            push_expression(output, group, None)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ParentExpression {
    precedence: u8,
    side: ChildSide,
    comparison: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChildSide {
    Left,
    Right,
    Prefix,
}

fn push_expression(
    output: &mut String,
    expression: &Expr,
    parent: Option<ParentExpression>,
) -> DevonResult<()> {
    let precedence = expression_precedence(expression);
    let parenthesized = parent.is_some_and(|context| {
        precedence < context.precedence
            || (precedence == context.precedence
                && (context.side == ChildSide::Right
                    || (context.comparison && context.side == ChildSide::Left)))
    });
    if parenthesized {
        output.push('(');
    }
    push_expression_body(output, expression, precedence)?;
    if parenthesized {
        output.push(')');
    }
    Ok(())
}

fn push_expression_body(output: &mut String, expression: &Expr, precedence: u8) -> DevonResult<()> {
    match expression {
        Expr::Col(column) => push_column_reference(output, column),
        Expr::ScoreOf(binding) => {
            output.push_str("scoreof(");
            push_identifier(output, binding)?;
            output.push(')');
            Ok(())
        }
        Expr::ClassOf(binding) => {
            output.push_str("classof(");
            push_identifier(output, binding)?;
            output.push(')');
            Ok(())
        }
        Expr::Lit(value) => push_value(output, value),
        Expr::Not(inner) => {
            output.push_str("not ");
            push_expression(
                output,
                inner,
                Some(ParentExpression {
                    precedence,
                    side: ChildSide::Prefix,
                    comparison: false,
                }),
            )
        }
        Expr::Binary { op, left, right } => push_binary(output, *op, left, right),
        Expr::Distance {
            left,
            right,
            metric,
        } => push_distance(output, left, right, *metric),
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => push_expression_call(output, "if", [cond.as_ref(), then_expr, else_expr]),
        Expr::Coalesce(expressions) => {
            push_variadic_expression_call(output, "coalesce", expressions)
        }
        Expr::Least(expressions) => push_variadic_expression_call(output, "least", expressions),
        Expr::Greatest(expressions) => {
            push_variadic_expression_call(output, "greatest", expressions)
        }
        Expr::DateTrunc { unit, value } => push_date_trunc(output, *unit, value),
        Expr::DateAdd {
            unit,
            value,
            amount,
        } => push_date_add(output, *unit, value, amount),
        Expr::Round { value, places } => push_round(output, value, *places),
        Expr::RoundDiv {
            numerator,
            denominator,
            places,
        } => push_round_div(output, numerator, denominator, *places),
        Expr::Scalar { plan } => push_scalar(output, plan),
    }
}

fn push_distance(
    output: &mut String,
    left: &Expr,
    right: &Expr,
    metric: Metric,
) -> DevonResult<()> {
    output.push_str("distance(");
    push_expression(output, left, None)?;
    output.push_str(", ");
    push_expression(output, right, None)?;
    output.push_str(", ");
    output.push_str(metric_text(metric));
    output.push(')');
    Ok(())
}

fn push_date_trunc(output: &mut String, unit: DateTruncUnit, value: &Expr) -> DevonResult<()> {
    output.push_str("date_trunc(\"");
    output.push_str(date_trunc_unit_text(unit));
    output.push_str("\", ");
    push_expression(output, value, None)?;
    output.push(')');
    Ok(())
}

fn push_date_add(
    output: &mut String,
    unit: DateTruncUnit,
    value: &Expr,
    amount: &Expr,
) -> DevonResult<()> {
    output.push_str("date_add(\"");
    output.push_str(date_trunc_unit_text(unit));
    output.push_str("\", ");
    push_expression(output, value, None)?;
    output.push_str(", ");
    push_expression(output, amount, None)?;
    output.push(')');
    Ok(())
}

fn push_round(output: &mut String, value: &Expr, places: u8) -> DevonResult<()> {
    ensure_places(places, "round")?;
    output.push_str("round(");
    push_expression(output, value, None)?;
    write!(output, ", {places})").map_err(|_| print_error("could not print `round` places"))
}

fn push_round_div(
    output: &mut String,
    numerator: &Expr,
    denominator: &Expr,
    places: u8,
) -> DevonResult<()> {
    ensure_places(places, "round_div")?;
    output.push_str("round_div(");
    push_expression(output, numerator, None)?;
    output.push_str(", ");
    push_expression(output, denominator, None)?;
    write!(output, ", {places})").map_err(|_| print_error("could not print `round_div` places"))
}

fn ensure_places(places: u8, name: &str) -> DevonResult<()> {
    if places > 38 {
        return Err(print_error(format!(
            "`{name}` places must be in 0..=38, got {places}"
        )));
    }
    Ok(())
}

fn push_scalar(output: &mut String, plan: &Operator) -> DevonResult<()> {
    output.push_str("scalar(");
    push_query(output, plan)?;
    output.push(')');
    Ok(())
}

fn push_expression_call<const N: usize>(
    output: &mut String,
    name: &str,
    expressions: [&Expr; N],
) -> DevonResult<()> {
    output.push_str(name);
    output.push('(');
    for (index, expression) in expressions.into_iter().enumerate() {
        push_separator(output, index);
        push_expression(output, expression, None)?;
    }
    output.push(')');
    Ok(())
}

fn push_variadic_expression_call(
    output: &mut String,
    name: &str,
    expressions: &[Expr],
) -> DevonResult<()> {
    if expressions.len() < 2 {
        return Err(print_error(format!(
            "`{name}` has wrong arity: expected at least 2 arguments, got {}",
            expressions.len()
        )));
    }
    output.push_str(name);
    output.push('(');
    for (index, expression) in expressions.iter().enumerate() {
        push_separator(output, index);
        push_expression(output, expression, None)?;
    }
    output.push(')');
    Ok(())
}

fn push_binary(output: &mut String, op: BinaryOp, left: &Expr, right: &Expr) -> DevonResult<()> {
    let precedence = binary_precedence(op);
    let comparison = is_comparison(op);
    push_expression(
        output,
        left,
        Some(ParentExpression {
            precedence,
            side: ChildSide::Left,
            comparison,
        }),
    )?;
    output.push(' ');
    output.push_str(binary_text(op));
    output.push(' ');
    push_expression(
        output,
        right,
        Some(ParentExpression {
            precedence,
            side: ChildSide::Right,
            comparison,
        }),
    )
}

fn expression_precedence(expression: &Expr) -> u8 {
    match expression {
        Expr::Binary { op, .. } => binary_precedence(*op),
        Expr::Not(_) => 3,
        Expr::Col(_)
        | Expr::ScoreOf(_)
        | Expr::ClassOf(_)
        | Expr::Lit(_)
        | Expr::Distance { .. }
        | Expr::If { .. }
        | Expr::Coalesce(_)
        | Expr::Least(_)
        | Expr::Greatest(_)
        | Expr::DateTrunc { .. }
        | Expr::DateAdd { .. }
        | Expr::Round { .. }
        | Expr::RoundDiv { .. }
        | Expr::Scalar { .. } => 7,
    }
}

fn binary_precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => 1,
        BinaryOp::And => 2,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            4
        }
        BinaryOp::Add | BinaryOp::Sub => 5,
        BinaryOp::Mul | BinaryOp::Div => 6,
    }
}

fn is_comparison(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    )
}

fn binary_text(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
    }
}

fn push_column_reference(output: &mut String, column: &str) -> DevonResult<()> {
    let Some((binding, name)) = column.split_once('.') else {
        return Err(print_error(format!(
            "column reference {column:?} has no `.` separator"
        )));
    };
    push_identifier(output, binding)?;
    output.push('.');
    push_identifier(output, name)
}

fn push_value(output: &mut String, value: &Value) -> DevonResult<()> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Int64(value) => output.push_str(&value.to_string()),
        Value::Float64(value) => output.push_str(&float64_text(*value)?),
        Value::String(value) => push_string(output, value),
        Value::Vector(values) => push_vector(output, values)?,
        Value::GeoPoint(point) => {
            // Canonical form guarantees finite components, so float64_text
            // cannot fail; reusing it keeps the float spelling identical to
            // Float64 literals and therefore parse-round-trippable.
            output.push_str("geo(");
            output.push_str(&float64_text(point.lat_deg())?);
            output.push_str(", ");
            output.push_str(&float64_text(point.lng_deg())?);
            output.push(')');
        }
        // Canonical spellings from `docs/PLAN_IR.md` § Literals.
        // Timestamp/Bytes/Decimal reuse Value's Display, which emits the
        // full function-style literal; Json goes through push_string so
        // its escaping matches every other text-form string.
        Value::Timestamp(_) | Value::Bytes(_) | Value::Decimal(_) => {
            output.push_str(&value.to_string());
        }
        Value::Json(text) => {
            output.push_str("json(");
            push_string(output, text);
            output.push(')');
        }
    }
    Ok(())
}

fn push_vector(output: &mut String, values: &[f32]) -> DevonResult<()> {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        push_separator(output, index);
        if !value.is_finite() {
            return Err(print_error(
                "non-finite vector element has no text spelling",
            ));
        }
        output.push_str(&float32_text(*value));
    }
    output.push(']');
    Ok(())
}

fn float64_text(value: f64) -> DevonResult<String> {
    if !value.is_finite() {
        return Err(print_error("non-finite Float64 has no text spelling"));
    }
    let mut decimal = value.to_string();
    if !decimal.contains(['.', 'e', 'E']) {
        decimal.push_str(".0");
    }
    Ok(decimal)
}

fn float32_text(value: f32) -> String {
    let mut decimal = value.to_string();
    // An integral spelling that the parser would read back as an integer
    // token must still round-trip the f32 bit pattern: `-0` would fold to
    // integer 0 and lose the sign bit, so `-0.0` keeps its `.0`.
    let loses_sign = value == 0.0 && value.is_sign_negative();
    if !decimal.contains(['.', 'e', 'E']) && (decimal.parse::<i64>().is_err() || loses_sign) {
        decimal.push_str(".0");
    }
    shorter(decimal, format!("{value:e}"))
}

fn shorter(decimal: String, scientific: String) -> String {
    if scientific.len() < decimal.len() {
        scientific
    } else {
        decimal
    }
}

fn push_count(output: &mut String, value: u64, argument: &str) -> DevonResult<()> {
    if value > i64::MAX as u64 {
        return Err(print_error(format!(
            "{argument} value {value} is outside the text Int64 range"
        )));
    }
    output.push_str(&value.to_string());
    Ok(())
}

fn print_create_rel(
    output: &mut String,
    name: &str,
    from: &str,
    to: &str,
    columns: &[Column],
) -> DevonResult<()> {
    output.push_str("create rel table ");
    push_identifier(output, name)?;
    output.push_str(" from ");
    push_identifier(output, from)?;
    output.push_str(" to ");
    push_identifier(output, to)?;
    if !columns.is_empty() {
        output.push_str(" (");
        push_columns(output, columns)?;
        output.push(')');
    }
    Ok(())
}

fn print_create_interface(
    output: &mut String,
    name: &str,
    columns: &[InterfaceColumn],
) -> DevonResult<()> {
    if columns.is_empty() {
        return Err(print_error("an interface requires at least one column"));
    }
    output.push_str("create interface ");
    push_identifier(output, name)?;
    output.push_str(" (");
    for (index, column) in columns.iter().enumerate() {
        push_separator(output, index);
        push_identifier(output, &column.name)?;
        output.push(' ');
        push_logical_type(output, column.ty);
    }
    output.push(')');
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_create_class(
    output: &mut String,
    table: &str,
    display: Option<&str>,
    plural: Option<&str>,
    label: Option<&str>,
    summary: &[String],
    color: Option<&str>,
    description: Option<&str>,
    verb: Option<&str>,
    inverse: Option<&str>,
    implements: &[String],
) -> DevonResult<()> {
    output.push_str("create class for ");
    push_identifier(output, table)?;
    let mut clauses = 0;
    push_optional_string_clause(output, &mut clauses, "display", display);
    push_optional_string_clause(output, &mut clauses, "plural", plural);
    if let Some(label) = label {
        push_class_clause_prefix(output, &mut clauses, "label");
        push_identifier(output, label)?;
    }
    push_optional_identifier_list(output, &mut clauses, "summary", summary)?;
    push_optional_string_clause(output, &mut clauses, "color", color);
    push_optional_string_clause(output, &mut clauses, "description", description);
    push_optional_string_clause(output, &mut clauses, "verb", verb);
    push_optional_string_clause(output, &mut clauses, "inverse", inverse);
    push_optional_identifier_list(output, &mut clauses, "implements", implements)?;
    if clauses > 0 {
        output.push(')');
    }
    Ok(())
}

fn push_optional_string_clause(
    output: &mut String,
    clauses: &mut usize,
    name: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        push_class_clause_prefix(output, clauses, name);
        push_string(output, value);
    }
}

fn push_optional_identifier_list(
    output: &mut String,
    clauses: &mut usize,
    name: &str,
    values: &[String],
) -> DevonResult<()> {
    if values.is_empty() {
        return Ok(());
    }
    push_class_clause_prefix(output, clauses, name);
    output.push('(');
    for (index, value) in values.iter().enumerate() {
        push_separator(output, index);
        push_identifier(output, value)?;
    }
    output.push(')');
    Ok(())
}

fn push_class_clause_prefix(output: &mut String, clauses: &mut usize, name: &str) {
    if *clauses == 0 {
        output.push_str(" (");
    } else {
        output.push_str(", ");
    }
    output.push_str(name);
    output.push(' ');
    *clauses += 1;
}

fn push_columns(output: &mut String, columns: &[Column]) -> DevonResult<()> {
    for (index, column) in columns.iter().enumerate() {
        push_separator(output, index);
        push_identifier(output, &column.name)?;
        output.push(' ');
        push_logical_type(output, column.ty);
        if column.primary_key {
            output.push_str(" primary key");
        }
    }
    Ok(())
}

fn push_logical_type(output: &mut String, logical_type: LogicalType) {
    match logical_type {
        LogicalType::Bool => output.push_str("Bool"),
        LogicalType::Int64 => output.push_str("Int64"),
        LogicalType::Float64 => output.push_str("Float64"),
        LogicalType::String => output.push_str("String"),
        LogicalType::Vector { dim } => {
            output.push_str("Vector(");
            output.push_str(&dim.to_string());
            output.push(')');
        }
        LogicalType::GeoPoint => output.push_str("GeoPoint"),
        // No text-surface spelling exists yet for quantized columns; the
        // Display form documents the type until the parser gains one.
        LogicalType::VectorEncoded { .. } => output.push_str(&logical_type.to_string()),
        // The Display form is the DDL name for `Timestamp`, `Bytes`,
        // `Decimal(p, s)`, and `Json`; the parser accepts the same spellings.
        LogicalType::Timestamp
        | LogicalType::Bytes
        | LogicalType::Decimal { .. }
        | LogicalType::Json => output.push_str(&logical_type.to_string()),
    }
}

fn push_node_rows(output: &mut String, rows: &[Vec<Value>]) -> DevonResult<()> {
    if rows.is_empty() {
        return Err(print_error("node insert requires at least one row"));
    }
    for (row_index, row) in rows.iter().enumerate() {
        if row.is_empty() {
            return Err(print_error("node insert rows require at least one value"));
        }
        push_separator(output, row_index);
        output.push('(');
        for (value_index, value) in row.iter().enumerate() {
            push_separator(output, value_index);
            push_value(output, value)?;
        }
        output.push(')');
    }
    Ok(())
}

fn push_rel_rows(output: &mut String, rows: &[RelRow]) -> DevonResult<()> {
    if rows.is_empty() {
        return Err(print_error("relationship insert requires at least one row"));
    }
    for (index, row) in rows.iter().enumerate() {
        push_separator(output, index);
        output.push('(');
        push_value(output, &row.from_key)?;
        output.push_str(" -> ");
        push_value(output, &row.to_key)?;
        for value in &row.values {
            output.push_str(", ");
            push_value(output, value)?;
        }
        output.push(')');
    }
    Ok(())
}

fn push_identifier(output: &mut String, identifier: &str) -> DevonResult<()> {
    if identifier.is_empty() {
        return Err(print_error("empty identifiers have no text spelling"));
    }
    if is_bare_identifier(identifier) && !is_reserved(identifier) {
        output.push_str(identifier);
        return Ok(());
    }
    output.push('`');
    push_escaped(output, identifier, '`', false);
    output.push('`');
    Ok(())
}

fn push_string(output: &mut String, value: &str) {
    output.push('"');
    push_escaped(output, value, '"', true);
    output.push('"');
}

fn push_escaped(output: &mut String, value: &str, delimiter: char, string: bool) {
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character == delimiter => {
                output.push('\\');
                output.push(character);
            }
            character if string && character <= '\u{1f}' => {
                let _ = write!(output, "\\u{{{:x}}}", u32::from(character));
            }
            character => output.push(character),
        }
    }
}

fn is_bare_identifier(identifier: &str) -> bool {
    let mut characters = identifier.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn is_reserved(identifier: &str) -> bool {
    matches!(
        identifier,
        "and"
            | "or"
            | "not"
            | "true"
            | "false"
            | "null"
            | "as"
            | "by"
            | "asc"
            | "desc"
            | "out"
            | "in"
            | "both"
            | "from"
            | "to"
            | "nodes"
            | "expand"
            | "filter"
            | "project"
            | "sort"
            | "limit"
            | "offset"
            | "aggregate"
            | "knn"
            | "distance"
            | "if"
            | "coalesce"
            | "least"
            | "greatest"
            | "date_trunc"
            | "date_add"
            | "round"
            | "round_div"
            | "scalar"
            | "cosine"
            | "l2"
            | "count"
            | "sum"
            | "min"
            | "max"
            | "avg"
            | "percentile_cont"
            | "create"
            | "insert"
            | "upsert"
            | "copy"
            | "update"
            | "set"
            | "delete"
            | "where"
            | "into"
            | "values"
            | "node"
            | "rel"
            | "table"
            | "primary"
            | "key"
            | "let"
            | "join"
            | "on"
    )
}

fn push_separator(output: &mut String, index: usize) {
    if index > 0 {
        output.push_str(", ");
    }
}

fn metric_text(metric: Metric) -> &'static str {
    match metric {
        Metric::Cosine => "cosine",
        Metric::L2 => "l2",
    }
}

fn date_trunc_unit_text(unit: DateTruncUnit) -> &'static str {
    match unit {
        DateTruncUnit::Day => "day",
    }
}

fn direction_text(direction: Direction) -> &'static str {
    match direction {
        Direction::Out => "out",
        Direction::In => "in",
        Direction::Both => "both",
    }
}

fn aggregate_function_text(function: AggregateFunction) -> DevonResult<&'static str> {
    match function {
        AggregateFunction::Count => Ok("count"),
        AggregateFunction::Sum => Ok("sum"),
        AggregateFunction::Min => Ok("min"),
        AggregateFunction::Max => Ok("max"),
        AggregateFunction::Avg => Ok("avg"),
        AggregateFunction::PercentileCont => Ok("percentile_cont"),
    }
}

fn print_error(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: format!("DevonPlan text print error: {}", context.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{print_expression, print_plan, print_statement};
    use crate::expr::{BinaryOp, DateTruncUnit, Expr, Metric};
    use crate::ops::{
        AggregateFunction, AggregateItem, Direction, JoinKey, JoinType, KnnMode, KnnVectorSource,
        Operator, PLAN_VERSION, Plan, ProjectionItem, SortKey, SortOrder,
    };
    use crate::statement::{InterfaceColumn, RelRow, SetItem, Statement, StatementEnvelope};
    use crate::text::parser::{Parsed, parse};
    use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};

    fn column(name: &str) -> Expr {
        Expr::Col(name.to_owned())
    }

    fn scan() -> Operator {
        Operator::ScanNodes {
            table: "Person".into(),
            binding: "p".into(),
        }
    }

    fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn join(join: JoinType, left: Operator, right: Operator) -> Operator {
        Operator::HashJoin {
            join,
            on: vec![JoinKey {
                left: column("p.id"),
                right: column("c.id"),
            }],
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    #[test]
    fn precedence_is_minimal_but_preserves_right_equal_subtrees() {
        let left = binary(
            BinaryOp::Sub,
            binary(BinaryOp::Sub, column("p.a"), column("p.b")),
            column("p.c"),
        );
        let right = binary(
            BinaryOp::Sub,
            column("p.a"),
            binary(BinaryOp::Sub, column("p.b"), column("p.c")),
        );
        assert_eq!(print_expression(&left).unwrap(), "p.a - p.b - p.c");
        assert_eq!(print_expression(&right).unwrap(), "p.a - (p.b - p.c)");
    }

    #[test]
    fn every_binary_operator_and_not_round_trips_through_text() {
        let operators = [
            BinaryOp::Eq,
            BinaryOp::Ne,
            BinaryOp::Lt,
            BinaryOp::Le,
            BinaryOp::Gt,
            BinaryOp::Ge,
            BinaryOp::And,
            BinaryOp::Or,
            BinaryOp::Add,
            BinaryOp::Sub,
            BinaryOp::Mul,
            BinaryOp::Div,
        ];
        for op in operators {
            let expression = Expr::Not(Box::new(binary(op, column("p.a"), column("p.b"))));
            let plan = Plan {
                v: PLAN_VERSION,
                plan: Operator::Filter {
                    predicate: expression,
                    input: Box::new(scan()),
                },
            };
            let text = print_plan(&plan).unwrap();
            assert_eq!(parse(&text).unwrap(), Parsed::Query(plan), "{text}");
        }
    }

    #[test]
    fn a6_calls_print_canonically_and_fixpoint_when_nested_in_binary_expressions() {
        let expression = binary(
            BinaryOp::Sub,
            Expr::If {
                cond: Box::new(binary(
                    BinaryOp::Eq,
                    column("p.active"),
                    Expr::Lit(Value::Bool(true)),
                )),
                then_expr: Box::new(Expr::Coalesce(vec![
                    binary(BinaryOp::Add, column("p.a"), column("p.b")),
                    Expr::Least(vec![column("p.c"), column("p.d")]),
                ])),
                else_expr: Box::new(Expr::Greatest(vec![column("p.e"), column("p.f")])),
            },
            Expr::DateTrunc {
                unit: DateTruncUnit::Day,
                value: Box::new(column("p.created_at")),
            },
        );
        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::Project {
                exprs: vec![ProjectionItem {
                    expr: expression,
                    alias: "result".into(),
                }],
                input: Box::new(scan()),
            },
        };

        let text = print_plan(&plan).unwrap();
        assert_eq!(
            text,
            concat!(
                "nodes(Person) as p | project if(p.active = true, ",
                "coalesce(p.a + p.b, least(p.c, p.d)), greatest(p.e, p.f)) - ",
                "date_trunc(\"day\", p.created_at) as result"
            )
        );
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed, Parsed::Query(plan));
        let Parsed::Query(parsed_plan) = parsed else {
            panic!("expected query");
        };
        assert_eq!(print_plan(&parsed_plan).unwrap(), text);
    }

    #[test]
    fn a6b_expressions_print_parse_and_reach_a_canonical_fixpoint() {
        let scalar = Expr::Scalar {
            plan: Box::new(Operator::Project {
                exprs: vec![ProjectionItem {
                    expr: column("u.value"),
                    alias: "value".into(),
                }],
                input: Box::new(Operator::ScanNodes {
                    table: "U".into(),
                    binding: "u".into(),
                }),
            }),
        };
        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::Project {
                exprs: vec![
                    ProjectionItem {
                        expr: Expr::DateAdd {
                            unit: DateTruncUnit::Day,
                            value: Box::new(column("p.created_at")),
                            amount: Box::new(Expr::Lit(Value::Int64(-6))),
                        },
                        alias: "shifted".into(),
                    },
                    ProjectionItem {
                        expr: Expr::Round {
                            value: Box::new(column("p.cost")),
                            places: 0,
                        },
                        alias: "rounded".into(),
                    },
                    ProjectionItem {
                        expr: Expr::RoundDiv {
                            numerator: Box::new(column("p.cost")),
                            denominator: Box::new(column("p.baseline")),
                            places: 1,
                        },
                        alias: "ratio".into(),
                    },
                    ProjectionItem {
                        expr: scalar,
                        alias: "lookup".into(),
                    },
                ],
                input: Box::new(scan()),
            },
        };
        let expected = concat!(
            "nodes(Person) as p | project date_add(\"day\", p.created_at, -6) as shifted, ",
            "round(p.cost, 0) as rounded, round_div(p.cost, p.baseline, 1) as ratio, ",
            "scalar(nodes(U) as u | project u.value as value) as lookup"
        );
        let text = print_plan(&plan).unwrap();
        assert_eq!(text, expected);
        assert_eq!(parse(&text).unwrap(), Parsed::Query(plan));
        let Parsed::Query(reparsed) = parse(&text).unwrap() else {
            panic!("expected query");
        };
        assert_eq!(print_plan(&reparsed).unwrap(), text);
    }

    #[test]
    fn every_expression_literal_form_round_trips_through_text() {
        let values = [
            Value::Null,
            Value::Bool(true),
            Value::Int64(-42),
            Value::Float64(30.0),
            Value::String("devon".into()),
            Value::Vector(vec![0.25, -0.5]),
        ];
        for value in values {
            let plan = Plan {
                v: PLAN_VERSION,
                plan: Operator::Filter {
                    predicate: Expr::Lit(value),
                    input: Box::new(scan()),
                },
            };
            let text = print_plan(&plan).unwrap();
            assert_eq!(parse(&text).unwrap(), Parsed::Query(plan), "{text}");
        }
    }

    #[test]
    fn every_aggregate_function_round_trips_through_text() {
        let functions = [
            AggregateFunction::Count,
            AggregateFunction::Sum,
            AggregateFunction::Min,
            AggregateFunction::Max,
            AggregateFunction::Avg,
            AggregateFunction::PercentileCont,
        ];
        for function in functions {
            let plan = Plan {
                v: PLAN_VERSION,
                plan: Operator::Aggregate {
                    group_by: Vec::new(),
                    aggs: vec![AggregateItem {
                        function,
                        expr: column("p.value"),
                        alias: "result".into(),
                    }],
                    input: Box::new(scan()),
                },
            };
            let text = print_plan(&plan).unwrap();
            assert_eq!(parse(&text).unwrap(), Parsed::Query(plan), "{text}");
        }
    }

    #[test]
    fn every_query_operator_prints_and_round_trips() {
        let expanded = Operator::Expand {
            rel: "Knows".into(),
            direction: Direction::Both,
            from_binding: "p".into(),
            binding: "f".into(),
            input: Box::new(scan()),
        };
        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::Limit {
                count: 10,
                offset: Some(0),
                input: Box::new(Operator::Aggregate {
                    group_by: vec![column("p.city")],
                    aggs: vec![AggregateItem {
                        function: AggregateFunction::Avg,
                        expr: column("f.age"),
                        alias: "average".into(),
                    }],
                    input: Box::new(Operator::Sort {
                        keys: vec![SortKey {
                            expr: column("f.age"),
                            order: SortOrder::Desc,
                        }],
                        input: Box::new(Operator::Project {
                            exprs: vec![ProjectionItem {
                                expr: column("f.age"),
                                alias: "f.age".into(),
                            }],
                            input: Box::new(expanded),
                        }),
                    }),
                }),
            },
        };
        let text = print_plan(&plan).unwrap();
        assert!(text.contains("expand Knows both as f"));
        assert!(text.contains("project f.age"));
        assert!(text.ends_with("limit 10 offset 0"));
        assert_eq!(parse(&text).unwrap(), Parsed::Query(plan));

        let no_offset = Plan {
            v: PLAN_VERSION,
            plan: Operator::Limit {
                count: 10,
                offset: None,
                input: Box::new(scan()),
            },
        };
        assert_eq!(
            print_plan(&no_offset).unwrap(),
            "nodes(Person) as p | limit 10"
        );
        assert_eq!(
            parse(&print_plan(&no_offset).unwrap()).unwrap(),
            Parsed::Query(no_offset)
        );
    }

    #[test]
    fn hash_join_prints_canonical_lets_and_round_trips_both_join_types() {
        for join_type in [JoinType::Inner, JoinType::Left] {
            let plan = Plan {
                v: PLAN_VERSION,
                plan: join(
                    join_type,
                    scan(),
                    Operator::ScanNodes {
                        table: "City".into(),
                        binding: "c".into(),
                    },
                ),
            };
            let text = print_plan(&plan).unwrap();
            let stage = if join_type == JoinType::Inner {
                "join j1"
            } else {
                "left join j1"
            };
            assert_eq!(
                text,
                format!("let j1 = nodes(City) as c;\nnodes(Person) as p | {stage} on p.id = c.id")
            );
            assert_eq!(parse(&text).unwrap(), Parsed::Query(plan));
        }
    }

    #[test]
    fn generated_join_names_skip_binding_collisions_to_preserve_round_trip() {
        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::HashJoin {
                join: JoinType::Inner,
                on: vec![JoinKey {
                    left: column("J1.id"),
                    right: column("c.id"),
                }],
                left: Box::new(Operator::ScanNodes {
                    table: "Person".into(),
                    binding: "J1".into(),
                }),
                right: Box::new(Operator::ScanNodes {
                    table: "City".into(),
                    binding: "c".into(),
                }),
            },
        };
        let text = print_plan(&plan).unwrap();
        assert!(text.starts_with("let j2 ="), "{text}");
        assert!(text.contains("join j2"), "{text}");
        assert_eq!(parse(&text).unwrap(), Parsed::Query(plan));
    }

    #[test]
    fn hash_join_json_omits_inner_and_spells_left_explicitly() {
        let inner = Plan {
            v: PLAN_VERSION,
            plan: join(
                JoinType::Inner,
                scan(),
                Operator::ScanNodes {
                    table: "City".into(),
                    binding: "c".into(),
                },
            ),
        };
        let json = inner.to_json().unwrap();
        assert!(!json.contains(r#""join":"#), "{json}");
        assert_eq!(Plan::from_json(&json).unwrap(), inner);

        let left = Plan {
            v: PLAN_VERSION,
            plan: join(
                JoinType::Left,
                scan(),
                Operator::ScanNodes {
                    table: "City".into(),
                    binding: "c".into(),
                },
            ),
        };
        let json = left.to_json().unwrap();
        assert!(json.contains(r#""join":"left""#), "{json}");
        assert_eq!(Plan::from_json(&json).unwrap(), left);
    }

    #[test]
    fn join_keywords_are_quoted_when_used_as_identifiers() {
        let envelope = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::CreateNodeTable {
                name: "let".into(),
                columns: vec![
                    Column {
                        name: "join".into(),
                        ty: LogicalType::Int64,
                        primary_key: true,
                    },
                    Column {
                        name: "on".into(),
                        ty: LogicalType::String,
                        primary_key: false,
                    },
                ],
            },
        };
        let text = print_statement(&envelope).unwrap();
        assert_eq!(
            text,
            "create node table `let` (`join` Int64 primary key, `on` String)"
        );
        assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope));
    }

    #[test]
    fn knn_distance_literals_and_quoted_names_round_trip() {
        let source = Operator::KnnScan {
            table: "my table".into(),
            column: "filter".into(),
            query: vec![0.25, -0.0, 2.0].into(),
            k: 3,
            metric: Metric::Cosine,
            mode: KnnMode::Exact,
        };
        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::Filter {
                predicate: Expr::Distance {
                    left: Box::new(column("p.embedding")),
                    right: Box::new(Expr::Lit(Value::Vector(vec![0.1, 0.2]))),
                    metric: Metric::L2,
                },
                input: Box::new(source),
            },
        };
        let text = print_plan(&plan).unwrap();
        assert!(text.starts_with("knn(`my table`.`filter`, [0.25, -0.0, 2], 3, cosine)"));
        assert_eq!(parse(&text).unwrap(), Parsed::Query(plan));
    }

    #[test]
    fn negative_zero_vector_elements_round_trip_bit_identically() {
        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::KnnScan {
                table: "Document".into(),
                column: "embedding".into(),
                query: vec![-0.0, 0.0, -1.5].into(),
                k: 2,
                metric: Metric::L2,
                mode: KnnMode::Exact,
            },
        };
        let text = print_plan(&plan).unwrap();
        assert_eq!(text, "knn(Document.embedding, [-0.0, 0, -1.5], 2, l2)");

        let Parsed::Query(reparsed) = parse(&text).unwrap() else {
            panic!("expected a query plan");
        };
        let Operator::KnnScan {
            query: KnnVectorSource::Literal(vector),
            ..
        } = reparsed.plan
        else {
            panic!("expected a literal KNN vector source");
        };
        let expected: Vec<u32> = [-0.0_f32, 0.0, -1.5].iter().map(|v| v.to_bits()).collect();
        let actual: Vec<u32> = vector.iter().map(|v| v.to_bits()).collect();
        assert_eq!(actual, expected, "text round-trip flipped a sign bit");
    }

    #[test]
    fn approximate_knn_prints_suffix_and_round_trips_while_exact_stays_bare() {
        let mut source = Operator::KnnScan {
            table: "Document".into(),
            column: "embedding".into(),
            query: vec![0.5].into(),
            k: 2,
            metric: Metric::Cosine,
            mode: KnnMode::Approximate,
        };
        let approximate = Plan {
            v: PLAN_VERSION,
            plan: source.clone(),
        };
        let text = print_plan(&approximate).unwrap();
        assert_eq!(
            text,
            "knn(Document.embedding, [0.5], 2, cosine, approximate)"
        );
        assert_eq!(parse(&text).unwrap(), Parsed::Query(approximate));

        if let Operator::KnnScan { mode, .. } = &mut source {
            *mode = KnnMode::Exact;
        }
        let exact = Plan {
            v: PLAN_VERSION,
            plan: source,
        };
        let text = print_plan(&exact).unwrap();
        assert_eq!(text, "knn(Document.embedding, [0.5], 2, cosine)");
        assert_eq!(parse(&text).unwrap(), Parsed::Query(exact));
    }

    #[test]
    fn float64_integral_spelling_keeps_float_type() {
        assert_eq!(
            print_expression(&Expr::Lit(Value::Float64(30.0))).unwrap(),
            "30.0"
        );
        assert_eq!(
            print_expression(&Expr::Lit(Value::Float64(-0.0))).unwrap(),
            "-0.0"
        );
        assert_eq!(
            print_expression(&Expr::Lit(Value::Float64(1e20))).unwrap(),
            "100000000000000000000.0"
        );
    }

    #[test]
    fn extreme_finite_vector_elements_use_parseable_shortest_text() {
        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::KnnScan {
                table: "T".into(),
                column: "v".into(),
                query: vec![f32::MAX, f32::MIN_POSITIVE].into(),
                k: 1,
                metric: Metric::L2,
                mode: KnnMode::Exact,
            },
        };
        let text = print_plan(&plan).unwrap();
        assert!(text.contains("3.4028235e38"));
        assert!(text.contains("1.1754944e-38"));
        assert_eq!(parse(&text).unwrap(), Parsed::Query(plan));
    }

    #[test]
    fn all_statement_forms_print_and_round_trip() {
        let statements = [
            Statement::CreateNodeTable {
                name: "Person".into(),
                columns: vec![Column {
                    name: "id".into(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                }],
            },
            Statement::CreateRelTable {
                name: "Likes".into(),
                from: "Person".into(),
                to: "Person".into(),
                columns: Vec::new(),
            },
            Statement::InsertNode {
                table: "Person".into(),
                rows: vec![vec![Value::Int64(1), Value::String("Ada".into())]],
            },
            Statement::InsertRel {
                table: "Likes".into(),
                rows: vec![RelRow {
                    from_key: Value::Int64(1),
                    to_key: Value::Int64(2),
                    values: Vec::new(),
                }],
            },
            Statement::UpdateNode {
                table: "Person".into(),
                set: vec![SetItem {
                    column: "name".into(),
                    value: Value::String("Ada".into()),
                }],
                key_column: "id".into(),
                key: Value::Int64(7),
            },
            Statement::DeleteNode {
                table: "Person".into(),
                key_column: "id".into(),
                key: Value::Int64(7),
            },
            Statement::DetachDeleteNode {
                table: "Person".into(),
                key_column: "id".into(),
                key: Value::Int64(7),
            },
            Statement::CopyNode {
                table: "Person".into(),
                path: "/tmp/people.csv".into(),
                sort_by: None,
            },
            Statement::CopyNode {
                table: "Person".into(),
                path: "/tmp/people.csv".into(),
                sort_by: Some("id".into()),
            },
            Statement::CreateHnswIndex {
                name: "embedding_cos".into(),
                table: "Corpus".into(),
                column: "embedding".into(),
                metric: Metric::Cosine,
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
                summary: vec!["name".into(), "role".into()],
                color: Some("#7aa2ff".into()),
                description: Some("a human".into()),
                verb: Some("knows".into()),
                inverse: Some("is known by".into()),
                implements: vec!["Nameable".into()],
            },
            Statement::CreateClass {
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
            Statement::PinPlan {
                name: "ada friends".into(),
                text: "pin \"ada friends\" as nodes(Person) as person".into(),
                plan: Plan {
                    v: PLAN_VERSION,
                    plan: Operator::ScanNodes {
                        table: "Person".into(),
                        binding: "person".into(),
                    },
                },
            },
            Statement::UnpinPlan {
                name: "ada friends".into(),
            },
        ];
        for statement in statements {
            let envelope = StatementEnvelope {
                v: PLAN_VERSION,
                stmt: statement,
            };
            let text = print_statement(&envelope).unwrap();
            assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope), "{text}");
        }
    }

    #[test]
    fn dml_statement_forms_print_parse_and_json_round_trip() {
        let cases = [
            (
                Statement::UpdateNode {
                    table: "Person".into(),
                    set: vec![SetItem {
                        column: "name".into(),
                        value: Value::String("Ada".into()),
                    }],
                    key_column: "id".into(),
                    key: Value::Int64(7),
                },
                "update Person set name = \"Ada\" where id = 7",
            ),
            (
                Statement::UpdateNode {
                    table: "Person".into(),
                    set: vec![
                        SetItem {
                            column: "name".into(),
                            value: Value::String("Ada".into()),
                        },
                        SetItem {
                            column: "age".into(),
                            value: Value::Int64(37),
                        },
                        SetItem {
                            column: "active".into(),
                            value: Value::Bool(true),
                        },
                    ],
                    key_column: "id".into(),
                    key: Value::Int64(7),
                },
                "update Person set name = \"Ada\", age = 37, active = true where id = 7",
            ),
            (
                Statement::DeleteNode {
                    table: "Person".into(),
                    key_column: "id".into(),
                    key: Value::Int64(7),
                },
                "delete from Person where id = 7",
            ),
            (
                Statement::DetachDeleteNode {
                    table: "Person".into(),
                    key_column: "id".into(),
                    key: Value::Int64(7),
                },
                "detach delete from Person where id = 7",
            ),
        ];

        for (statement, expected) in cases {
            let envelope = StatementEnvelope {
                v: PLAN_VERSION,
                stmt: statement,
            };
            let text = print_statement(&envelope).unwrap();
            assert_eq!(text, expected);
            assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope.clone()));
            let json = envelope.to_json().unwrap();
            assert_eq!(StatementEnvelope::from_json(&json).unwrap(), envelope);
        }
    }

    #[test]
    fn new_dml_keywords_are_quoted_when_used_as_identifiers() {
        let envelope = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::UpdateNode {
                table: "update".into(),
                set: vec![SetItem {
                    column: "set".into(),
                    value: Value::String("reserved".into()),
                }],
                key_column: "where".into(),
                key: Value::Int64(1),
            },
        };
        let text = print_statement(&envelope).unwrap();
        assert_eq!(
            text,
            "update `update` set `set` = \"reserved\" where `where` = 1"
        );
        assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope));

        let contextual = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::DetachDeleteNode {
                table: "detach".into(),
                key_column: "detach".into(),
                key: Value::Int64(1),
            },
        };
        let text = print_statement(&contextual).unwrap();
        assert_eq!(text, "detach delete from detach where detach = 1");
        assert_eq!(parse(&text).unwrap(), Parsed::Statement(contextual));
    }

    #[test]
    fn copy_statement_forms_print_parse_and_json_round_trip() {
        for (sort_by, expected) in [
            (None, "copy Person from \"/tmp/people.csv\""),
            (
                Some("id".to_owned()),
                "copy Person from \"/tmp/people.csv\" sort by id",
            ),
        ] {
            let envelope = StatementEnvelope {
                v: PLAN_VERSION,
                stmt: Statement::CopyNode {
                    table: "Person".into(),
                    path: "/tmp/people.csv".into(),
                    sort_by,
                },
            };
            let text = print_statement(&envelope).unwrap();
            assert_eq!(text, expected);
            assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope.clone()));
            let json = envelope.to_json().unwrap();
            assert_eq!(StatementEnvelope::from_json(&json).unwrap(), envelope);
        }
    }

    #[test]
    fn ontology_clauses_print_in_binding_order() {
        let envelope = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::CreateClass {
                table: "Person".into(),
                display: Some("Person".into()),
                plural: Some("people".into()),
                label: Some("name".into()),
                summary: vec!["name".into(), "role".into()],
                color: Some("#7aa2ff".into()),
                description: Some("a human".into()),
                verb: Some("knows".into()),
                inverse: Some("is known by".into()),
                implements: vec!["Nameable".into()],
            },
        };
        let text = print_statement(&envelope).unwrap();
        assert_eq!(
            text,
            concat!(
                "create class for Person (display \"Person\", plural \"people\", label name, ",
                "summary (name, role), color \"#7aa2ff\", description \"a human\", ",
                "verb \"knows\", inverse \"is known by\", implements (Nameable))"
            )
        );
        assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope));
    }

    #[test]
    fn pin_printer_uses_the_stored_plan_not_provenance_text() {
        let envelope = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::PinPlan {
                name: "ada \"friends\"".into(),
                text: "who does ada know".into(),
                plan: Plan {
                    v: PLAN_VERSION,
                    plan: Operator::ScanNodes {
                        table: "Person".into(),
                        binding: "person".into(),
                    },
                },
            },
        };
        assert_eq!(
            print_statement(&envelope).unwrap(),
            "pin \"ada \\\"friends\\\"\" as nodes(Person) as person"
        );
    }

    #[test]
    fn strings_and_identifiers_are_minimally_escaped() {
        let envelope = StatementEnvelope {
            v: PLAN_VERSION,
            stmt: Statement::InsertNode {
                table: "a`b\\c".into(),
                rows: vec![vec![Value::String("a\"b\\c\nd\u{1}".into())]],
            },
        };
        let text = print_statement(&envelope).unwrap();
        assert_eq!(
            text,
            "insert into `a\\`b\\\\c` values (\"a\\\"b\\\\c\\nd\\u{1}\")"
        );
        assert_eq!(parse(&text).unwrap(), Parsed::Statement(envelope));
    }

    #[test]
    fn malformed_ir_returns_print_errors() {
        assert!(print_expression(&Expr::Col("missing_dot".into())).is_err());
        assert!(print_expression(&Expr::Lit(Value::Float64(f64::NAN))).is_err());
        for expression in [
            Expr::Coalesce(Vec::new()),
            Expr::Least(vec![Expr::Lit(Value::Int64(1))]),
            Expr::Greatest(Vec::new()),
        ] {
            let error = print_expression(&expression).unwrap_err().to_string();
            assert!(error.contains("wrong arity"), "{error}");
        }
        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::Project {
                exprs: Vec::new(),
                input: Box::new(scan()),
            },
        };
        assert!(print_plan(&plan).is_err());

        let plan = Plan {
            v: PLAN_VERSION,
            plan: Operator::Limit {
                count: u64::MAX,
                offset: None,
                input: Box::new(scan()),
            },
        };
        assert!(print_plan(&plan).is_err());
    }
}
