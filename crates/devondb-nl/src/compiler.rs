//! The deterministic normalize → match → ground → emit pipeline.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use devondb::text::parser::{Parsed, parse};
use devondb::{
    ColumnSummary, Database, NodeTableSummary, RelTableSummary, SchemaSummary, did_you_mean, fold,
};

use crate::ground::{
    GroundFailure, GroundIssue, GroundedHop, GroundedIntent, GroundedStatement, GroundedTimePhrase,
    MultiHopCap, Predicate, catalog_targets, ground, ground_statement, multi_hop_cap,
    relationship_spans,
};
use crate::normalize::{Token, TokenKind, normalize};
use crate::syntax::{column_ref, identifier, quote_string};
use crate::template::{
    AggregateFunction, Direction, IntentTokens, STATEMENT_TEMPLATES, SortDirection,
    StatementFamily, StatementIntentTokens, TEMPLATES, matches, statement_matches,
};
use crate::vocabulary::{all_words, is_action_list, is_vocabulary};
use crate::{
    Compiled, CompiledStatement, Grounded, IntentCompiler, NoParse, TemplateHint, Ungrounded,
};

/// The deterministic, vocabulary-driven intent compiler (`docs/NL.md`).
#[derive(Debug, Clone, Copy, Default)]
pub struct DeterministicCompiler;

impl IntentCompiler for DeterministicCompiler {
    fn compile(&self, question: &str, schema: &SchemaSummary) -> Compiled {
        compile_inner(question, schema, None, None)
    }

    fn compile_at(&self, question: &str, schema: &SchemaSummary, reference_date: i64) -> Compiled {
        compile_inner(question, schema, None, Some(reference_date))
    }

    fn compile_with_database(&self, question: &str, database: &mut Database) -> Compiled {
        let schema = database.schema_summary();
        compile_inner(question, &schema, Some(database), None)
    }

    fn compile_at_with_database(
        &self,
        question: &str,
        database: &mut Database,
        reference_date: i64,
    ) -> Compiled {
        let schema = database.schema_summary();
        compile_inner(question, &schema, Some(database), Some(reference_date))
    }

    fn compile_statement(&self, input: &str, schema: &SchemaSummary) -> CompiledStatement {
        compile_statement_inner(input, schema)
    }
}

fn compile_statement_inner(input: &str, schema: &SchemaSummary) -> CompiledStatement {
    let tokens = normalize(input);
    let candidates = statement_matches(&tokens);
    let mut failures = Vec::new();
    for (template, intent) in &candidates {
        match ground_statement(intent, &tokens, schema) {
            Ok(grounded) => {
                if let Some(statement) = emit_statement(&grounded) {
                    return CompiledStatement::Statement(statement);
                }
            }
            Err(failure) => failures.push((*template, failure)),
        }
    }
    CompiledStatement::NoParse(statement_refusal(&tokens, schema, &candidates, &failures))
}

fn emit_statement(intent: &GroundedStatement) -> Option<devondb::Statement> {
    let text = match intent {
        GroundedStatement::Update {
            table,
            key_column,
            key,
            column,
            value,
        } => format!(
            "update {} set {} = {value} where {} = {key}",
            identifier(table),
            identifier(column),
            identifier(key_column)
        ),
        GroundedStatement::Delete {
            table,
            key_column,
            key,
        } => format!(
            "delete from {} where {} = {key}",
            identifier(table),
            identifier(key_column)
        ),
    };
    match parse(&text).ok()? {
        Parsed::Statement(envelope) => Some(envelope.stmt),
        Parsed::Query(_) => None,
    }
}

pub(crate) fn statement_opt_out(input: &str) -> NoParse {
    let tokens = normalize(input);
    NoParse {
        recognized: Vec::new(),
        unrecognized: tokens
            .into_iter()
            .map(|token| Ungrounded {
                token: token.original,
                suggestion: None,
            })
            .collect(),
        nearest: STATEMENT_TEMPLATES
            .iter()
            .take(3)
            .map(|template| TemplateHint {
                example: template.example.to_owned(),
            })
            .collect(),
    }
}

fn compile_inner(
    question: &str,
    schema: &SchemaSummary,
    mut database: Option<&mut Database>,
    reference_date: Option<i64>,
) -> Compiled {
    let tokens = normalize(question);
    match compile_edge_case(&tokens, schema) {
        EdgeCompile::Plan(plan) => return Compiled::Plan(plan),
        EdgeCompile::Refusal(report) => return Compiled::NoParse(report),
        EdgeCompile::NoMatch => {}
    }
    let rel_targets = relationship_spans(&tokens, schema);
    let mut candidates = matches(&tokens);
    candidates.retain(|(_, intent)| {
        intent
            .multi_hop_rels()
            .is_none_or(|rels| rels.clone().any(|index| rel_targets.contains_key(&index)))
    });
    if let Some(cap) = candidates
        .iter()
        .find_map(|(_, intent)| multi_hop_cap(intent, &tokens, schema))
    {
        return Compiled::NoParse(multi_hop_cap_refusal(&tokens, schema, &cap));
    }
    let mut failures = Vec::new();
    // Template order is semantic: for a bare token, the table candidate
    // runs before the open-ended entity-literal candidate.
    for (template, intent) in &candidates {
        let result = match database.as_deref_mut() {
            Some(database) => ground(intent, &tokens, schema, Some(database)),
            None => ground(intent, &tokens, schema, None),
        };
        match result {
            Ok(grounded) => match emit(&grounded, reference_date) {
                Ok(Some(plan)) => return Compiled::Plan(plan),
                Ok(None) => {}
                Err(failure) => failures.push((*template, failure)),
            },
            Err(failure) => failures.push((*template, failure)),
        }
    }
    Compiled::NoParse(refusal(&tokens, schema, &candidates, &failures))
}

enum EdgeCompile {
    NoMatch,
    Plan(devondb::Plan),
    Refusal(NoParse),
}

struct EdgeFailure {
    failure: GroundFailure,
    literal_slots: BTreeSet<usize>,
    hint: String,
}

fn compile_edge_case(tokens: &[Token], schema: &SchemaSummary) -> EdgeCompile {
    let Some((trigger, result)) = edge_case_text(tokens, schema) else {
        return EdgeCompile::NoMatch;
    };
    match result {
        Ok(text) => match parse(&text).ok() {
            Some(Parsed::Query(plan)) => EdgeCompile::Plan(plan),
            Some(Parsed::Statement(_)) | None => EdgeCompile::Refusal(edge_refusal(
                tokens,
                schema,
                edge_failure(trigger, "edge-case plan could not be emitted", "<table>"),
            )),
        },
        Err(failure) => EdgeCompile::Refusal(edge_refusal(tokens, schema, failure)),
    }
}

