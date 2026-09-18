//! Plan validation: bind a `Plan` against table schemas before execution.

use std::collections::{HashMap, HashSet};

use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{NodeTableSchema, RelTableSchema, fold, suggestion_suffix};
use devondb_types::{DevonError, DevonResult};

use crate::expr::Expr;
use crate::ops::{Direction, KnnVectorSource, Operator, Plan, ensure_supported_version};
use crate::typing::{
    ExpressionType, InterfaceSchema, SchemaInput, aggregate_type_with_subqueries,
    expression_type_with_subqueries, knn_vector_source_type,
};

struct Bindings<'schema> {
    nodes: HashMap<String, &'schema NodeTableSchema>,
    interfaces: HashSet<String>,
    text_bindings: HashSet<String>,
    names: HashMap<String, String>,
    columns: HashMap<String, LogicalType>,
    outputs: Vec<LogicalType>,
}

impl<'schema> Bindings<'schema> {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            interfaces: HashSet::new(),
            text_bindings: HashSet::new(),
            names: HashMap::new(),
            columns: HashMap::new(),
            outputs: Vec::new(),
        }
    }

    fn bind_node(&mut self, binding: &str, schema: &'schema NodeTableSchema) {
        let name = binding.to_owned();
        let binding = fold(binding).into_owned();
        self.nodes.insert(binding.clone(), schema);
        self.names.insert(binding.clone(), name);
        for column in schema.columns() {
            self.columns
                .insert(format!("{binding}.{}", fold(&column.name)), column.ty);
            self.outputs.push(column.ty);
        }
    }

    fn bind_interface(&mut self, binding: &str, schema: &InterfaceSchema) {
        let name = binding.to_owned();
        let binding = fold(binding).into_owned();
        self.interfaces.insert(binding.clone());
        self.names.insert(binding.clone(), name);
        for column in schema.columns() {
            self.columns
                .insert(format!("{binding}.{}", fold(&column.name)), column.ty);
            self.outputs.push(column.ty);
        }
    }
}

/// Types one expression against outer visible columns WITH scalar-subquery
/// support: embedded plans validate against the supplied schemas, and their
/// correlated references resolve against `columns`. Correlated `Expand` from an
/// outer binding is not reconstructible from a flat column map and refuses
/// through the validator's normal unknown-binding error; the full validator
/// remains the final authority at plan admission.
pub fn expression_output_type_with_context(
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
    node_tables: &[NodeTableSchema],
    rel_tables: &[RelTableSchema],
) -> DevonResult<ExpressionType> {
    let schema = SchemaInput::tables_only(node_tables, rel_tables);
    expression_output_type_with_schema(expression, columns, &schema)
}

/// Types one expression with scalar-subquery support against complete plan
/// schema input, including executable ontology interfaces.
pub fn expression_output_type_with_schema(
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
    schema: &SchemaInput<'_>,
) -> DevonResult<ExpressionType> {
    let validator = Validator { schema: *schema };
    let empty = Bindings::new();
    let mut outer = Bindings::new();
    outer.columns = columns.clone();
    outer.text_bindings = columns
        .iter()
        .filter_map(|(name, ty)| {
            (*ty == LogicalType::Float64)
                .then(|| name.strip_prefix("\0devondb-scoreof\0"))
                .flatten()
                .map(str::to_owned)
        })
        .collect();
    let mut scalar_type = |plan: &Operator| validator.validate_scalar(plan, &empty, &outer);
    expression_type_with_subqueries(expression, columns, &mut scalar_type)
}

/// The aggregate-item sibling of [`expression_output_type_with_context`].
pub fn aggregate_output_type_with_context(
    function: crate::ops::AggregateFunction,
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
    node_tables: &[NodeTableSchema],
    rel_tables: &[RelTableSchema],
) -> DevonResult<LogicalType> {
    let schema = SchemaInput::tables_only(node_tables, rel_tables);
    aggregate_output_type_with_schema(function, expression, columns, &schema)
}

/// Types one aggregate item with scalar-subquery support against complete
/// plan schema input, including executable ontology interfaces.
pub fn aggregate_output_type_with_schema(
    function: crate::ops::AggregateFunction,
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
    schema: &SchemaInput<'_>,
) -> DevonResult<LogicalType> {
    let validator = Validator { schema: *schema };
    let empty = Bindings::new();
    let mut outer = Bindings::new();
    outer.columns = columns.clone();
    outer.text_bindings = columns
        .iter()
        .filter_map(|(name, ty)| {
            (*ty == LogicalType::Float64)
                .then(|| name.strip_prefix("\0devondb-scoreof\0"))
                .flatten()
                .map(str::to_owned)
        })
        .collect();
    let mut scalar_type = |plan: &Operator| validator.validate_scalar(plan, &empty, &outer);
    aggregate_type_with_subqueries(function, expression, columns, &mut scalar_type)
}

/// Validates a plan against the supplied node and relationship schemas.
pub fn validate(
    plan: &Plan,
    node_tables: &[NodeTableSchema],
    rel_tables: &[RelTableSchema],
) -> DevonResult<()> {
    let schema = SchemaInput::tables_only(node_tables, rel_tables);
    validate_with_schema(plan, &schema)
}

/// Validates a plan against complete table and ontology-interface schema
/// input.
pub fn validate_with_schema(plan: &Plan, schema: &SchemaInput<'_>) -> DevonResult<()> {
    ensure_supported_version(plan.v)?;
    let mut namespace = ScoreNamespace::default();
    namespace.operator(&plan.plan);
    if namespace.text && namespace.reserved {
        return Err(invalid_argument(
            "TextScan",
            "names beginning with the private score metadata prefix are reserved in plans containing TextScan",
        ));
    }
    Validator { schema: *schema }.validate_operator(&plan.plan, &Bindings::new())?;
    Ok(())
}

// Admission is whole-plan, including scalar subqueries and both join branches.
// Older plans without TextScan retain their existing arbitrary alias spelling.
#[derive(Default)]
struct ScoreNamespace {
    text: bool,
    reserved: bool,
}
impl ScoreNamespace {
    fn name(&mut self, name: &str) {
        self.reserved |= fold(name).starts_with("\0devondb-scoreof\0");
    }
    fn expression(&mut self, expr: &Expr) {
        match expr {
            Expr::Scalar { plan } => self.operator(plan),
            Expr::Binary { left, right, .. } | Expr::Distance { left, right, .. } => {
                self.expression(left);
                self.expression(right);
            }
            Expr::Not(value) | Expr::DateTrunc { value, .. } | Expr::Round { value, .. } => {
                self.expression(value)
            }
            Expr::If {
                cond,
                then_expr,
                else_expr,
            } => {
                self.expression(cond);
                self.expression(then_expr);
                self.expression(else_expr);
            }
            Expr::Coalesce(values) | Expr::Least(values) | Expr::Greatest(values) => {
                for value in values {
                    self.expression(value);
                }
            }
            Expr::DateAdd { value, amount, .. } => {
                self.expression(value);
                self.expression(amount);
            }
            Expr::RoundDiv {
                numerator,
                denominator,
                ..
            } => {
                self.expression(numerator);
                self.expression(denominator);
            }
            Expr::Col(_) | Expr::ClassOf(_) | Expr::ScoreOf(_) | Expr::Lit(_) => {}
        }
    }
    fn operator(&mut self, op: &Operator) {
        match op {
            Operator::TextScan { binding, .. } => {
                self.text = true;
                self.name(binding);
            }
            Operator::ScanNodes { binding, .. } | Operator::ScanInterface { binding, .. } => {
                self.name(binding)
            }
            Operator::Expand { binding, input, .. } => {
                self.name(binding);
                self.operator(input);
            }
            Operator::ExpandRel {
                binding,
                rel_binding,
                input,
                ..
            } => {
                self.name(binding);
                self.name(rel_binding);
                self.operator(input);
            }
            Operator::Project { exprs, input } => {
                self.operator(input);
                for item in exprs {
                    self.name(&item.alias);
                    self.expression(&item.expr);
                }
            }
            Operator::Aggregate {
                group_by,
                aggs,
                input,
            } => {
                self.operator(input);
                for expr in group_by {
                    self.expression(expr);
                }
                for item in aggs {
                    self.name(&item.alias);
                    self.expression(&item.expr);
                }
            }
            Operator::Filter { predicate, input } => {
                self.operator(input);
                self.expression(predicate);
            }
            Operator::Sort { keys, input } => {
                self.operator(input);
                for key in keys {
                    self.expression(&key.expr);
                }
            }
            Operator::Limit { input, .. } => self.operator(input),
            Operator::HashJoin {
                left, right, on, ..
            } => {
                self.operator(left);
                self.operator(right);
                for key in on {
                    self.expression(&key.left);
                    self.expression(&key.right);
                }
            }
            Operator::KnnScan { table, query, .. } => {
                self.name(table);
                if let crate::ops::KnnVectorSource::Scalar { plan } = query {
                    self.operator(plan);
                }
            }
            Operator::WithinScan { table, .. } => self.name(table),
        }
    }
}

