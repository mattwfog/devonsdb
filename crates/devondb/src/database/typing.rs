use super::*;

use std::collections::BTreeSet;

use devondb_plan::validate::{
    aggregate_output_type_with_schema, expression_output_type_with_schema, validate_with_schema,
};
use devondb_plan::{
    statement::InterfaceColumn as PlanInterfaceColumn,
    typing::{ExpressionType, InterfaceSchema, SchemaInput},
};
use devondb_types::schema::fold;

// `database::mod` historically imports these plan vocabulary items for this
// child module. Keep those imports intentionally consumed without duplicating
// the canonical typing logic that now lives in `devondb-plan`.
const _: (Option<BinaryOp>, Option<AggregateFunction>, u32) = (None, None, PLAN_VERSION);

pub(super) fn projection_types(
    exprs: &[ProjectionItem],
    columns: &HashMap<String, LogicalType>,
    catalog: &Catalog,
) -> DevonResult<Vec<LogicalType>> {
    exprs
        .iter()
        .map(|item| expression_type(&item.expr, columns, catalog).map(ExpressionType::output_type))
        .collect()
}

pub(super) fn aggregate_types(
    group_by: &[Expr],
    aggs: &[AggregateItem],
    columns: &HashMap<String, LogicalType>,
    catalog: &Catalog,
) -> DevonResult<Vec<LogicalType>> {
    let mut output = group_by
        .iter()
        .map(|expression| {
            expression_type(expression, columns, catalog).map(ExpressionType::output_type)
        })
        .collect::<DevonResult<Vec<_>>>()?;
    let interfaces = interface_schemas(catalog);
    let schema = SchemaInput::new(catalog.node_tables(), catalog.rel_tables(), &interfaces);
    for aggregate in aggs {
        output.push(aggregate_output_type_with_schema(
            aggregate.function,
            &aggregate.expr,
            columns,
            &schema,
        )?);
    }
    Ok(output)
}

pub(super) fn validate_plan(plan: &Plan, catalog: &Catalog) -> DevonResult<()> {
    let interfaces = interface_schemas(catalog);
    validate_with_schema(
        plan,
        &SchemaInput::new(catalog.node_tables(), catalog.rel_tables(), &interfaces),
    )
}

pub(super) fn expression_type(
    expression: &Expr,
    columns: &HashMap<String, LogicalType>,
    catalog: &Catalog,
) -> DevonResult<ExpressionType> {
    let interfaces = interface_schemas(catalog);
    expression_output_type_with_schema(
        expression,
        columns,
        &SchemaInput::new(catalog.node_tables(), catalog.rel_tables(), &interfaces),
    )
}

fn interface_schemas(catalog: &Catalog) -> Vec<InterfaceSchema> {
    catalog.ontology().map_or_else(Vec::new, |ontology| {
        ontology
            .interfaces
            .iter()
            .map(|interface| {
                InterfaceSchema::new(
                    interface.name.clone(),
                    interface
                        .columns
                        .iter()
                        .map(|column| PlanInterfaceColumn {
                            name: column.name.clone(),
                            ty: column.ty,
                        })
                        .collect(),
                )
            })
            .collect()
    })
}

/// The binding-map construction site for node-table scans.
///
/// Under projection pushdown the scan chunk carries only the referenced
/// table columns (plus the trailing hidden offset the caller appends), so
/// every `binding.column` index is remapped through the referenced set —
/// here, once, so the scan source and every downstream consumer agree.
/// `projection == None` (or a set covering the whole schema) mints the
/// identity layout: declaration-order indices over every column.
pub(super) struct ScanBinding {
    /// Folded `binding.column` -> physical chunk index.
    pub(super) columns: HashMap<String, usize>,
    /// Folded `binding.column` -> logical type.
    pub(super) types: HashMap<String, LogicalType>,
    /// Table column indices in chunk order (ascending table order).
    pub(super) indices: Vec<usize>,
}