fn edge_case_text(
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Option<(usize, Result<String, EdgeFailure>)> {
    if let Some(index) = token_pair(tokens, "with", "no") {
        return Some((index, edge_without_relation(tokens, schema, index)));
    }
    if let Some(index) = word_index(tokens, "without") {
        return Some((index, edge_without_property(tokens, schema, index)));
    }
    if let Some(index) = token_pair(tokens, "not", "in") {
        return Some((index, edge_not_in(tokens, schema, index)));
    }
    if let Some(index) = word_index(tokens, "between") {
        return Some((index, edge_between(tokens, schema, index)));
    }
    month_range_trigger(tokens).map(|index| (index, edge_month_range(tokens, schema, index)))
}

fn token_pair(tokens: &[Token], first: &str, second: &str) -> Option<usize> {
    tokens
        .windows(2)
        .position(|pair| pair[0].is_word(first) && pair[1].is_word(second))
}

fn word_index(tokens: &[Token], word: &str) -> Option<usize> {
    tokens.iter().position(|token| token.is_word(word))
}

fn month_range_trigger(tokens: &[Token]) -> Option<usize> {
    let index = word_index(tokens, "from")?;
    tokens
        .get(index + 1)
        .and_then(|token| month_number(&token.folded))?;
    Some(index)
}

fn subject_index(tokens: &[Token]) -> usize {
    usize::from(tokens.first().is_some_and(is_action_list))
}

fn edge_without_property(
    tokens: &[Token],
    schema: &SchemaSummary,
    trigger: usize,
) -> Result<String, EdgeFailure> {
    let subject = subject_index(tokens);
    if trigger != subject + 1 || tokens.len() != trigger + 2 {
        return Err(edge_failure(
            trigger,
            "`without` requires one property",
            "<table> without <column>",
        ));
    }
    let table = edge_node(subject, tokens, schema, "<table> without <column>")?;
    let column = edge_column(trigger + 1, tokens, table, "<table> without <column>")?;
    if !equality_type(&column.ty) {
        return Err(edge_failure(
            trigger + 1,
            "NULL-property negation requires an equality-comparable column",
            "<table> without <column>",
        ));
    }
    let binding = edge_binding(&table.name);
    Ok(format!(
        "{} | filter {}",
        scan(&table.name, &binding),
        missing_value_expr(&binding, &column.name)
    ))
}

fn edge_not_in(
    tokens: &[Token],
    schema: &SchemaSummary,
    trigger: usize,
) -> Result<String, EdgeFailure> {
    let subject = subject_index(tokens);
    if trigger != subject + 1 || tokens.len() != trigger + 4 {
        return Err(edge_failure_with_literals(
            trigger,
            "`not in` requires a property and value",
            "<table> not in <column> <value>",
            [trigger + 3],
        ));
    }
    let hint = "<table> not in <column> <value>";
    let table = edge_node(subject, tokens, schema, hint)?;
    let column = edge_column(trigger + 2, tokens, table, hint)?;
    let literal = negated_literal(trigger + 3, tokens, column, hint)?;
    let binding = edge_binding(&table.name);
    Ok(format!(
        "{} | filter {} != {literal}",
        scan(&table.name, &binding),
        column_ref(&binding, &column.name)
    ))
}

fn edge_between(
    tokens: &[Token],
    schema: &SchemaSummary,
    trigger: usize,
) -> Result<String, EdgeFailure> {
    let subject = subject_index(tokens);
    let column_index = if trigger == subject + 3 && tokens[subject + 1].is_word("with") {
        subject + 2
    } else if trigger == subject + 2 {
        subject + 1
    } else {
        return Err(between_syntax_failure(trigger));
    };
    if tokens.len() != trigger + 4 || !tokens[trigger + 2].is_word("and") {
        return Err(between_syntax_failure(trigger));
    }
    let hint = "<table> with <column> between <lower> and <upper>";
    let table = edge_node(subject, tokens, schema, hint)?;
    let column = edge_column(column_index, tokens, table, hint)?;
    if !matches!(column.ty.as_str(), "Int64" | "Float64") {
        return Err(edge_failure(
            trigger,
            "`between` requires a numeric column",
            hint,
        ));
    }
    numeric_bound(trigger + 1, tokens, hint)?;
    numeric_bound(trigger + 3, tokens, hint)?;
    Ok(emit_between(
        table,
        column,
        &tokens[trigger + 1],
        &tokens[trigger + 3],
    ))
}

fn between_syntax_failure(trigger: usize) -> EdgeFailure {
    edge_failure_with_literals(
        trigger,
        "`between` requires `<lower> and <upper>`",
        "<table> with <column> between <lower> and <upper>",
        [trigger + 1, trigger + 3],
    )
}

fn emit_between(
    table: &NodeTableSummary,
    column: &ColumnSummary,
    lower: &Token,
    upper: &Token,
) -> String {
    let binding = edge_binding(&table.name);
    let reference = column_ref(&binding, &column.name);
    format!(
        "{} | filter {reference} >= {} and {reference} <= {}",
        scan(&table.name, &binding),
        lower.original,
        upper.original
    )
}

fn edge_month_range(
    tokens: &[Token],
    schema: &SchemaSummary,
    trigger: usize,
) -> Result<String, EdgeFailure> {
    let subject = subject_index(tokens);
    let explicit_column = (trigger == subject + 2).then_some(subject + 1);
    if !matches!(trigger, value if value == subject + 1 || value == subject + 2)
        || tokens.len() != trigger + 5
        || !tokens[trigger + 2].is_word("to")
    {
        return Err(month_syntax_failure(trigger));
    }
    let hint = "<table> [<column>] from <month> to <month> <year>";
    let table = edge_node(subject, tokens, schema, hint)?;
    let column = month_column(explicit_column, tokens, table, trigger, hint)?;
    let bounds = month_bounds(trigger, tokens, hint)?;
    let binding = edge_binding(&table.name);
    Ok(with_time_filter(
        scan(&table.name, &binding),
        &[],
        &binding,
        &column.name,
        bounds,
    ))
}

fn month_syntax_failure(trigger: usize) -> EdgeFailure {
    edge_failure_with_literals(
        trigger,
        "month range requires `from <month> to <month> <year>`",
        "<table> [<column>] from <month> to <month> <year>",
        [trigger + 4],
    )
}

fn edge_without_relation(
    tokens: &[Token],
    schema: &SchemaSummary,
    trigger: usize,
) -> Result<String, EdgeFailure> {
    let subject = subject_index(tokens);
    let hint = "<table> with no <relationship>";
    if trigger != subject + 1 || tokens.len() <= trigger + 2 {
        return Err(edge_failure(
            trigger,
            "`with no` requires a relationship phrase",
            hint,
        ));
    }
    let table = edge_node(subject, tokens, schema, hint)?;
    let rel = absent_relation(trigger + 2, tokens, schema, hint)?;
    let direction = incident_direction(table, rel, trigger + 2, hint)?;
    let key = primary_key(table, subject, hint)?;
    Ok(emit_absent_edge(table, rel, key, direction))
}

fn absent_relation<'a>(
    start: usize,
    tokens: &[Token],
    schema: &'a SchemaSummary,
    hint: &str,
) -> Result<&'a RelTableSummary, EdgeFailure> {
    let phrase = tokens[start..]
        .iter()
        .map(|token| token.folded.as_str())
        .collect::<Vec<_>>();
    let declared = declared_edge_relations(&phrase, schema);
    let exact = relation_candidates(&phrase, schema, false);
    let plural = relation_candidates(&phrase, schema, true);
    let candidates = if !declared.is_empty() {
        declared
    } else if !exact.is_empty() {
        exact
    } else {
        plural
    };
    unique_relation(start, candidates, tokens, schema, hint)
}

fn declared_edge_relations<'a>(
    phrase: &[&str],
    schema: &'a SchemaSummary,
) -> Vec<&'a RelTableSummary> {
    let Some(classes) = &schema.classes else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    for class in &classes.rel_classes {
        let declared = [class.verb.as_deref(), class.inverse.as_deref()]
            .into_iter()
            .flatten()
            .any(|value| phrase_exact(phrase, &value.split_whitespace().collect::<Vec<_>>()));
        if declared
            && let Some(rel) = schema
                .rel_tables
                .iter()
                .find(|rel| fold(&rel.name) == fold(&class.table))
            && !matches
                .iter()
                .any(|candidate: &&RelTableSummary| fold(&candidate.name) == fold(&rel.name))
        {
            matches.push(rel);
        }
    }
    matches
}