struct Validator<'schema> {
    schema: SchemaInput<'schema>,
}

impl<'schema> Validator<'schema> {
    fn validate_operator(
        &self,
        operator: &Operator,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        match operator {
            Operator::ScanNodes { table, binding } => self.validate_scan(table, binding, outer),
            Operator::TextScan {
                table,
                column,
                binding,
                k,
                ..
            } => {
                if *k == 0 || i64::try_from(*k).is_err() || usize::try_from(*k).is_err() {
                    return Err(invalid_argument(
                        "TextScan",
                        "k must be positive and fit Int64 and usize",
                    ));
                }
                let mut bindings = self.validate_scan(table, binding, outer)?;
                let schema = self.node_table(table, "TextScan")?;
                let definition = schema
                    .columns()
                    .iter()
                    .find(|entry| fold(&entry.name) == fold(column))
                    .ok_or_else(|| {
                        invalid_argument("TextScan", format!("unknown column `{table}.{column}`"))
                    })?;
                if definition.ty != LogicalType::String {
                    return Err(invalid_argument(
                        "TextScan",
                        format!(
                            "column `{table}.{column}` expects String, got {}",
                            definition.ty
                        ),
                    ));
                }
                bindings.text_bindings.insert(fold(binding).into_owned());
                Ok(bindings)
            }
            Operator::ScanInterface { interface, binding } => {
                self.validate_interface_scan(interface, binding, outer)
            }
            Operator::Expand {
                rel,
                direction,
                from_binding,
                binding,
                input,
            } => self.validate_expand(rel, *direction, from_binding, binding, input, outer),
            Operator::ExpandRel {
                rel,
                direction,
                from_binding,
                binding,
                rel_binding,
                input,
            } => {
                let mut bindings =
                    self.validate_expand(rel, *direction, from_binding, binding, input, outer)?;
                if bindings.names.contains_key(fold(rel_binding).as_ref()) {
                    return Err(invalid_argument(
                        "ExpandRel",
                        format!("binding `{rel_binding}` is bound twice"),
                    ));
                }
                reject_outer_binding(rel_binding, outer)?;
                let schema = self.rel_table(rel, "ExpandRel")?;
                let key = fold(rel_binding).into_owned();
                bindings.names.insert(key.clone(), rel_binding.clone());
                for column in schema.columns() {
                    bindings
                        .columns
                        .insert(format!("{key}.{}", fold(&column.name)), column.ty);
                    bindings.outputs.push(column.ty);
                }
                Ok(bindings)
            }
            Operator::Filter { predicate, input } => self.validate_filter(predicate, input, outer),
            Operator::Project { exprs, input } => self.validate_project(exprs, input, outer),
            Operator::Sort { keys, input } => {
                let bindings = self.validate_operator(input, outer)?;
                for key in keys {
                    let ty = self.typed_expression(&key.expr, &bindings, outer, "Sort")?;
                    reject_unordered_sort_type(&key.expr, ty)?;
                }
                Ok(bindings)
            }
            Operator::Limit { input, .. } => self.validate_operator(input, outer),
            Operator::Aggregate {
                group_by,
                aggs,
                input,
            } => self.validate_aggregate(group_by, aggs, input, outer),
            Operator::KnnScan {
                table,
                column,
                query,
                ..
            } => self.validate_knn_scan(table, column, query, outer),
            Operator::WithinScan {
                table,
                column,
                meters,
                ..
            } => self.validate_within_scan(table, column, *meters, outer),
            Operator::HashJoin {
                on, left, right, ..
            } => self.validate_hash_join(on, left, right, outer),
        }
    }

    fn validate_scan(
        &self,
        table: &str,
        binding: &str,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        reject_outer_binding(binding, outer)?;
        let schema = self.node_table(table, "ScanNodes")?;
        let mut bindings = Bindings::new();
        bindings.bind_node(binding, schema);
        Ok(bindings)
    }

    fn validate_interface_scan(
        &self,
        interface: &str,
        binding: &str,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        reject_outer_binding(binding, outer)?;
        let schema = self.interface(interface, "ScanInterface")?;
        if let Some(table) = self
            .schema
            .node_tables()
            .iter()
            .find(|table| fold(table.name()) == fold(interface))
        {
            return Err(invalid_argument(
                "ScanInterface",
                format!(
                    "interface `{}` is shadowed by node table `{}` under folded name resolution",
                    schema.name(),
                    table.name()
                ),
            ));
        }
        let mut bindings = Bindings::new();
        bindings.bind_interface(binding, schema);
        Ok(bindings)
    }

    fn validate_expand(
        &self,
        rel: &str,
        direction: Direction,
        from_binding: &str,
        binding: &str,
        input: &Operator,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        let mut bindings = self.validate_operator(input, outer)?;
        let rel_schema = self.rel_table(rel, "Expand")?;
        let from_table = bindings
            .nodes
            .get(fold(from_binding).as_ref())
            .copied()
            .ok_or_else(|| {
                let mut candidates = bindings
                    .nodes
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>();
                candidates.sort_unstable();
                invalid_argument(
                    "Expand",
                    format!(
                        "from_binding `{from_binding}` is not in scope{}",
                        suggestion_suffix(from_binding, candidates)
                    ),
                )
            })?;
        let target_name = expand_target(rel_schema, direction, from_binding, from_table)?;
        if bindings.names.contains_key(fold(binding).as_ref()) {
            return Err(invalid_argument(
                "Expand",
                format!("binding `{binding}` is bound twice"),
            ));
        }
        reject_outer_binding(binding, outer)?;
        let target_table = self.node_table(target_name, "Expand")?;
        bindings.bind_node(binding, target_table);
        Ok(bindings)
    }

    fn validate_filter(
        &self,
        predicate: &Expr,
        input: &Operator,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        let bindings = self.validate_operator(input, outer)?;
        let predicate_type = self.typed_expression(predicate, &bindings, outer, "Filter")?;
        if !matches!(
            predicate_type,
            ExpressionType::Null | ExpressionType::Value(LogicalType::Bool)
        ) {
            return Err(invalid_argument(
                "Filter",
                format!(
                    "predicate must have type Bool; got {}",
                    predicate_type.output_type()
                ),
            ));
        }
        Ok(bindings)
    }