pub(super) fn scan_binding(
    schema: &NodeTableSchema,
    binding: &str,
    projection: Option<&BTreeSet<usize>>,
) -> DevonResult<ScanBinding> {
    let binding_key = fold(binding).into_owned();
    let indices: Vec<usize> = projection.map_or_else(
        || (0..schema.columns().len()).collect(),
        |set| set.iter().copied().collect(),
    );
    let mut columns = HashMap::with_capacity(indices.len());
    let mut types = HashMap::with_capacity(indices.len());
    for (position, index) in indices.iter().enumerate() {
        let column = schema.columns().get(*index).ok_or_else(|| {
            corrupt(format!(
                "scan projection for `{}` names out-of-range column {index}",
                schema.name()
            ))
        })?;
        let name = format!("{binding_key}.{}", fold(&column.name));
        columns.insert(name.clone(), position);
        types.insert(name, column.ty);
    }
    Ok(ScanBinding {
        columns,
        types,
        indices,
    })
}

pub(super) fn projected_metadata(
    exprs: &[ProjectionItem],
    output_types: &[LogicalType],
) -> (HashMap<String, usize>, HashMap<String, LogicalType>) {
    let mut columns = HashMap::new();
    let mut types = HashMap::new();
    for (index, (item, output_type)) in exprs.iter().zip(output_types).enumerate() {
        columns.insert(item.alias.clone(), index);
        types.insert(item.alias.clone(), *output_type);
        if let Expr::Col(reference) = &item.expr {
            columns.insert(reference.clone(), index);
            types.insert(reference.clone(), *output_type);
        }
    }
    (columns, types)
}

pub(super) fn aggregate_metadata(
    group_by: &[Expr],
    aggs: &[AggregateItem],
    output_types: &[LogicalType],
) -> DevonResult<(HashMap<String, usize>, HashMap<String, LogicalType>)> {
    let mut names = group_by
        .iter()
        .map(canonical_expression)
        .collect::<DevonResult<Vec<_>>>()?;
    names.extend(aggs.iter().map(|aggregate| aggregate.alias.clone()));
    let mut columns = HashMap::new();
    let mut types = HashMap::new();
    for (index, (name, output_type)) in names.into_iter().zip(output_types).enumerate() {
        columns.insert(name.clone(), index);
        types.insert(name, *output_type);
    }
    Ok((columns, types))
}

pub(super) fn canonical_expression(expression: &Expr) -> DevonResult<String> {
    devondb_plan::text::printer::print_expression(expression)
}

#[cfg(test)]
mod tests {
    use devondb_plan::expr::{BinaryOp, Expr};
    use devondb_plan::ops::ProjectionItem;
    use devondb_types::logical_type::LogicalType;

    use super::{canonical_expression, projected_metadata};

    #[test]
    fn projected_metadata_uses_alias_and_source_reference() {
        let expressions = [ProjectionItem {
            expr: Expr::Col("p.name".into()),
            alias: "q.name".into(),
        }];

        let (columns, types) = projected_metadata(&expressions, &[LogicalType::String]);

        assert_eq!(columns.get("q.name"), Some(&0));
        assert_eq!(types.get("q.name"), Some(&LogicalType::String));
        assert_eq!(columns.get("p.name"), Some(&0));
        assert_eq!(types.get("p.name"), Some(&LogicalType::String));
    }

    #[test]
    fn canonical_expression_calls_the_expression_printer_directly() {
        let expression = Expr::Binary {
            op: BinaryOp::Sub,
            left: Box::new(Expr::Col("p.a".into())),
            right: Box::new(Expr::Binary {
                op: BinaryOp::Sub,
                left: Box::new(Expr::Col("p.b".into())),
                right: Box::new(Expr::Col("p.c".into())),
            }),
        };

        assert_eq!(
            canonical_expression(&expression).unwrap(),
            "p.a - (p.b - p.c)"
        );
    }
}