fn relation_candidates<'a>(
    phrase: &[&str],
    schema: &'a SchemaSummary,
    plural: bool,
) -> Vec<&'a RelTableSummary> {
    schema
        .rel_tables
        .iter()
        .filter(|rel| {
            let parts = rel.name.split('_').collect::<Vec<_>>();
            if plural {
                phrase.len() == parts.len()
                    && phrase
                        .iter()
                        .zip(parts)
                        .all(|(left, right)| edge_plural_equal(left, right))
            } else {
                phrase_exact(phrase, &parts)
            }
        })
        .collect()
}

fn phrase_exact(phrase: &[&str], parts: &[&str]) -> bool {
    phrase.len() == parts.len()
        && phrase
            .iter()
            .zip(parts)
            .all(|(left, right)| *left == fold(right).as_ref())
}

fn unique_relation<'a>(
    token: usize,
    candidates: Vec<&'a RelTableSummary>,
    tokens: &[Token],
    schema: &SchemaSummary,
    hint: &str,
) -> Result<&'a RelTableSummary, EdgeFailure> {
    match candidates.as_slice() {
        [rel] => Ok(*rel),
        [] => Err(edge_failure(
            token,
            did_you_mean(
                &tokens[token].original,
                schema.rel_tables.iter().map(|rel| rel.name.as_str()),
            )
            .unwrap_or_else(|| "unknown relationship phrase".to_owned()),
            hint,
        )),
        rels => Err(edge_failure(
            token,
            rels.iter()
                .map(|rel| rel.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            hint,
        )),
    }
}

fn incident_direction(
    table: &NodeTableSummary,
    rel: &RelTableSummary,
    token: usize,
    hint: &str,
) -> Result<&'static str, EdgeFailure> {
    let outgoing = fold(&rel.from) == fold(&table.name);
    let incoming = fold(&rel.to) == fold(&table.name);
    match (outgoing, incoming) {
        (true, false) => Ok("out"),
        (false, true) => Ok("in"),
        (true, true) => Err(edge_failure(
            token,
            format!(
                "ambiguous edge direction for self-relationship `{}`",
                rel.name
            ),
            hint,
        )),
        (false, false) => Err(edge_failure(
            token,
            format!(
                "relationship `{}` is not incident to `{}`",
                rel.name, table.name
            ),
            hint,
        )),
    }
}

fn primary_key<'a>(
    table: &'a NodeTableSummary,
    token: usize,
    hint: &str,
) -> Result<&'a ColumnSummary, EdgeFailure> {
    let keys = table
        .columns
        .iter()
        .filter(|column| column.primary_key)
        .collect::<Vec<_>>();
    match keys.as_slice() {
        [key] => Ok(*key),
        _ => Err(edge_failure(
            token,
            format!("`{}` requires exactly one primary key", table.name),
            hint,
        )),
    }
}

fn emit_absent_edge(
    table: &NodeTableSummary,
    rel: &RelTableSummary,
    key: &ColumnSummary,
    direction: &str,
) -> String {
    let binding = edge_binding(&table.name);
    let source_key = column_ref("edge_source", &key.name);
    let left_key = column_ref(&binding, &key.name);
    let projection = table
        .columns
        .iter()
        .map(|column| column_ref(&binding, &column.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "let matches = {} | expand {} {direction} as edge_target | project {source_key}; {} | left join matches on {left_key} = {source_key} | filter {} | project {projection}",
        scan(&table.name, "edge_source"),
        identifier(&rel.name),
        scan(&table.name, &binding),
        missing_value_expr("edge_source", &key.name),
    )
}

fn missing_value_expr(binding: &str, column: &str) -> String {
    let reference = column_ref(binding, column);
    format!("coalesce({reference} = {reference}, false) = false")
}

fn edge_node<'a>(
    index: usize,
    tokens: &[Token],
    schema: &'a SchemaSummary,
    hint: &str,
) -> Result<&'a NodeTableSummary, EdgeFailure> {
    let mut candidates = schema
        .node_tables
        .iter()
        .filter(|table| fold(&table.name).as_ref() == tokens[index].folded)
        .collect::<Vec<_>>();
    let has_exact_table = !candidates.is_empty();
    append_class_matches(&mut candidates, &tokens[index], schema);
    if !has_exact_table {
        for table in schema
            .node_tables
            .iter()
            .filter(|table| edge_plural_equal(&tokens[index].folded, &table.name))
        {
            if !candidates
                .iter()
                .any(|candidate| candidate.name == table.name)
            {
                candidates.push(table);
            }
        }
    }
    if !candidates.is_empty() {
        return unique_node(index, candidates, hint);
    }
    Err(edge_failure(
        index,
        did_you_mean(
            &tokens[index].original,
            schema.node_tables.iter().map(|table| table.name.as_str()),
        )
        .unwrap_or_else(|| "unknown node table".to_owned()),
        hint,
    ))
}

fn append_class_matches<'a>(
    candidates: &mut Vec<&'a NodeTableSummary>,
    token: &Token,
    schema: &'a SchemaSummary,
) {
    let Some(classes) = &schema.classes else {
        return;
    };
    for class in &classes.node_classes {
        let matches = fold(&class.display).as_ref() == token.folded
            || class
                .plural
                .as_deref()
                .is_some_and(|plural| fold(plural).as_ref() == token.folded);
        if matches
            && let Some(table) = schema
                .node_tables
                .iter()
                .find(|table| fold(&table.name) == fold(&class.table))
            && !candidates
                .iter()
                .any(|candidate| candidate.name == table.name)
        {
            candidates.push(table);
        }
    }
}

fn unique_node<'a>(
    token: usize,
    candidates: Vec<&'a NodeTableSummary>,
    hint: &str,
) -> Result<&'a NodeTableSummary, EdgeFailure> {
    match candidates.as_slice() {
        [table] => Ok(*table),
        tables => Err(edge_failure(
            token,
            tables
                .iter()
                .map(|table| table.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            hint,
        )),
    }
}

fn edge_column<'a>(
    index: usize,
    tokens: &[Token],
    table: &'a NodeTableSummary,
    hint: &str,
) -> Result<&'a ColumnSummary, EdgeFailure> {
    let matches = table
        .columns
        .iter()
        .filter(|column| fold(&column.name).as_ref() == tokens[index].folded)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [column] => Ok(*column),
        [] => Err(edge_failure(
            index,
            did_you_mean(
                &tokens[index].original,
                table.columns.iter().map(|column| column.name.as_str()),
            )
            .unwrap_or_else(|| "unknown column".to_owned()),
            hint,
        )),
        columns => Err(edge_failure(
            index,
            columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            hint,
        )),
    }
}

fn negated_literal(
    index: usize,
    tokens: &[Token],
    column: &ColumnSummary,
    hint: &str,
) -> Result<String, EdgeFailure> {
    let token = &tokens[index];
    match (column.ty.as_str(), token.kind) {
        ("String", TokenKind::Quoted) if token.original.ends_with('"') => {
            Ok(token.original.clone())
        }
        ("String", TokenKind::Word) if !token.original.is_empty() => {
            Ok(quote_string(&token.original))
        }
        ("Int64" | "Float64", TokenKind::Number) => {
            numeric_bound(index, tokens, hint)?;
            Ok(token.original.clone())
        }
        _ => Err(edge_failure_with_literals(
            index,
            format!("value does not match column type `{}`", column.ty),
            hint,
            [index],
        )),
    }
}

fn numeric_bound(index: usize, tokens: &[Token], hint: &str) -> Result<(), EdgeFailure> {
    let valid = tokens.get(index).is_some_and(|token| {
        token.kind == TokenKind::Number
            && if token.original.contains(['.', 'e', 'E']) {
                token.original.parse::<f64>().is_ok_and(f64::is_finite)
            } else {
                token.original.parse::<i64>().is_ok()
            }
    });
    if valid {
        Ok(())
    } else {
        Err(edge_failure_with_literals(
            index,
            "range bound must be a finite DevonPlan number",
            hint,
            [index],
        ))
    }
}