    fn validate_project(
        &self,
        exprs: &[crate::ops::ProjectionItem],
        input: &Operator,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        let input_bindings = self.validate_operator(input, outer)?;
        let mut columns = HashMap::new();
        let mut outputs = Vec::with_capacity(exprs.len());
        let mut aliases = HashSet::new();
        let mut owners = HashMap::new();
        for (index, item) in exprs.iter().enumerate() {
            let ty = self.typed_expression(&item.expr, &input_bindings, outer, "Project")?;
            outputs.push(ty.output_type());
            if !aliases.insert(fold(&item.alias).into_owned()) {
                return Err(invalid_argument(
                    "Project",
                    format!("duplicate output name `{}`", item.alias),
                ));
            }
            insert_project_reference(
                &mut columns,
                &mut owners,
                &item.alias,
                index,
                ty.output_type(),
            )?;
            if let Expr::Col(reference) = &item.expr {
                insert_project_reference(
                    &mut columns,
                    &mut owners,
                    reference,
                    index,
                    ty.output_type(),
                )?;
            }
        }
        Ok(Bindings {
            nodes: input_bindings.nodes,
            interfaces: input_bindings.interfaces,
            text_bindings: input_bindings.text_bindings,
            names: input_bindings.names,
            columns,
            outputs,
        })
    }

    fn validate_aggregate(
        &self,
        group_by: &[Expr],
        aggs: &[crate::ops::AggregateItem],
        input: &Operator,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        if group_by.is_empty() && aggs.is_empty() {
            return Err(invalid_argument(
                "Aggregate",
                "both group_by and aggs are empty",
            ));
        }
        let input_bindings = self.validate_operator(input, outer)?;
        let mut columns = HashMap::new();
        let mut outputs = Vec::with_capacity(group_by.len() + aggs.len());
        for expression in group_by {
            let ty = self.typed_expression(expression, &input_bindings, outer, "Aggregate")?;
            outputs.push(ty.output_type());
            let name = crate::text::printer::print_expression(expression)
                .map_err(|error| contextualize("Aggregate", error))?;
            let lookup_name = canonical_folded_expression(expression)?;
            insert_output_name(
                &mut columns,
                &lookup_name,
                &name,
                ty.output_type(),
                "Aggregate",
            )?;
        }
        for aggregate in aggs {
            let visible = visible_scope(&input_bindings, outer);
            validate_classof_bindings(&aggregate.expr, &visible, "Aggregate")?;
            let resolved = resolved_expression_columns(&aggregate.expr, &visible.columns);
            let mut scalar_type =
                |plan: &Operator| self.validate_scalar(plan, &input_bindings, outer);
            let ty = aggregate_type_with_subqueries(
                aggregate.function,
                &aggregate.expr,
                &resolved,
                &mut scalar_type,
            )
            .map_err(|error| contextualize("Aggregate", error))?;
            outputs.push(ty);
            insert_output_name(
                &mut columns,
                fold(&aggregate.alias).as_ref(),
                &aggregate.alias,
                ty,
                "Aggregate",
            )?;
        }
        Ok(Bindings {
            nodes: HashMap::new(),
            interfaces: HashSet::new(),
            text_bindings: HashSet::new(),
            names: HashMap::new(),
            columns,
            outputs,
        })
    }

    fn validate_knn_scan(
        &self,
        table: &str,
        column: &str,
        query: &KnnVectorSource,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        reject_outer_binding(table, outer)?;
        let table_schema = self.node_table(table, "KnnScan")?;
        let column_schema = table_schema
            .column_index(column)
            .and_then(|index| table_schema.columns().get(index))
            .ok_or_else(|| {
                invalid_argument(
                    "KnnScan",
                    format!("column `{table}.{column}` does not exist"),
                )
            })?;
        // `VectorEncoded` columns hold Vector(dim) values (value_type()),
        // so KNN admits both spellings of a vector column.
        let column_type = column_schema.ty.value_type();
        let LogicalType::Vector { dim } = column_type else {
            return Err(invalid_argument(
                "KnnScan",
                format!(
                    "column `{table}.{column}` has type {}; expected Vector",
                    column_schema.ty
                ),
            ));
        };
        // Unlike expression-position scalar subqueries, a KNN vector-source
        // scalar is uncorrelated in v1. Its inner operator still receives the
        // full scalar validation and typing rules, but no enclosing bindings
        // are visible.
        let empty = Bindings::new();
        let mut scalar_type = |plan: &Operator| self.validate_scalar(plan, &empty, &empty);
        let query_type = knn_vector_source_type(query, &mut scalar_type)
            .map_err(|error| contextualize("KnnScan query", error))?;
        if query_type != column_type {
            return Err(invalid_argument(
                "KnnScan",
                format!(
                    "query vector source has type {query_type}; column `{table}.{column}` has type Vector({dim})"
                ),
            ));
        }
        // KnnScan binds its table's columns under the table name so
        // downstream stages compose (PLAN_IR.md § knn semantics). The
        // trailing `distance` result column is output-only and never a
        // referenceable binding.
        let mut bindings = Bindings::new();
        bindings.bind_node(table, table_schema);
        bindings.outputs.push(LogicalType::Float64);
        Ok(bindings)
    }

    fn validate_within_scan(
        &self,
        table: &str,
        column: &str,
        meters: f64,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        reject_outer_binding(table, outer)?;
        let table_schema = self.node_table(table, "WithinScan")?;
        let column_schema = table_schema
            .column_index(column)
            .and_then(|index| table_schema.columns().get(index))
            .ok_or_else(|| {
                invalid_argument(
                    "WithinScan",
                    format!("column `{table}.{column}` does not exist"),
                )
            })?;
        if !matches!(column_schema.ty, LogicalType::GeoPoint) {
            return Err(invalid_argument(
                "WithinScan",
                format!(
                    "column `{table}.{column}` has type {}; expected GeoPoint",
                    column_schema.ty
                ),
            ));
        }
        if !meters.is_finite() || meters <= 0.0 {
            return Err(invalid_argument(
                "WithinScan",
                format!("radius must be a finite number of meters > 0, got {meters}"),
            ));
        }
        // WithinScan binds its table's columns under the table name so
        // downstream stages compose (PLAN_IR.md § within semantics).
        let mut bindings = Bindings::new();
        bindings.bind_node(table, table_schema);
        Ok(bindings)
    }