fn equality_type(ty: &str) -> bool {
    matches!(
        ty,
        "Bool" | "Int64" | "Float64" | "String" | "Timestamp" | "Bytes" | "Json"
    ) || ty.starts_with("Decimal(")
}

fn month_column<'a>(
    explicit: Option<usize>,
    tokens: &[Token],
    table: &'a NodeTableSummary,
    trigger: usize,
    hint: &str,
) -> Result<&'a ColumnSummary, EdgeFailure> {
    if let Some(index) = explicit {
        let column = edge_column(index, tokens, table, hint)?;
        return (column.ty == "Int64").then_some(column).ok_or_else(|| {
            edge_failure(
                index,
                format!(
                    "time column `{}` must be Int64, found {}",
                    column.name, column.ty
                ),
                hint,
            )
        });
    }
    let candidates = table
        .columns
        .iter()
        .filter(|column| column.ty == "Int64" && time_column_name(&column.name))
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [column] => Ok(*column),
        [] => Err(edge_failure(
            trigger,
            "month range requires exactly one named Int64 time column",
            hint,
        )),
        columns => Err(edge_failure(
            trigger,
            format!(
                "ambiguous time column: {}",
                columns
                    .iter()
                    .map(|column| format!("`{}`", column.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            hint,
        )),
    }
}

fn time_column_name(name: &str) -> bool {
    matches!(
        fold(name).as_ref(),
        "date"
            | "time"
            | "timestamp"
            | "occurred"
            | "created"
            | "created_at"
            | "ordered"
            | "ordered_at"
            | "order_date"
            | "placed"
            | "placed_at"
    )
}

fn month_bounds(trigger: usize, tokens: &[Token], hint: &str) -> Result<TimeBounds, EdgeFailure> {
    let start = month_number(&tokens[trigger + 1].folded)
        .ok_or_else(|| edge_failure(trigger + 1, "unknown start month", hint))?;
    let end = month_number(&tokens[trigger + 3].folded)
        .ok_or_else(|| edge_failure(trigger + 3, "unknown end month", hint))?;
    let year = tokens[trigger + 4]
        .original
        .parse::<i32>()
        .ok()
        .filter(|year| (1..=9999).contains(year))
        .ok_or_else(|| {
            edge_failure_with_literals(
                trigger + 4,
                "year must be an integer from 1 through 9999",
                hint,
                [trigger + 4],
            )
        })?;
    if start > end {
        return Err(edge_failure(
            trigger + 3,
            "month range is ambiguous across a year boundary",
            hint,
        ));
    }
    let (upper_year, upper_month) = next_month(year, end);
    let lower = month_start_epoch(year, start)
        .ok_or_else(|| edge_failure(trigger + 1, "month range is outside Int64 bounds", hint))?;
    let upper = month_start_epoch(upper_year, upper_month)
        .ok_or_else(|| edge_failure(trigger + 3, "month range is outside Int64 bounds", hint))?;
    Ok(TimeBounds {
        lower,
        upper: Some(upper),
    })
}

fn month_number(month: &str) -> Option<u32> {
    [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ]
    .iter()
    .position(|candidate| *candidate == month)
    .and_then(|index| u32::try_from(index + 1).ok())
}

fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

fn month_start_epoch(year: i32, month: u32) -> Option<i64> {
    let january_or_february = if month <= 2 { 1 } else { 0 };
    let adjusted_year = i64::from(year) - january_or_february;
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era
        .checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)?;
    days.checked_mul(DAY_SECONDS)
}

fn edge_binding(table: &str) -> String {
    let folded = fold(table).into_owned();
    if let Some((_, singular)) = EDGE_IRREGULARS.iter().find(|(plural, _)| *plural == folded) {
        return (*singular).to_owned();
    }
    if let Some(stem) = folded.strip_suffix("ies") {
        return format!("{stem}y");
    }
    if let Some(stem) = folded.strip_suffix('s') {
        stem.to_owned()
    } else {
        folded
    }
}

const EDGE_IRREGULARS: [(&str, &str); 8] = [
    ("people", "person"),
    ("children", "child"),
    ("men", "man"),
    ("women", "woman"),
    ("feet", "foot"),
    ("teeth", "tooth"),
    ("geese", "goose"),
    ("mice", "mouse"),
];

fn edge_plural_equal(left: &str, right: &str) -> bool {
    let left_forms = edge_plural_forms(left);
    let right_forms = edge_plural_forms(fold(right).as_ref());
    left_forms.iter().any(|form| right_forms.contains(form))
}

fn edge_plural_forms(word: &str) -> BTreeSet<String> {
    let mut forms = BTreeSet::from([word.to_owned()]);
    if let Some((_, singular)) = EDGE_IRREGULARS.iter().find(|(plural, _)| *plural == word) {
        forms.insert((*singular).to_owned());
    }
    if let Some(stem) = word.strip_suffix("ies") {
        forms.insert(format!("{stem}y"));
    }
    if let Some(stem) = word.strip_suffix("es") {
        forms.insert(stem.to_owned());
    }
    if let Some(stem) = word.strip_suffix('s') {
        forms.insert(stem.to_owned());
    }
    forms
}

fn edge_failure(
    token: usize,
    suggestion: impl Into<String>,
    hint: impl Into<String>,
) -> EdgeFailure {
    EdgeFailure {
        failure: GroundFailure {
            issues: vec![GroundIssue {
                token,
                suggestion: Some(suggestion.into()),
            }],
        },
        literal_slots: BTreeSet::new(),
        hint: hint.into(),
    }
}

fn edge_failure_with_literals<const N: usize>(
    token: usize,
    suggestion: impl Into<String>,
    hint: impl Into<String>,
    literals: [usize; N],
) -> EdgeFailure {
    let mut failure = edge_failure(token, suggestion, hint);
    failure.literal_slots.extend(literals);
    failure
}

fn edge_refusal(tokens: &[Token], schema: &SchemaSummary, failure: EdgeFailure) -> NoParse {
    let issues = failure
        .failure
        .issues
        .iter()
        .map(|issue| (issue.token, issue.suggestion.clone()))
        .collect::<BTreeMap<_, _>>();
    let rel_targets = relationship_spans(tokens, schema);
    let mut recognized = Vec::new();
    let mut unrecognized = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        classify_token(
            index,
            token,
            schema,
            &issues,
            &failure.literal_slots,
            &rel_targets,
            &mut recognized,
            &mut unrecognized,
        );
    }
    NoParse {
        recognized,
        unrecognized,
        nearest: vec![TemplateHint {
            example: failure.hint,
        }],
    }
}

fn emit(
    intent: &GroundedIntent,
    reference_date: Option<i64>,
) -> Result<Option<devondb::Plan>, GroundFailure> {
    let text = match intent {
        GroundedIntent::Time {
            base,
            column,
            phrase,
            phrase_token,
        } => emit_time(base, column, *phrase, *phrase_token, reference_date)?,
        _ => {
            let Some(text) = emit_text(intent) else {
                return Ok(None);
            };
            text
        }
    };
    Ok(match parse(&text).ok() {
        Some(Parsed::Query(plan)) => Some(plan),
        Some(Parsed::Statement(_)) | None => None,
    })
}

fn emit_text(intent: &GroundedIntent) -> Option<String> {
    Some(match intent {
        GroundedIntent::Search {
            table,
            binding,
            column,
            query,
        } => format!(
            "textscan({}.{}, {}, k=10) as {}",
            identifier(table),
            identifier(column),
            query,
            identifier(binding)
        ),
        GroundedIntent::List { table, binding } => scan(table, binding),
        GroundedIntent::Filter {
            table,
            binding,
            predicates,
        } => with_filters(scan(table, binding), predicates),
        GroundedIntent::Entity {
            table,
            binding,
            label,
            literal,
        } => entity_scan(table, binding, label, literal),
        GroundedIntent::Traversal {
            table,
            binding,
            label,
            literal,
            rel,
            direction,
            far_label,
            predicates,
        } => emit_traversal(TraversalText {
            table,
            binding,
            label,
            literal,
            rel,
            direction: *direction,
            far_label,
            predicates,
        }),
        GroundedIntent::MultiHop {
            table,
            binding,
            label,
            literal,
            hops,
            far_label,
        } => emit_multi_hop(table, binding, label, literal, hops, far_label),
        GroundedIntent::Count {
            table,
            binding,
            count_column,
            predicates,
            group,
        } => emit_count(table, binding, count_column, predicates, group.as_deref()),
        GroundedIntent::Aggregate {
            table,
            binding,
            function,
            column,
            predicates,
            group,
        } => emit_aggregate(
            table,
            binding,
            *function,
            column,
            predicates,
            group.as_deref(),
        ),
        GroundedIntent::Top {
            table,
            binding,
            predicates,
            sort_column,
            direction,
            count,
        } => emit_top(table, binding, predicates, sort_column, *direction, *count),
        GroundedIntent::Within {
            base,
            column,
            center,
            meters,
        } => return emit_within(base, column, center, *meters),
        GroundedIntent::SimilarTo {
            table,
            label,
            literal,
            embedding,
            k,
        } => emit_similar_to(table, label, literal, embedding, *k),
        GroundedIntent::Time { .. } => return None,
    })
}

/// `knn(<table>.<embedding>, scalar(<anchor pipeline>), k, cosine)` (§ 18).
/// The `anchor` binding is canonical and fixed; refusal pins depend on it.
fn emit_similar_to(table: &str, label: &str, literal: &str, embedding: &str, k: u64) -> String {
    let query = format!(
        "nodes({}) as anchor | filter anchor.{} = {literal} | project anchor.{}",
        identifier(table),
        identifier(label),
        identifier(embedding),
    );
    format!(
        "knn({}, scalar({query}), {k}, cosine)",
        column_ref(table, embedding)
    )
}

const DAY_SECONDS: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimeBounds {
    lower: i64,
    upper: Option<i64>,
}

fn emit_time(
    base: &GroundedIntent,
    column: &str,
    phrase: GroundedTimePhrase,
    phrase_token: usize,
    reference_date: Option<i64>,
) -> Result<String, GroundFailure> {
    let reference_date = reference_date.ok_or_else(|| {
        compile_failure(
            phrase_token,
            "time phrase requires a reference date; use compile_at",
        )
    })?;
    if reference_date.rem_euclid(DAY_SECONDS) != 0 {
        return Err(compile_failure(
            phrase_token,
            "reference date must be Int64 epoch seconds at UTC midnight",
        ));
    }
    let bounds = time_bounds(reference_date, phrase).ok_or_else(|| {
        compile_failure(
            phrase_token,
            "time range is outside Int64 epoch-second bounds for this reference date",
        )
    })?;
    match base {
        GroundedIntent::List { table, binding } => Ok(with_time_filter(
            scan(table, binding),
            &[],
            binding,
            column,
            bounds,
        )),
        GroundedIntent::Filter {
            table,
            binding,
            predicates,
        } => Ok(with_time_filter(
            scan(table, binding),
            predicates,
            binding,
            column,
            bounds,
        )),
        _ => Err(compile_failure(
            phrase_token,
            "time phrases compose only with list and property-filter entity sets",
        )),
    }
}

fn time_bounds(reference_date: i64, phrase: GroundedTimePhrase) -> Option<TimeBounds> {
    match phrase {
        GroundedTimePhrase::SinceLastWeek => Some(TimeBounds {
            lower: days_before(reference_date, 7)?,
            upper: None,
        }),
        GroundedTimePhrase::LastDays(days) => Some(TimeBounds {
            lower: days_before(reference_date, days)?,
            upper: Some(reference_date),
        }),
        GroundedTimePhrase::Yesterday => Some(TimeBounds {
            lower: days_before(reference_date, 1)?,
            upper: Some(reference_date),
        }),
        GroundedTimePhrase::Today => Some(TimeBounds {
            lower: reference_date,
            upper: Some(reference_date.checked_add(DAY_SECONDS)?),
        }),
        GroundedTimePhrase::SinceDaysAgo(days) => Some(TimeBounds {
            lower: days_before(reference_date, days)?,
            upper: None,
        }),
    }
}

fn days_before(reference_date: i64, days: i64) -> Option<i64> {
    reference_date.checked_sub(days.checked_mul(DAY_SECONDS)?)
}

fn compile_failure(token: usize, suggestion: &str) -> GroundFailure {
    GroundFailure {
        issues: vec![GroundIssue {
            token,
            suggestion: Some(suggestion.to_owned()),
        }],
    }
}

fn with_time_filter(
    mut text: String,
    predicates: &[Predicate],
    binding: &str,
    column: &str,
    bounds: TimeBounds,
) -> String {
    text.push_str(" | filter ");
    for (index, predicate) in predicates.iter().enumerate() {
        if index != 0 {
            text.push_str(" and ");
        }
        let _ = write!(
            text,
            "{} {} {}",
            column_ref(&predicate.binding, &predicate.column),
            predicate.comparator.text(),
            predicate.literal
        );
    }
    if !predicates.is_empty() {
        text.push_str(" and ");
    }
    let column = column_ref(binding, column);
    let _ = write!(text, "{column} >= {}", bounds.lower);
    if let Some(upper) = bounds.upper {
        let _ = write!(text, " and {column} < {upper}");
    }
    text
}

fn emit_multi_hop(
    table: &str,
    binding: &str,
    label: &str,
    literal: &str,
    hops: &[GroundedHop; 2],
    far_label: &str,
) -> String {
    let mut text = entity_scan(table, binding, label, literal);
    for (index, hop) in hops.iter().enumerate() {
        let direction = match hop.direction {
            Direction::Out => "out",
            Direction::In => "in",
        };
        let far_binding = if index == 0 { "hop1" } else { "other" };
        let _ = write!(
            text,
            " | expand {} {direction} as {far_binding}",
            identifier(&hop.rel)
        );
    }
    let _ = write!(text, " | project {}", column_ref("other", far_label));
    text
}

fn scan(table: &str, binding: &str) -> String {
    format!("nodes({}) as {}", identifier(table), identifier(binding))
}

fn entity_scan(table: &str, binding: &str, label: &str, literal: &str) -> String {
    let mut text = scan(table, binding);
    let _ = write!(text, " | filter {} = {literal}", column_ref(binding, label));
    text
}

fn with_filters(mut text: String, predicates: &[Predicate]) -> String {
    if predicates.is_empty() {
        return text;
    }
    text.push_str(" | filter ");
    for (index, predicate) in predicates.iter().enumerate() {
        if index != 0 {
            text.push_str(" and ");
        }
        let _ = write!(
            text,
            "{} {} {}",
            column_ref(&predicate.binding, &predicate.column),
            predicate.comparator.text(),
            predicate.literal
        );
    }
    text
}

struct TraversalText<'a> {
    table: &'a str,
    binding: &'a str,
    label: &'a str,
    literal: &'a str,
    rel: &'a str,
    direction: Direction,
    far_label: &'a str,
    predicates: &'a [Predicate],
}