    fn validate_hash_join(
        &self,
        on: &[crate::ops::JoinKey],
        left: &Operator,
        right: &Operator,
        outer: &Bindings<'schema>,
    ) -> DevonResult<Bindings<'schema>> {
        let mut left_bindings = self.validate_operator(left, outer)?;
        let right_bindings = self.validate_operator(right, outer)?;
        reject_duplicate_join_binding(&left_bindings, &right_bindings)?;
        if on.is_empty() {
            return Err(invalid_argument(
                "HashJoin",
                "on must contain at least one equality key",
            ));
        }
        for key in on {
            self.validate_join_key(key, &left_bindings, &right_bindings, outer)?;
        }
        left_bindings.nodes.extend(right_bindings.nodes);
        left_bindings.interfaces.extend(right_bindings.interfaces);
        left_bindings
            .text_bindings
            .extend(right_bindings.text_bindings);
        left_bindings.names.extend(right_bindings.names);
        left_bindings.columns.extend(right_bindings.columns);
        left_bindings.outputs.extend(right_bindings.outputs);
        Ok(left_bindings)
    }

    fn validate_join_key(
        &self,
        key: &crate::ops::JoinKey,
        left: &Bindings<'schema>,
        right: &Bindings<'schema>,
        outer: &Bindings<'schema>,
    ) -> DevonResult<()> {
        let left_type = self.typed_expression(&key.left, left, outer, "HashJoin left key")?;
        let right_type = self.typed_expression(&key.right, right, outer, "HashJoin right key")?;
        reject_null_join_key("left", left_type)?;
        reject_null_join_key("right", right_type)?;
        reject_join_key_type(&key.left, left_type)?;
        reject_join_key_type(&key.right, right_type)?;
        if left_type != right_type {
            return Err(invalid_argument(
                "HashJoin",
                format!(
                    "equality key types must be exactly equal; got {} and {}",
                    join_key_type_name(left_type),
                    join_key_type_name(right_type)
                ),
            ));
        }
        Ok(())
    }

    fn typed_expression(
        &self,
        expression: &Expr,
        local: &Bindings<'schema>,
        outer: &Bindings<'schema>,
        operator: &str,
    ) -> DevonResult<ExpressionType> {
        let visible = visible_scope(local, outer);
        validate_classof_bindings(expression, &visible, operator)?;
        let resolved = resolved_expression_columns(expression, &visible.columns);
        let mut scalar_type = |plan: &Operator| self.validate_scalar(plan, local, outer);
        expression_type_with_subqueries(expression, &resolved, &mut scalar_type)
            .map_err(|error| contextualize(operator, error))
    }

    fn validate_scalar(
        &self,
        plan: &Operator,
        local: &Bindings<'schema>,
        outer: &Bindings<'schema>,
    ) -> DevonResult<ExpressionType> {
        let scalar_outer = visible_scope(local, outer);
        let output = self
            .validate_operator(plan, &scalar_outer)
            .map_err(|error| contextualize("scalar", error))?;
        let [ty] = output.outputs.as_slice() else {
            return Err(invalid_argument(
                "scalar",
                format!(
                    "scalar subquery must expose exactly one output column; got {}",
                    output.outputs.len()
                ),
            ));
        };
        Ok(ExpressionType::Value(*ty))
    }

    fn node_table(&self, name: &str, operator: &str) -> DevonResult<&'schema NodeTableSchema> {
        self.schema
            .node_tables()
            .iter()
            .find(|table| fold(table.name()) == fold(name))
            .ok_or_else(|| DevonError::NotFound {
                what: format!(
                    "node table `{name}` referenced by {operator}{}",
                    suggestion_suffix(
                        name,
                        self.schema.node_tables().iter().map(NodeTableSchema::name)
                    )
                ),
            })
    }

    fn rel_table(&self, name: &str, operator: &str) -> DevonResult<&'schema RelTableSchema> {
        self.schema
            .rel_tables()
            .iter()
            .find(|table| fold(table.name()) == fold(name))
            .ok_or_else(|| DevonError::NotFound {
                what: format!(
                    "relationship table `{name}` referenced by {operator}{}",
                    suggestion_suffix(
                        name,
                        self.schema.rel_tables().iter().map(RelTableSchema::name)
                    )
                ),
            })
    }

    fn interface(&self, name: &str, operator: &str) -> DevonResult<&'schema InterfaceSchema> {
        self.schema
            .interfaces()
            .iter()
            .find(|interface| fold(interface.name()) == fold(name))
            .ok_or_else(|| DevonError::NotFound {
                what: format!(
                    "interface `{name}` referenced by {operator}{}",
                    suggestion_suffix(
                        name,
                        self.schema.interfaces().iter().map(InterfaceSchema::name)
                    )
                ),
            })
    }
}

fn reject_duplicate_join_binding(left: &Bindings<'_>, right: &Bindings<'_>) -> DevonResult<()> {
    for binding in right.names.keys() {
        if left.names.contains_key(binding) {
            let display = right
                .names
                .get(binding)
                .map_or(binding.as_str(), String::as_str);
            return Err(invalid_argument(
                "HashJoin",
                format!("binding `{display}` is present in both inputs"),
            ));
        }
    }
    Ok(())
}

/// NULL join keys never match (`docs/PLAN_IR.md` § join semantics), so a
/// key pair whose type is `Null` on either side is a validation error
/// naming the side.
fn reject_null_join_key(side: &str, ty: ExpressionType) -> DevonResult<()> {
    if ty != ExpressionType::Null {
        return Ok(());
    }
    Err(invalid_argument(
        "HashJoin",
        format!("{side} join key has type Null; NULL join keys never match"),
    ))
}

fn reject_join_key_type(expression: &Expr, ty: ExpressionType) -> DevonResult<()> {
    let ExpressionType::Value(logical_type) = ty else {
        return Ok(());
    };
    if !matches!(
        logical_type,
        LogicalType::Vector { .. } | LogicalType::GeoPoint
    ) {
        return Ok(());
    }
    let column = first_column_reference(expression).unwrap_or("<literal>");
    Err(invalid_argument(
        "HashJoin",
        format!("column `{column}` has type {logical_type} and cannot be used as a join key"),
    ))
}

fn first_column_reference(expression: &Expr) -> Option<&str> {
    match expression {
        Expr::Col(reference) => Some(reference),
        Expr::ClassOf(_) | Expr::ScoreOf(_) => None,
        Expr::Binary { left, right, .. } | Expr::Distance { left, right, .. } => {
            first_column_reference(left).or_else(|| first_column_reference(right))
        }
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => first_column_reference(cond)
            .or_else(|| first_column_reference(then_expr))
            .or_else(|| first_column_reference(else_expr)),
        Expr::Coalesce(expressions) | Expr::Least(expressions) | Expr::Greatest(expressions) => {
            expressions.iter().find_map(first_column_reference)
        }
        Expr::DateTrunc { value, .. } => first_column_reference(value),
        Expr::DateAdd { value, amount, .. } => {
            first_column_reference(value).or_else(|| first_column_reference(amount))
        }
        Expr::Round { value, .. } => first_column_reference(value),
        Expr::RoundDiv {
            numerator,
            denominator,
            ..
        } => first_column_reference(numerator).or_else(|| first_column_reference(denominator)),
        Expr::Scalar { .. } => None,
        Expr::Not(operand) => first_column_reference(operand),
        Expr::Lit(_) => None,
    }
}

fn join_key_type_name(ty: ExpressionType) -> String {
    match ty {
        ExpressionType::Null => "Null".to_owned(),
        ExpressionType::Value(ty) => ty.to_string(),
    }
}

fn expand_target<'rel>(
    rel: &'rel RelTableSchema,
    direction: Direction,
    from_binding: &str,
    from_table: &NodeTableSchema,
) -> DevonResult<&'rel str> {
    let table = from_table.name();
    match direction {
        Direction::Out if fold(table) == fold(rel.from()) => Ok(rel.to()),
        Direction::In if fold(table) == fold(rel.to()) => Ok(rel.from()),
        Direction::Both if fold(table) == fold(rel.from()) => Ok(rel.to()),
        Direction::Both if fold(table) == fold(rel.to()) => Ok(rel.from()),
        _ => Err(invalid_argument(
            "Expand",
            format!(
                "direction `{}` on relationship `{}` does not accept from_binding `{from_binding}` from node table `{table}`",
                direction_name(direction),
                rel.name()
            ),
        )),
    }
}

fn visible_scope<'schema>(
    local: &Bindings<'schema>,
    outer: &Bindings<'schema>,
) -> Bindings<'schema> {
    let mut nodes = outer.nodes.clone();
    nodes.extend(
        local
            .nodes
            .iter()
            .map(|(name, schema)| (name.clone(), *schema)),
    );
    let mut names = outer.names.clone();
    names.extend(local.names.clone());
    let mut interfaces = outer.interfaces.clone();
    interfaces.extend(local.interfaces.iter().cloned());
    let mut columns = outer.columns.clone();
    columns.extend(local.columns.clone());
    Bindings {
        nodes,
        interfaces,
        text_bindings: outer
            .text_bindings
            .union(&local.text_bindings)
            .cloned()
            .collect(),
        names,
        columns,
        outputs: Vec::new(),
    }
}