fn emit_traversal(parts: TraversalText<'_>) -> String {
    let mut text = entity_scan(parts.table, parts.binding, parts.label, parts.literal);
    let direction = match parts.direction {
        Direction::Out => "out",
        Direction::In => "in",
    };
    let _ = write!(
        text,
        " | expand {} {direction} as other",
        identifier(parts.rel)
    );
    text = with_filters(text, parts.predicates);
    let _ = write!(text, " | project {}", column_ref("other", parts.far_label));
    text
}

fn emit_count(
    table: &str,
    binding: &str,
    count_column: &str,
    predicates: &[Predicate],
    group: Option<&str>,
) -> String {
    let mut text = with_filters(scan(table, binding), predicates);
    push_count_aggregate(&mut text, binding, count_column, group);
    text
}

fn push_count_aggregate(text: &mut String, binding: &str, count_column: &str, group: Option<&str>) {
    let _ = write!(
        text,
        " | aggregate count({}) as {}",
        column_ref(binding, count_column),
        identifier("count")
    );
    if let Some(group) = group {
        let _ = write!(text, " by {}", column_ref(binding, group));
    }
}

/// `… | aggregate <fn>(<column>) as <fixed-output> [by <group>]` (§ 17).
/// Column typing is the engine's law; emission never inspects types.
fn emit_aggregate(
    table: &str,
    binding: &str,
    function: AggregateFunction,
    column: &str,
    predicates: &[Predicate],
    group: Option<&str>,
) -> String {
    let mut text = with_filters(scan(table, binding), predicates);
    let _ = write!(
        text,
        " | aggregate {}({}) as {}",
        function.text(),
        column_ref(binding, column),
        identifier(function.output())
    );
    if let Some(group) = group {
        let _ = write!(text, " by {}", column_ref(binding, group));
    }
    text
}

fn emit_top(
    table: &str,
    binding: &str,
    predicates: &[Predicate],
    sort_column: &str,
    direction: SortDirection,
    count: u64,
) -> String {
    let mut text = with_filters(scan(table, binding), predicates);
    let order = match direction {
        SortDirection::Desc => " desc",
        SortDirection::Asc => "",
    };
    let _ = write!(
        text,
        " | sort {}{order} | limit {count}",
        column_ref(binding, sort_column)
    );
    text
}

fn emit_within(base: &GroundedIntent, column: &str, center: &str, meters: f64) -> Option<String> {
    let table = source_table(base)?;
    let source = format!(
        "within({}.{}, {center}, {meters})",
        identifier(table),
        identifier(column)
    );
    emit_from_source(base, table, source)
}

fn source_table(intent: &GroundedIntent) -> Option<&str> {
    match intent {
        GroundedIntent::List { table, .. }
        | GroundedIntent::Filter { table, .. }
        | GroundedIntent::Entity { table, .. }
        | GroundedIntent::Count { table, .. }
        | GroundedIntent::Top { table, .. } => Some(table),
        GroundedIntent::Traversal { .. }
        | GroundedIntent::MultiHop { .. }
        | GroundedIntent::Within { .. }
        | GroundedIntent::Time { .. }
        | GroundedIntent::Aggregate { .. }
        | GroundedIntent::Search { .. }
        | GroundedIntent::SimilarTo { .. } => None,
    }
}

fn emit_from_source(base: &GroundedIntent, binding: &str, source: String) -> Option<String> {
    match base {
        GroundedIntent::List { .. } => Some(source),
        GroundedIntent::Filter { predicates, .. } => {
            Some(with_filters_as(source, predicates, binding))
        }
        GroundedIntent::Entity { label, literal, .. } => {
            let mut text = source;
            let _ = write!(text, " | filter {} = {literal}", column_ref(binding, label));
            Some(text)
        }
        GroundedIntent::Count {
            count_column,
            predicates,
            group: None,
            ..
        } => Some(emit_count_from(source, binding, count_column, predicates)),
        GroundedIntent::Count { group: Some(_), .. } => None,
        GroundedIntent::Top {
            predicates,
            sort_column,
            direction,
            count,
            ..
        } => Some(emit_top_from(
            source,
            binding,
            predicates,
            sort_column,
            *direction,
            *count,
        )),
        GroundedIntent::Traversal { .. }
        | GroundedIntent::MultiHop { .. }
        | GroundedIntent::Within { .. }
        | GroundedIntent::Time { .. }
        | GroundedIntent::Aggregate { .. }
        | GroundedIntent::Search { .. }
        | GroundedIntent::SimilarTo { .. } => None,
    }
}

fn with_filters_as(mut text: String, predicates: &[Predicate], binding: &str) -> String {
    if predicates.is_empty() {
        return text;
    }
    text.push_str(" | filter ");
    for (index, predicate) in predicates.iter().enumerate() {
        if index != 0 {
            text.push_str(" and ");
        }
        let _ = write!(
            text,
            "{} {} {}",
            column_ref(binding, &predicate.column),
            predicate.comparator.text(),
            predicate.literal
        );
    }
    text
}

fn emit_count_from(
    source: String,
    binding: &str,
    count_column: &str,
    predicates: &[Predicate],
) -> String {
    let mut text = with_filters_as(source, predicates, binding);
    push_count_aggregate(&mut text, binding, count_column, None);
    text
}

fn emit_top_from(
    source: String,
    binding: &str,
    predicates: &[Predicate],
    sort_column: &str,
    direction: SortDirection,
    count: u64,
) -> String {
    let mut text = with_filters_as(source, predicates, binding);
    let order = match direction {
        SortDirection::Desc => " desc",
        SortDirection::Asc => "",
    };
    let _ = write!(
        text,
        " | sort {}{order} | limit {count}",
        column_ref(binding, sort_column)
    );
    text
}

fn statement_refusal(
    tokens: &[Token],
    schema: &SchemaSummary,
    candidates: &[(usize, StatementIntentTokens)],
    failures: &[(usize, GroundFailure)],
) -> NoParse {
    let issues = best_issues(failures);
    let literal_slots = statement_literal_slots(candidates);
    let mut recognized = Vec::new();
    let mut unrecognized = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        classify_token(
            index,
            token,
            schema,
            &issues,
            &literal_slots,
            &BTreeMap::new(),
            &mut recognized,
            &mut unrecognized,
        );
    }
    NoParse {
        recognized,
        unrecognized,
        nearest: statement_nearest_hints(tokens, schema, candidates, &issues),
    }
}

fn statement_literal_slots(candidates: &[(usize, StatementIntentTokens)]) -> BTreeSet<usize> {
    let mut slots = BTreeSet::new();
    for (_, intent) in candidates {
        match intent {
            StatementIntentTokens::Update { key, value, .. } => {
                slots.insert(*key);
                slots.insert(*value);
            }
            StatementIntentTokens::Delete { key, .. } => {
                slots.insert(*key);
            }
        }
    }
    slots
}

#[derive(Default)]
struct StatementHintSlots {
    table: Option<String>,
    key: Option<String>,
    column: Option<String>,
    value: Option<String>,
}

fn statement_nearest_hints(
    tokens: &[Token],
    schema: &SchemaSummary,
    candidates: &[(usize, StatementIntentTokens)],
    issues: &BTreeMap<usize, Option<String>>,
) -> Vec<TemplateHint> {
    let slots = statement_hint_slots(tokens, schema, candidates, issues);
    let family = statement_family(tokens);
    let mut ranked = STATEMENT_TEMPLATES
        .iter()
        .enumerate()
        .map(|(index, template)| {
            let (example, score) = instantiate_statement(template.example, &slots);
            // § 7 / § 19: the question's head verb grounds the matching
            // family's verb slot — ONE grounded slot, never a separate tier.
            let marker = family == Some(template.family);
            let score = score + usize::from(marker);
            (score, marker, index, TemplateHint { example })
        })
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(score, marker, index, _)| {
        (
            std::cmp::Reverse(*score),
            std::cmp::Reverse(*marker),
            *index,
        )
    });
    ranked
        .into_iter()
        .take(3)
        .map(|(_, _, _, hint)| hint)
        .collect()
}