fn validate_classof_bindings(
    expression: &Expr,
    bindings: &Bindings<'_>,
    operator: &str,
) -> DevonResult<()> {
    match expression {
        Expr::ScoreOf(binding) if !bindings.text_bindings.contains(fold(binding).as_ref()) => {
            Err(invalid_argument(
                operator,
                format!("scoreof binding `{binding}` is not a TextScan binding"),
            ))
        }
        Expr::ClassOf(binding) if !bindings.interfaces.contains(fold(binding).as_ref()) => {
            Err(invalid_argument(
                operator,
                format!("classof binding `{binding}` is not an interface-scan binding"),
            ))
        }
        Expr::Binary { left, right, .. } | Expr::Distance { left, right, .. } => {
            validate_classof_bindings(left, bindings, operator)?;
            validate_classof_bindings(right, bindings, operator)
        }
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            validate_classof_bindings(cond, bindings, operator)?;
            validate_classof_bindings(then_expr, bindings, operator)?;
            validate_classof_bindings(else_expr, bindings, operator)
        }
        Expr::Coalesce(expressions) | Expr::Least(expressions) | Expr::Greatest(expressions) => {
            for expression in expressions {
                validate_classof_bindings(expression, bindings, operator)?;
            }
            Ok(())
        }
        Expr::DateTrunc { value, .. } | Expr::Round { value, .. } | Expr::Not(value) => {
            validate_classof_bindings(value, bindings, operator)
        }
        Expr::DateAdd { value, amount, .. } => {
            validate_classof_bindings(value, bindings, operator)?;
            validate_classof_bindings(amount, bindings, operator)
        }
        Expr::RoundDiv {
            numerator,
            denominator,
            ..
        } => {
            validate_classof_bindings(numerator, bindings, operator)?;
            validate_classof_bindings(denominator, bindings, operator)
        }
        Expr::Col(_) | Expr::Lit(_) | Expr::ClassOf(_) | Expr::ScoreOf(_) | Expr::Scalar { .. } => {
            Ok(())
        }
    }
}

fn reject_unordered_sort_type(expression: &Expr, ty: ExpressionType) -> DevonResult<()> {
    let ExpressionType::Value(logical_type) = ty else {
        return Ok(());
    };
    // PLAN_IR § Type system: sort keys require an ordered scalar. Bytes and
    // Json admit equality only, Vector and GeoPoint are not scalar-comparable.
    if matches!(
        logical_type,
        LogicalType::Bool
            | LogicalType::Int64
            | LogicalType::Float64
            | LogicalType::String
            | LogicalType::Timestamp
            | LogicalType::Decimal { .. }
    ) {
        return Ok(());
    }
    Err(invalid_argument(
        "Sort",
        format!(
            "sort key `{}` has type {}; sort keys require an ordered scalar",
            crate::text::printer::print_expression(expression)
                .map_err(|error| contextualize("Sort", error))?,
            type_name(ty)
        ),
    ))
}

fn type_name(ty: ExpressionType) -> String {
    match ty {
        ExpressionType::Null => "Null".to_owned(),
        ExpressionType::Value(ty) => ty.to_string(),
    }
}

fn reject_outer_binding(binding: &str, outer: &Bindings<'_>) -> DevonResult<()> {
    if !outer.names.contains_key(fold(binding).as_ref()) {
        return Ok(());
    }
    Err(invalid_argument(
        "scalar",
        format!("inner binding `{binding}` collides with a visible outer binding"),
    ))
}

fn insert_output_name(
    columns: &mut HashMap<String, LogicalType>,
    lookup_name: &str,
    display_name: &str,
    ty: LogicalType,
    operator: &str,
) -> DevonResult<()> {
    if columns.contains_key(lookup_name) {
        return Err(invalid_argument(
            operator,
            format!("duplicate output name `{display_name}`"),
        ));
    }
    columns.insert(lookup_name.to_owned(), ty);
    Ok(())
}

fn insert_project_reference(
    columns: &mut HashMap<String, LogicalType>,
    owners: &mut HashMap<String, usize>,
    name: &str,
    index: usize,
    ty: LogicalType,
) -> DevonResult<()> {
    let lookup_name = fold(name);
    if owners
        .get(lookup_name.as_ref())
        .is_some_and(|owner| *owner != index)
    {
        return Err(invalid_argument(
            "Project",
            format!("ambiguous output reference `{name}`"),
        ));
    }
    owners.insert(lookup_name.clone().into_owned(), index);
    columns.insert(lookup_name.into_owned(), ty);
    Ok(())
}

fn resolved_expression_columns(
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
) -> HashMap<String, LogicalType> {
    let mut resolved = columns.clone();
    add_resolved_references(expression, columns, &mut resolved);
    resolved
}

fn add_resolved_references(
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
    resolved: &mut HashMap<String, LogicalType>,
) {
    match expression {
        Expr::Col(reference) => {
            if let Some(ty) = columns.get(fold(reference).as_ref()) {
                resolved.insert(reference.clone(), *ty);
            }
        }
        Expr::ClassOf(_) | Expr::ScoreOf(_) => {}
        Expr::Binary { left, right, .. } | Expr::Distance { left, right, .. } => {
            add_resolved_references(left, columns, resolved);
            add_resolved_references(right, columns, resolved);
        }
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            add_resolved_references(cond, columns, resolved);
            add_resolved_references(then_expr, columns, resolved);
            add_resolved_references(else_expr, columns, resolved);
        }
        Expr::Coalesce(expressions) | Expr::Least(expressions) | Expr::Greatest(expressions) => {
            for expression in expressions {
                add_resolved_references(expression, columns, resolved);
            }
        }
        Expr::DateTrunc { value, .. } => add_resolved_references(value, columns, resolved),
        Expr::DateAdd { value, amount, .. } => {
            add_resolved_references(value, columns, resolved);
            add_resolved_references(amount, columns, resolved);
        }
        Expr::Round { value, .. } => add_resolved_references(value, columns, resolved),
        Expr::RoundDiv {
            numerator,
            denominator,
            ..
        } => {
            add_resolved_references(numerator, columns, resolved);
            add_resolved_references(denominator, columns, resolved);
        }
        Expr::Scalar { .. } => {}
        Expr::Not(operand) => add_resolved_references(operand, columns, resolved),
        Expr::Lit(_) => {}
    }
}

fn canonical_folded_expression(expression: &Expr) -> DevonResult<String> {
    crate::text::printer::print_expression(&fold_expression_identifiers(expression))
}

fn fold_expression_identifiers(expression: &Expr) -> Expr {
    match expression {
        Expr::Col(reference) => Expr::Col(fold(reference).into_owned()),
        Expr::ClassOf(binding) => Expr::ClassOf(fold(binding).into_owned()),
        Expr::ScoreOf(binding) => Expr::ScoreOf(fold(binding).into_owned()),
        Expr::Lit(value) => Expr::Lit(value.clone()),
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(fold_expression_identifiers(left)),
            right: Box::new(fold_expression_identifiers(right)),
        },
        Expr::Not(operand) => Expr::Not(Box::new(fold_expression_identifiers(operand))),
        Expr::Distance {
            left,
            right,
            metric,
        } => Expr::Distance {
            left: Box::new(fold_expression_identifiers(left)),
            right: Box::new(fold_expression_identifiers(right)),
            metric: *metric,
        },
        Expr::If {
            cond,
            then_expr,
            else_expr,
        } => Expr::If {
            cond: Box::new(fold_expression_identifiers(cond)),
            then_expr: Box::new(fold_expression_identifiers(then_expr)),
            else_expr: Box::new(fold_expression_identifiers(else_expr)),
        },
        Expr::Coalesce(expressions) => Expr::Coalesce(fold_expressions(expressions)),
        Expr::Least(expressions) => Expr::Least(fold_expressions(expressions)),
        Expr::Greatest(expressions) => Expr::Greatest(fold_expressions(expressions)),
        Expr::DateTrunc { unit, value } => Expr::DateTrunc {
            unit: *unit,
            value: Box::new(fold_expression_identifiers(value)),
        },
        Expr::DateAdd {
            unit,
            value,
            amount,
        } => Expr::DateAdd {
            unit: *unit,
            value: Box::new(fold_expression_identifiers(value)),
            amount: Box::new(fold_expression_identifiers(amount)),
        },
        Expr::Round { value, places } => Expr::Round {
            value: Box::new(fold_expression_identifiers(value)),
            places: *places,
        },
        Expr::RoundDiv {
            numerator,
            denominator,
            places,
        } => Expr::RoundDiv {
            numerator: Box::new(fold_expression_identifiers(numerator)),
            denominator: Box::new(fold_expression_identifiers(denominator)),
            places: *places,
        },
        Expr::Scalar { plan } => Expr::Scalar { plan: plan.clone() },
    }
}