fn statement_hint_slots(
    tokens: &[Token],
    schema: &SchemaSummary,
    candidates: &[(usize, StatementIntentTokens)],
    issues: &BTreeMap<usize, Option<String>>,
) -> StatementHintSlots {
    let mut slots = StatementHintSlots::default();
    for (_, intent) in candidates {
        fill_statement_candidate_slots(&mut slots, intent, tokens, schema, issues);
    }
    if candidates.is_empty() && slots.table.is_none() {
        slots.table = tokens.iter().find_map(|token| {
            schema
                .node_tables
                .iter()
                .any(|table| catalog_targets(token, schema).contains(&table.name))
                .then(|| token.original.clone())
        });
    }
    if candidates.is_empty() && slots.key.is_none() {
        slots.key = tokens
            .iter()
            .rev()
            .find(|token| matches!(token.kind, TokenKind::Number | TokenKind::Quoted))
            .map(|token| token.original.clone());
    }
    slots
}

fn fill_statement_candidate_slots(
    slots: &mut StatementHintSlots,
    intent: &StatementIntentTokens,
    tokens: &[Token],
    schema: &SchemaSummary,
    issues: &BTreeMap<usize, Option<String>>,
) {
    let (table, key, column, value) = match intent {
        StatementIntentTokens::Update {
            table,
            key,
            column,
            value,
        } => (*table, *key, Some(*column), Some(*value)),
        StatementIntentTokens::Delete { table, key } => (*table, *key, None, None),
    };
    fill_grounded_slot(&mut slots.table, table, tokens, issues);
    fill_grounded_slot(&mut slots.key, key, tokens, issues);
    if let Some(column) = column
        && !issues.contains_key(&column)
        && !catalog_targets(&tokens[column], schema).is_empty()
    {
        slots.column = Some(tokens[column].original.clone());
    }
    if let Some(value) = value {
        fill_grounded_slot(&mut slots.value, value, tokens, issues);
    }
}

fn fill_grounded_slot(
    slot: &mut Option<String>,
    index: usize,
    tokens: &[Token],
    issues: &BTreeMap<usize, Option<String>>,
) {
    if slot.is_none() && !issues.contains_key(&index) {
        *slot = Some(tokens[index].original.clone());
    }
}

fn statement_family(tokens: &[Token]) -> Option<StatementFamily> {
    match tokens.first()?.folded.as_str() {
        "set" | "change" => Some(StatementFamily::Update),
        "delete" | "remove" => Some(StatementFamily::Delete),
        _ => None,
    }
}

fn instantiate_statement(example: &str, slots: &StatementHintSlots) -> (String, usize) {
    let replacements = [
        ("<pk-value>", slots.key.as_deref()),
        ("<column>", slots.column.as_deref()),
        ("<table>", slots.table.as_deref()),
        ("<value>", slots.value.as_deref()),
    ];
    let mut output = example.to_owned();
    let mut score = 0;
    for (placeholder, value) in replacements {
        if output.contains(placeholder)
            && let Some(value) = value
        {
            output = output.replace(placeholder, value);
            score += 1;
        }
    }
    (output, score)
}

fn refusal(
    tokens: &[Token],
    schema: &SchemaSummary,
    candidates: &[(usize, IntentTokens)],
    failures: &[(usize, GroundFailure)],
) -> NoParse {
    let issues = best_issues(failures);
    let literal_slots = literal_slots(candidates);
    let rel_targets = relationship_spans(tokens, schema);
    let mut recognized = Vec::new();
    let mut unrecognized = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        classify_token(
            index,
            token,
            schema,
            &issues,
            &literal_slots,
            &rel_targets,
            &mut recognized,
            &mut unrecognized,
        );
    }
    NoParse {
        recognized,
        unrecognized,
        nearest: nearest_hints(tokens, schema, candidates, &issues),
    }
}

fn multi_hop_cap_refusal(tokens: &[Token], schema: &SchemaSummary, cap: &MultiHopCap) -> NoParse {
    let issues = BTreeMap::from([(
        cap.blocked_token,
        Some("multi-hop traversal has a hard 2-hop cap".to_owned()),
    )]);
    let literal_slots = BTreeSet::from([cap.entity]);
    let rel_targets = relationship_spans(tokens, schema);
    let mut recognized = Vec::new();
    let mut unrecognized = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        classify_token(
            index,
            token,
            schema,
            &issues,
            &literal_slots,
            &rel_targets,
            &mut recognized,
            &mut unrecognized,
        );
    }
    NoParse {
        recognized,
        unrecognized,
        nearest: vec![TemplateHint {
            example: cap.nearest.clone(),
        }],
    }
}

fn best_issues(failures: &[(usize, GroundFailure)]) -> BTreeMap<usize, Option<String>> {
    failures
        .iter()
        .min_by_key(|(template, failure)| (failure.issues.len(), *template))
        .map(|(_, failure)| {
            failure
                .issues
                .iter()
                .map(|issue| (issue.token, issue.suggestion.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn literal_slots(candidates: &[(usize, IntentTokens)]) -> BTreeSet<usize> {
    let mut slots = BTreeSet::new();
    for (_, intent) in candidates {
        collect_literal_slots(intent, &mut slots);
    }
    slots
}

fn collect_literal_slots(intent: &IntentTokens, slots: &mut BTreeSet<usize>) {
    match intent {
        IntentTokens::Entity { entity }
        | IntentTokens::Traversal { entity, .. }
        | IntentTokens::MultiHop { entity, .. } => {
            slots.insert(*entity);
        }
        IntentTokens::Search { query, .. } => {
            slots.insert(*query);
        }
        IntentTokens::SimilarTo { count, entity, .. } => {
            if let Some(count) = count {
                slots.insert(*count);
            }
            slots.insert(*entity);
        }
        IntentTokens::Filter { filters, .. }
        | IntentTokens::Count { filters, .. }
        | IntentTokens::Top { filters, .. } => {
            slots.extend(filters.iter().map(|filter| filter.value));
        }
        IntentTokens::Within { base, place, .. } => {
            slots.extend(place.clone());
            collect_literal_slots(base, slots);
        }
        IntentTokens::Time { base, phrase, .. } => {
            match phrase {
                crate::template::TimePhrase::LastDays { count }
                | crate::template::TimePhrase::SinceDaysAgo { count } => {
                    slots.insert(*count);
                }
                crate::template::TimePhrase::SinceLastWeek
                | crate::template::TimePhrase::Yesterday
                | crate::template::TimePhrase::Today => {}
            }
            collect_literal_slots(base, slots);
        }
        IntentTokens::Aggregate { base, .. } => collect_literal_slots(base, slots),
        IntentTokens::List { .. } => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn classify_token(
    index: usize,
    token: &Token,
    schema: &SchemaSummary,
    issues: &BTreeMap<usize, Option<String>>,
    literal_slots: &BTreeSet<usize>,
    rel_targets: &BTreeMap<usize, String>,
    recognized: &mut Vec<Grounded>,
    unrecognized: &mut Vec<Ungrounded>,
) {
    if let Some(suggestion) = issues.get(&index) {
        unrecognized.push(Ungrounded {
            token: token.original.clone(),
            suggestion: suggestion.clone(),
        });
        return;
    }
    let targets = catalog_targets(token, schema);
    let target = if is_vocabulary(token) {
        Some(token.folded.clone())
    } else if matches!(
        token.kind,
        TokenKind::Number | TokenKind::Quoted | TokenKind::Geo
    ) || literal_slots.contains(&index)
    {
        Some("literal".to_owned())
    } else if let Some(target) = rel_targets.get(&index) {
        Some(target.clone())
    } else if targets.len() == 1 {
        targets.first().cloned()
    } else if targets.is_empty() && starts_uppercase(&token.original) {
        Some("literal".to_owned())
    } else {
        None
    };
    if let Some(target) = target {
        recognized.push(Grounded {
            token: token.original.clone(),
            target,
        });
    } else {
        unrecognized.push(Ungrounded {
            token: token.original.clone(),
            suggestion: token_suggestion(token, schema, &targets),
        });
    }
}

fn token_suggestion(token: &Token, schema: &SchemaSummary, targets: &[String]) -> Option<String> {
    if targets.len() > 1 {
        return Some(targets.join(", "));
    }
    let mut names = all_words().collect::<Vec<_>>();
    names.extend(schema.node_tables.iter().map(|table| table.name.as_str()));
    names.extend(schema.rel_tables.iter().map(|rel| rel.name.as_str()));
    for table in &schema.node_tables {
        names.extend(table.columns.iter().map(|column| column.name.as_str()));
    }
    did_you_mean(&token.original, names)
}

fn starts_uppercase(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

#[derive(Default)]
struct HintSlots {
    table: Option<String>,
    columns: Vec<String>,
    number: Option<String>,
    value: Option<String>,
    entity: Option<String>,
    rel: Option<String>,
    place: Option<String>,
}

fn nearest_hints(
    tokens: &[Token],
    schema: &SchemaSummary,
    candidates: &[(usize, IntentTokens)],
    issues: &BTreeMap<usize, Option<String>>,
) -> Vec<TemplateHint> {
    let slots = hint_slots(tokens, schema, candidates, issues);
    let search_question = tokens.first().is_some_and(|token| token.is_word("search"));
    let within_question = tokens.iter().any(|token| token.is_word("within"));
    let multi_hop_question = candidates
        .iter()
        .any(|(_, intent)| intent.multi_hop_rels().is_some());
    let time_question = candidates
        .iter()
        .any(|(_, intent)| matches!(intent, IntentTokens::Time { .. }));
    let aggregate_question = candidates.iter().any(|(_, intent)| {
        matches!(intent, IntentTokens::Aggregate { .. })
            || matches!(intent, IntentTokens::Count { group: Some(_), .. })
    });
    let similar_question = candidates
        .iter()
        .any(|(_, intent)| matches!(intent, IntentTokens::SimilarTo { .. }));
    let mut ranked = TEMPLATES
        .iter()
        .enumerate()
        .filter(|(_, template)| multi_hop_question || !template.is_multi_hop())
        .filter(|(_, template)| search_question || !template.example.starts_with("search "))
        .map(|(index, template)| {
            let (example, score) = instantiate(template.example, &slots);
            // § 7 / § 19: a recognized family marker (`within`, a time
            // phrase, an aggregate head, similar-to) grounds the matching
            // template's marker slot — ONE grounded slot, never a tier.
            let marker_grounded = (search_question && template.example.starts_with("search "))
                || (within_question && template.is_within())
                || (time_question && template.is_time())
                || (aggregate_question && template.is_aggregate())
                || (similar_question && template.is_similar_to());
            let score = score + usize::from(marker_grounded);
            (score, marker_grounded, index, TemplateHint { example })
        })
        .collect::<Vec<_>>();
    ranked.sort_by_key(|(score, marker, index, _)| {
        (
            std::cmp::Reverse(*score),
            std::cmp::Reverse(*marker),
            *index,
        )
    });
    ranked
        .into_iter()
        .take(3)
        .map(|(_, _, _, hint)| hint)
        .collect()
}

fn hint_slots(
    tokens: &[Token],
    schema: &SchemaSummary,
    candidates: &[(usize, IntentTokens)],
    issues: &BTreeMap<usize, Option<String>>,
) -> HintSlots {
    let mut slots = HintSlots::default();
    for (index, token) in tokens.iter().enumerate() {
        let targets = catalog_targets(token, schema);
        if slots.table.is_none()
            && schema
                .node_tables
                .iter()
                .any(|table| targets.contains(&table.name))
        {
            slots.table = Some(token.original.clone());
        }
        if schema.node_tables.iter().any(|table| {
            table
                .columns
                .iter()
                .any(|column| targets.contains(&column.name))
        }) {
            slots.columns.push(token.original.clone());
        }
        if token.kind == TokenKind::Number {
            slots.number.get_or_insert_with(|| token.original.clone());
            slots.value.get_or_insert_with(|| token.original.clone());
        } else if token.kind == TokenKind::Quoted || starts_uppercase(&token.original) {
            slots.value.get_or_insert_with(|| token.original.clone());
        }
        if entity_indices(candidates).contains(&index) {
            slots.entity.get_or_insert_with(|| token.original.clone());
        }
    }
    slots.rel = relationship_spans(tokens, schema).values().next().cloned();
    slots.place = candidates
        .iter()
        .find_map(|(_, intent)| within_place(intent, tokens));
    // A slot that failed grounding still teaches the working spelling: when
    // its suggestion is exactly a catalog column (§ 17's `did you mean`),
    // fill the column slot with the suggestion so the hint reads as the
    // corrected phrasing. Grounded columns keep precedence.
    for suggestion in issues.values().flatten() {
        let is_column = schema.node_tables.iter().any(|table| {
            table
                .columns
                .iter()
                .any(|column| fold(&column.name) == fold(suggestion))
        });
        if is_column && !slots.columns.iter().any(|column| column == suggestion) {
            slots.columns.push(suggestion.clone());
        }
    }
    slots
}

fn within_place(intent: &IntentTokens, tokens: &[Token]) -> Option<String> {
    let IntentTokens::Within { place, .. } = intent else {
        return None;
    };
    Some(
        tokens[place.clone()]
            .iter()
            .map(|token| token.original.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn entity_indices(candidates: &[(usize, IntentTokens)]) -> BTreeSet<usize> {
    candidates
        .iter()
        .filter_map(|(_, intent)| entity_index(intent))
        .collect()
}

fn entity_index(intent: &IntentTokens) -> Option<usize> {
    match intent {
        IntentTokens::Entity { entity }
        | IntentTokens::Traversal { entity, .. }
        | IntentTokens::MultiHop { entity, .. }
        | IntentTokens::SimilarTo { entity, .. } => Some(*entity),
        IntentTokens::Within { base, .. } => entity_index(base),
        IntentTokens::Time { base, .. } => entity_index(base),
        IntentTokens::Aggregate { base, .. } => entity_index(base),
        _ => None,
    }
}

fn instantiate(example: &str, slots: &HintSlots) -> (String, usize) {
    let first_column = slots.columns.first().map(String::as_str);
    let sort_column = slots
        .columns
        .get(1)
        .or(slots.columns.first())
        .map(String::as_str);
    let replacements = [
        ("<sort-column>", sort_column),
        ("<column>", first_column),
        ("<table>", slots.table.as_deref()),
        ("<n>", slots.number.as_deref()),
        ("<value>", slots.value.as_deref()),
        ("<entity>", slots.entity.as_deref()),
        ("<rel>", slots.rel.as_deref()),
        ("<place>", slots.place.as_deref()),
    ];
    let mut output = example.to_owned();
    let mut score = 0;
    for (placeholder, value) in replacements {
        if output.contains(placeholder)
            && let Some(value) = value
        {
            output = output.replace(placeholder, value);
            score += 1;
        }
    }
    (output, score)
}