fn fold_expressions(expressions: &[Expr]) -> Vec<Expr> {
    expressions
        .iter()
        .map(fold_expression_identifiers)
        .collect()
}

fn contextualize(operator: &str, error: DevonError) -> DevonError {
    match error {
        DevonError::InvalidArgument { context } => invalid_argument(operator, context),
        other => other,
    }
}

fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Out => "out",
        Direction::In => "in",
        Direction::Both => "both",
    }
}

fn invalid_argument(operator: &str, problem: impl std::fmt::Display) -> DevonError {
    DevonError::InvalidArgument {
        context: format!("{operator}: {problem}"),
    }
}

#[cfg(test)]
mod tests {
    use devondb_types::logical_type::LogicalType;
    use devondb_types::schema::{Column, NodeTableSchema, RelTableSchema};
    use devondb_types::{DevonError, DevonResult, value::Value};

    use super::validate;
    use crate::expr::{BinaryOp, Expr, Metric};
    use crate::ops::{
        AggregateItem, Direction, JoinKey, JoinType, KnnMode, Operator, Plan, ProjectionItem,
    };

    fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
        Column {
            name: name.to_owned(),
            ty,
            primary_key,
        }
    }

    fn schemas() -> (Vec<NodeTableSchema>, Vec<RelTableSchema>) {
        let person = NodeTableSchema::new(
            "Person".to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
                column("age", LogicalType::Int64, false),
                column("active", LogicalType::Bool, false),
                column("embedding", LogicalType::Vector { dim: 3 }, false),
                column("location", LogicalType::GeoPoint, false),
            ],
        )
        .unwrap();
        let city = NodeTableSchema::new(
            "City".to_owned(),
            vec![
                column("id", LogicalType::Int64, true),
                column("name", LogicalType::String, false),
                column("score", LogicalType::Float64, false),
                column("embedding", LogicalType::Vector { dim: 3 }, false),
                column("location", LogicalType::GeoPoint, false),
            ],
        )
        .unwrap();
        let knows = RelTableSchema::new(
            "KNOWS".to_owned(),
            "Person".to_owned(),
            "Person".to_owned(),
            vec![],
        )
        .unwrap();
        let lives_in = RelTableSchema::new(
            "LIVES_IN".to_owned(),
            "Person".to_owned(),
            "City".to_owned(),
            vec![],
        )
        .unwrap();
        (vec![person, city], vec![knows, lives_in])
    }

    fn plan(operator: Operator) -> Plan {
        Plan {
            v: 0,
            plan: operator,
        }
    }

    fn scan(table: &str, binding: &str) -> Operator {
        Operator::ScanNodes {
            table: table.to_owned(),
            binding: binding.to_owned(),
        }
    }

    fn check(operator: Operator) -> DevonResult<()> {
        let (node_tables, rel_tables) = schemas();
        validate(&plan(operator), &node_tables, &rel_tables)
    }

    fn join(left: Operator, right: Operator, on: Vec<JoinKey>) -> Operator {
        Operator::HashJoin {
            join: JoinType::Inner,
            on,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    fn join_key(left: &str, right: &str) -> JoinKey {
        JoinKey {
            left: Expr::Col(left.into()),
            right: Expr::Col(right.into()),
        }
    }

    #[test]
    fn valid_multi_operator_plan_passes() {
        let operator = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col("FRIEND.NAME".to_owned()),
                alias: "friend_name".to_owned(),
            }],
            input: Box::new(Operator::Filter {
                predicate: Expr::Binary {
                    op: BinaryOp::Gt,
                    left: Box::new(Expr::Col("p.age".to_owned())),
                    right: Box::new(Expr::Lit(Value::Int64(30))),
                },
                input: Box::new(Operator::Expand {
                    rel: "knows".to_owned(),
                    direction: Direction::Out,
                    from_binding: "P".to_owned(),
                    binding: "friend".to_owned(),
                    input: Box::new(scan("person", "p")),
                }),
            }),
        };

        check(operator).unwrap();
    }

    #[test]
    fn unknown_scan_table_is_not_found() {
        assert_not_found(check(scan("Missing", "p")), &["Missing", "ScanNodes"]);
    }

    #[test]
    fn did_you_mean_unknown_node_table_uses_catalog_spelling() {
        let error = check(scan("Persn", "p")).unwrap_err();
        let DevonError::NotFound { what } = error else {
            panic!("expected NotFound, got {error}");
        };
        assert_eq!(
            what,
            "node table `Persn` referenced by ScanNodes (did you mean `Person`?)"
        );
    }

    #[test]
    fn did_you_mean_unknown_node_table_beyond_budget_is_unchanged() {
        let error = check(scan("zzzzzz", "p")).unwrap_err();
        let DevonError::NotFound { what } = error else {
            panic!("expected NotFound, got {error}");
        };
        assert_eq!(what, "node table `zzzzzz` referenced by ScanNodes");
    }

    #[test]
    fn did_you_mean_unknown_rel_table_uses_only_rel_candidates() {
        let operator = Operator::Expand {
            rel: "KNWOS".to_owned(),
            direction: Direction::Out,
            from_binding: "p".to_owned(),
            binding: "friend".to_owned(),
            input: Box::new(scan("Person", "p")),
        };
        let error = check(operator).unwrap_err();
        let DevonError::NotFound { what } = error else {
            panic!("expected NotFound, got {error}");
        };
        assert_eq!(
            what,
            "relationship table `KNWOS` referenced by Expand (did you mean `KNOWS`?)"
        );
    }

    #[test]
    fn unknown_column_in_filter_is_invalid() {
        let operator = Operator::Filter {
            predicate: Expr::Col("p.missing".to_owned()),
            input: Box::new(scan("Person", "p")),
        };

        assert_invalid(check(operator), &["Filter", "p.missing", "input schema"]);
    }

    #[test]
    fn did_you_mean_unknown_column_uses_sorted_resolved_keys() {
        let operator = Operator::Filter {
            predicate: Expr::Col("p.nme".to_owned()),
            input: Box::new(scan("Person", "p")),
        };
        let error = check(operator).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert_eq!(
            context,
            "Filter: column reference `p.nme` is not in the input schema (did you mean `p.name`?)"
        );
    }

    #[test]
    fn unknown_binding_in_project_is_invalid() {
        let operator = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col("other.name".to_owned()),
                alias: "name".to_owned(),
            }],
            input: Box::new(scan("Person", "p")),
        };

        assert_invalid(check(operator), &["Project", "other.name", "input schema"]);
    }

    #[test]
    fn duplicate_binding_is_invalid() {
        let operator = Operator::Expand {
            rel: "KNOWS".to_owned(),
            direction: Direction::Out,
            from_binding: "p".to_owned(),
            binding: "P".to_owned(),
            input: Box::new(scan("Person", "p")),
        };

        assert_invalid(check(operator), &["Expand", "P", "bound twice"]);
    }

    #[test]
    fn expand_from_unbound_binding_is_invalid() {
        let operator = Operator::Expand {
            rel: "KNOWS".to_owned(),
            direction: Direction::Out,
            from_binding: "missing".to_owned(),
            binding: "friend".to_owned(),
            input: Box::new(scan("Person", "p")),
        };

        assert_invalid(check(operator), &["Expand", "missing", "not in scope"]);
    }

    #[test]
    fn did_you_mean_expand_binding_uses_folded_in_scope_spelling() {
        let operator = Operator::Expand {
            rel: "KNOWS".to_owned(),
            direction: Direction::Out,
            from_binding: "personbindin".to_owned(),
            binding: "friend".to_owned(),
            input: Box::new(scan("Person", "PersonBinding")),
        };
        let error = check(operator).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert_eq!(
            context,
            "Expand: from_binding `personbindin` is not in scope (did you mean `personbinding`?)"
        );
    }

    #[test]
    fn did_you_mean_is_not_added_to_bound_twice_error() {
        let operator = Operator::Expand {
            rel: "KNOWS".to_owned(),
            direction: Direction::Out,
            from_binding: "p".to_owned(),
            binding: "P".to_owned(),
            input: Box::new(scan("Person", "p")),
        };
        let error = check(operator).unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        assert_eq!(context, "Expand: binding `P` is bound twice");
    }

    #[test]
    fn expand_direction_must_match_bound_table() {
        let out = Operator::Expand {
            rel: "LIVES_IN".to_owned(),
            direction: Direction::Out,
            from_binding: "c".to_owned(),
            binding: "p".to_owned(),
            input: Box::new(scan("City", "c")),
        };
        assert_invalid(check(out), &["Expand", "out", "LIVES_IN", "City"]);

        let incoming = Operator::Expand {
            rel: "LIVES_IN".to_owned(),
            direction: Direction::In,
            from_binding: "c".to_owned(),
            binding: "p".to_owned(),
            input: Box::new(scan("City", "c")),
        };
        check(incoming).unwrap();
    }

    #[test]
    fn knn_scan_rejects_non_vector_column() {
        let operator = Operator::KnnScan {
            table: "Person".to_owned(),
            column: "name".to_owned(),
            query: vec![0.0, 1.0, 2.0].into(),
            k: 5,
            metric: Metric::Cosine,
            mode: KnnMode::Exact,
        };

        assert_invalid(check(operator), &["KnnScan", "name", "String", "Vector"]);
    }

    #[test]
    fn knn_scan_rejects_dimension_mismatch() {
        let operator = Operator::KnnScan {
            table: "Person".to_owned(),
            column: "embedding".to_owned(),
            query: vec![0.0, 1.0].into(),
            k: 5,
            metric: Metric::Cosine,
            mode: KnnMode::Exact,
        };

        assert_invalid(check(operator), &["KnnScan", "2", "3", "embedding"]);
    }

    #[test]
    fn empty_aggregate_is_invalid() {
        let operator = Operator::Aggregate {
            group_by: vec![],
            aggs: vec![],
            input: Box::new(scan("Person", "p")),
        };

        assert_invalid(check(operator), &["Aggregate", "group_by", "aggs"]);
    }

    #[test]
    fn zero_limit_is_valid() {
        let operator = Operator::Limit {
            count: 0,
            offset: None,
            input: Box::new(scan("Person", "p")),
        };

        check(operator).unwrap();
    }

    #[test]
    fn hash_join_accepts_inner_left_multi_key_and_expression_keys() {
        let keys = vec![
            join_key("p.id", "c.id"),
            join_key("p.name", "c.name"),
            JoinKey {
                left: Expr::Binary {
                    op: BinaryOp::Add,
                    left: Box::new(Expr::Col("p.age".into())),
                    right: Box::new(Expr::Lit(Value::Int64(1))),
                },
                right: Expr::Col("c.id".into()),
            },
        ];
        check(join(scan("Person", "p"), scan("City", "c"), keys)).unwrap();

        let mut left = join(
            scan("Person", "p"),
            scan("City", "c"),
            vec![join_key("p.id", "c.id")],
        );
        let Operator::HashJoin { join, .. } = &mut left else {
            panic!("expected hash join");
        };
        *join = JoinType::Left;
        check(left).unwrap();
    }

    #[test]
    fn hash_join_validates_both_children_and_requires_nonempty_on() {
        let invalid_right = join(
            scan("Person", "p"),
            scan("Missing", "m"),
            vec![join_key("p.id", "m.id")],
        );
        assert_not_found(check(invalid_right), &["Missing", "ScanNodes"]);

        let empty = join(scan("Person", "p"), scan("City", "c"), vec![]);
        assert_invalid(check(empty), &["HashJoin", "on", "at least one"]);
    }

    #[test]
    fn hash_join_rejects_duplicate_bindings_and_wrong_side_references() {
        let duplicate = join(
            scan("Person", "p"),
            scan("City", "P"),
            vec![join_key("p.id", "P.id")],
        );
        assert_invalid(
            check(duplicate),
            &["HashJoin", "binding `P`", "both inputs"],
        );

        let wrong_left = join(
            scan("Person", "p"),
            scan("City", "c"),
            vec![join_key("c.id", "p.id")],
        );
        assert_invalid(
            check(wrong_left),
            &["HashJoin left key", "c.id", "input schema"],
        );

        let wrong_right = join(
            scan("Person", "p"),
            scan("City", "c"),
            vec![join_key("p.id", "p.id")],
        );
        assert_invalid(
            check(wrong_right),
            &["HashJoin right key", "p.id", "input schema"],
        );
    }

    #[test]
    fn hash_join_requires_exact_equal_key_types() {
        for (left, right, expected) in [
            ("p.name", "c.id", ["String", "Int64"]),
            ("p.id", "c.score", ["Int64", "Float64"]),
        ] {
            let operator = join(
                scan("Person", "p"),
                scan("City", "c"),
                vec![join_key(left, right)],
            );
            assert_invalid(
                check(operator),
                &["HashJoin", "exactly equal", expected[0], expected[1]],
            );
        }
    }

    #[test]
    fn hash_join_rejects_vector_and_geopoint_keys_naming_the_column() {
        for (left, right, ty) in [
            ("p.embedding", "c.embedding", "Vector"),
            ("p.location", "c.location", "GeoPoint"),
        ] {
            let operator = join(
                scan("Person", "p"),
                scan("City", "c"),
                vec![join_key(left, right)],
            );
            assert_invalid(check(operator), &["HashJoin", left, ty, "join key"]);
        }
    }

    #[test]
    fn column_reference_without_dot_is_invalid() {
        let operator = Operator::Aggregate {
            group_by: vec![Expr::Col("name".to_owned())],
            aggs: vec![AggregateItem {
                function: crate::ops::AggregateFunction::Count,
                expr: Expr::Col("p.id".to_owned()),
                alias: "count".to_owned(),
            }],
            input: Box::new(scan("Person", "p")),
        };

        assert_invalid(check(operator), &["Aggregate", "name", "binding.column"]);
    }

    #[test]
    fn expression_operand_types_are_checked() {
        let invalid_expressions = [
            Expr::Binary {
                op: BinaryOp::Add,
                left: Box::new(Expr::Col("p.name".into())),
                right: Box::new(Expr::Col("p.name".into())),
            },
            Expr::Binary {
                op: BinaryOp::And,
                left: Box::new(Expr::Col("p.age".into())),
                right: Box::new(Expr::Col("p.active".into())),
            },
            Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Col("p.name".into())),
                right: Box::new(Expr::Col("p.age".into())),
            },
            Expr::Distance {
                left: Box::new(Expr::Col("p.embedding".into())),
                right: Box::new(Expr::Lit(Value::Vector(vec![0.0; 2]))),
                metric: Metric::L2,
            },
        ];

        for expression in invalid_expressions {
            let operator = Operator::Project {
                exprs: vec![ProjectionItem {
                    expr: expression,
                    alias: "result".into(),
                }],
                input: Box::new(scan("Person", "p")),
            };
            assert_invalid(check(operator), &["Project", "operator"]);
        }
    }

    #[test]
    fn filter_requires_a_boolean_or_null_predicate() {
        let invalid = Operator::Filter {
            predicate: Expr::Col("p.age".into()),
            input: Box::new(scan("Person", "p")),
        };
        assert_invalid(check(invalid), &["Filter", "Bool", "Int64"]);

        let null = Operator::Filter {
            predicate: Expr::Lit(Value::Null),
            input: Box::new(scan("Person", "p")),
        };
        check(null).unwrap();
    }

    #[test]
    fn sum_and_avg_require_numeric_operands() {
        for (function, column) in [
            (crate::ops::AggregateFunction::Sum, "p.name"),
            (crate::ops::AggregateFunction::Avg, "p.active"),
        ] {
            let operator = Operator::Aggregate {
                group_by: vec![],
                aggs: vec![AggregateItem {
                    function,
                    expr: Expr::Col(column.into()),
                    alias: "result".into(),
                }],
                input: Box::new(scan("Person", "p")),
            };
            assert_invalid(check(operator), &["Aggregate", "numeric"]);
        }
    }

    #[test]
    fn null_literals_are_admitted_as_values() {
        let projection = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Lit(Value::Null),
                alias: "q.nothing".into(),
            }],
            input: Box::new(scan("Person", "p")),
        };
        let aggregate = Operator::Aggregate {
            group_by: vec![],
            aggs: vec![
                AggregateItem {
                    function: crate::ops::AggregateFunction::Sum,
                    expr: Expr::Lit(Value::Null),
                    alias: "q.sum".into(),
                },
                AggregateItem {
                    function: crate::ops::AggregateFunction::Avg,
                    expr: Expr::Lit(Value::Null),
                    alias: "q.avg".into(),
                },
            ],
            input: Box::new(projection),
        };

        check(aggregate).unwrap();
    }

    #[test]
    fn projection_aliases_are_referenceable_downstream() {
        let projection = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col("p.name".into()),
                alias: "q.name".into(),
            }],
            input: Box::new(scan("Person", "p")),
        };
        let filter = Operator::Filter {
            predicate: Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(Expr::Col("q.name".into())),
                right: Box::new(Expr::Lit(Value::String("Ada".into()))),
            },
            input: Box::new(projection),
        };

        check(filter).unwrap();
    }

    #[test]
    fn projected_bare_columns_retain_their_source_reference() {
        let projection = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col("p.age".into()),
                alias: "age".into(),
            }],
            input: Box::new(scan("Person", "p")),
        };
        let sort = Operator::Sort {
            keys: vec![crate::ops::SortKey {
                expr: Expr::Col("p.age".into()),
                order: crate::ops::SortOrder::Asc,
            }],
            input: Box::new(projection),
        };

        check(sort).unwrap();
    }

    #[test]
    fn aggregate_aliases_are_referenceable_downstream() {
        let aggregate = Operator::Aggregate {
            group_by: vec![],
            aggs: vec![AggregateItem {
                function: crate::ops::AggregateFunction::Count,
                expr: Expr::Col("p.id".into()),
                alias: "q.count".into(),
            }],
            input: Box::new(scan("Person", "p")),
        };
        let filter = Operator::Filter {
            predicate: Expr::Binary {
                op: BinaryOp::Gt,
                left: Box::new(Expr::Col("q.count".into())),
                right: Box::new(Expr::Lit(Value::Int64(0))),
            },
            input: Box::new(aggregate),
        };

        check(filter).unwrap();
    }

    #[test]
    fn duplicate_output_names_are_invalid() {
        let projection = Operator::Project {
            exprs: vec![
                ProjectionItem {
                    expr: Expr::Col("p.name".into()),
                    alias: "q.value".into(),
                },
                ProjectionItem {
                    expr: Expr::Col("p.age".into()),
                    alias: "Q.VALUE".into(),
                },
            ],
            input: Box::new(scan("Person", "p")),
        };
        assert_invalid(check(projection), &["Project", "duplicate", "Q.VALUE"]);

        let ambiguous_projection = Operator::Project {
            exprs: vec![
                ProjectionItem {
                    expr: Expr::Col("p.name".into()),
                    alias: "q.name".into(),
                },
                ProjectionItem {
                    expr: Expr::Col("p.age".into()),
                    alias: "P.NAME".into(),
                },
            ],
            input: Box::new(scan("Person", "p")),
        };
        assert_invalid(
            check(ambiguous_projection),
            &["Project", "ambiguous", "P.NAME"],
        );

        let aggregate = Operator::Aggregate {
            group_by: vec![Expr::Col("p.age".into())],
            aggs: vec![AggregateItem {
                function: crate::ops::AggregateFunction::Count,
                expr: Expr::Col("p.id".into()),
                alias: "p.age".into(),
            }],
            input: Box::new(scan("Person", "p")),
        };
        assert_invalid(check(aggregate), &["Aggregate", "duplicate", "p.age"]);

        let duplicate_groups = Operator::Aggregate {
            group_by: vec![Expr::Col("p.age".into()), Expr::Col("p.age".into())],
            aggs: vec![],
            input: Box::new(scan("Person", "p")),
        };
        assert_invalid(
            check(duplicate_groups),
            &["Aggregate", "duplicate", "p.age"],
        );

        let duplicate_aggregates = Operator::Aggregate {
            group_by: vec![],
            aggs: vec![
                AggregateItem {
                    function: crate::ops::AggregateFunction::Count,
                    expr: Expr::Col("p.id".into()),
                    alias: "q.count".into(),
                },
                AggregateItem {
                    function: crate::ops::AggregateFunction::Count,
                    expr: Expr::Col("p.age".into()),
                    alias: "q.count".into(),
                },
            ],
            input: Box::new(scan("Person", "p")),
        };
        assert_invalid(
            check(duplicate_aggregates),
            &["Aggregate", "duplicate", "q.count"],
        );
    }

    fn assert_invalid(result: DevonResult<()>, expected: &[&str]) {
        let error = result.unwrap_err();
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        for fragment in expected {
            assert!(
                context.contains(fragment),
                "expected `{context}` to contain `{fragment}`"
            );
        }
    }

    fn assert_not_found(result: DevonResult<()>, expected: &[&str]) {
        let error = result.unwrap_err();
        let DevonError::NotFound { what } = error else {
            panic!("expected NotFound, got {error}");
        };
        for fragment in expected {
            assert!(
                what.contains(fragment),
                "expected `{what}` to contain `{fragment}`"
            );
        }
    }

    #[test]
    fn sort_rejects_non_ordered_scalar_keys_naming_type_and_operator() {
        for (column, ty) in [("p.embedding", "Vector"), ("p.location", "GeoPoint")] {
            let sort = Operator::Sort {
                keys: vec![crate::ops::SortKey {
                    expr: Expr::Col(column.to_owned()),
                    order: crate::ops::SortOrder::Asc,
                }],
                input: Box::new(scan("Person", "p")),
            };
            assert_invalid(check(sort), &["Sort", column, ty, "ordered scalar"]);
        }
        // Bytes and Json admit equality only (PLAN_IR § Type system), so
        // they are not ordered scalars either.
        for (literal, ty) in [
            (Expr::Lit(Value::Bytes(vec![0x00])), "Bytes"),
            (Expr::Lit(Value::Json("{}".to_owned())), "Json"),
        ] {
            let sort = Operator::Sort {
                keys: vec![crate::ops::SortKey {
                    expr: literal,
                    order: crate::ops::SortOrder::Asc,
                }],
                input: Box::new(scan("Person", "p")),
            };
            assert_invalid(check(sort), &["Sort", ty, "ordered scalar"]);
        }
    }

    #[test]
    fn hash_join_rejects_null_keys_naming_the_side() {
        let null_left = join(
            scan("Person", "p"),
            scan("City", "c"),
            vec![JoinKey {
                left: Expr::Lit(Value::Null),
                right: Expr::Col("c.id".into()),
            }],
        );
        assert_invalid(
            check(null_left),
            &["HashJoin", "left", "Null", "never match"],
        );

        let null_right = join(
            scan("Person", "p"),
            scan("City", "c"),
            vec![JoinKey {
                left: Expr::Col("p.id".into()),
                right: Expr::Lit(Value::Null),
            }],
        );
        assert_invalid(
            check(null_right),
            &["HashJoin", "right", "Null", "never match"],
        );
    }
}
