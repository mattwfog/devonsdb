//! Namespace-correct catalog grounding (`docs/NL.md` § 6).

use std::collections::BTreeSet;

use devondb::text::parser::{Parsed, parse};
use devondb::{
    ColumnSummary, Database, NodeTableSummary, RelTableSummary, SchemaSummary, Statement,
    did_you_mean, fold,
};

use crate::normalize::{Token, TokenKind, is_plan_number};
use crate::syntax::{column_ref, identifier, quote_string};
use crate::template::{
    AggregateFunction, Direction, FilterTokens, IntentTokens, MultiHopStyle, SortDirection,
    StatementIntentTokens, TimePhrase,
};
use crate::vocabulary::{Comparator, WITHIN_WORDS};

const IRREGULARS: [(&str, &str); 8] = [
    ("people", "person"),
    ("children", "child"),
    ("men", "man"),
    ("women", "woman"),
    ("feet", "foot"),
    ("teeth", "tooth"),
    ("geese", "goose"),
    ("mice", "mouse"),
];

/// The `k` a similar-to question emits when it names no count (`docs/NL.md`
/// § 18).
const SIMILAR_TO_DEFAULT_K: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroundIssue {
    pub(crate) token: usize,
    pub(crate) suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroundFailure {
    pub(crate) issues: Vec<GroundIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Predicate {
    pub(crate) binding: String,
    pub(crate) column: String,
    pub(crate) comparator: Comparator,
    pub(crate) literal: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroundedHop {
    pub(crate) rel: String,
    pub(crate) direction: Direction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MultiHopCap {
    pub(crate) blocked_token: usize,
    pub(crate) entity: usize,
    pub(crate) nearest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroundedTimePhrase {
    SinceLastWeek,
    LastDays(i64),
    Yesterday,
    Today,
    SinceDaysAgo(i64),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GroundedIntent {
    Search {
        table: String,
        binding: String,
        column: String,
        query: String,
    },
    List {
        table: String,
        binding: String,
    },
    Filter {
        table: String,
        binding: String,
        predicates: Vec<Predicate>,
    },
    Entity {
        table: String,
        binding: String,
        label: String,
        literal: String,
    },
    Traversal {
        table: String,
        binding: String,
        label: String,
        literal: String,
        rel: String,
        direction: Direction,
        far_label: String,
        predicates: Vec<Predicate>,
    },
    MultiHop {
        table: String,
        binding: String,
        label: String,
        literal: String,
        hops: [GroundedHop; 2],
        far_label: String,
    },
    Count {
        table: String,
        binding: String,
        count_column: String,
        predicates: Vec<Predicate>,
        group: Option<String>,
    },
    Aggregate {
        table: String,
        binding: String,
        function: AggregateFunction,
        column: String,
        predicates: Vec<Predicate>,
        group: Option<String>,
    },
    Top {
        table: String,
        binding: String,
        predicates: Vec<Predicate>,
        sort_column: String,
        direction: SortDirection,
        count: u64,
    },
    Within {
        base: Box<GroundedIntent>,
        column: String,
        center: String,
        meters: f64,
    },
    Time {
        base: Box<GroundedIntent>,
        column: String,
        phrase: GroundedTimePhrase,
        phrase_token: usize,
    },
    SimilarTo {
        table: String,
        label: String,
        literal: String,
        embedding: String,
        k: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GroundedStatement {
    Update {
        table: String,
        key_column: String,
        key: String,
        column: String,
        value: String,
    },
    Delete {
        table: String,
        key_column: String,
        key: String,
    },
}

pub(crate) fn ground_statement(
    intent: &StatementIntentTokens,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedStatement, GroundFailure> {
    match intent {
        StatementIntentTokens::Update {
            table,
            key,
            column,
            value,
        } => ground_update(*table, *key, *column, *value, tokens, schema),
        StatementIntentTokens::Delete { table, key } => ground_delete(*table, *key, tokens, schema),
    }
}

fn ground_update(
    table_index: usize,
    key_index: usize,
    column_index: usize,
    value_index: usize,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedStatement, GroundFailure> {
    let table = resolve_node(table_index, tokens, schema)?;
    let key_column = primary_key(table, table_index)?;
    let key = statement_literal(key_index, tokens, key_column, false)?;
    let column = resolve_column(column_index, tokens, table)?;
    if column.primary_key {
        return Err(failure(
            column_index,
            Some("primary-key updates are not supported; use delete plus insert".to_owned()),
        ));
    }
    let value = statement_literal(value_index, tokens, column, true)?;
    Ok(GroundedStatement::Update {
        table: table.name.clone(),
        key_column: key_column.name.clone(),
        key,
        column: column.name.clone(),
        value,
    })
}

fn ground_delete(
    table_index: usize,
    key_index: usize,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedStatement, GroundFailure> {
    let table = resolve_node(table_index, tokens, schema)?;
    let key_column = primary_key(table, table_index)?;
    let key = statement_literal(key_index, tokens, key_column, false)?;
    Ok(GroundedStatement::Delete {
        table: table.name.clone(),
        key_column: key_column.name.clone(),
        key,
    })
}

fn primary_key(
    table: &NodeTableSummary,
    table_index: usize,
) -> Result<&ColumnSummary, GroundFailure> {
    table
        .columns
        .iter()
        .find(|column| column.primary_key)
        .ok_or_else(|| {
            failure(
                table_index,
                Some("node table has no primary key".to_owned()),
            )
        })
}

fn statement_literal(
    index: usize,
    tokens: &[Token],
    column: &ColumnSummary,
    nullable: bool,
) -> Result<String, GroundFailure> {
    let token = &tokens[index];
    let literal = if column.ty == "String"
        && token.kind == TokenKind::Word
        && !matches!(token.folded.as_str(), "true" | "false" | "null")
    {
        quote_string(&token.original)
    } else {
        token.original.clone()
    };
    if literal_matches_type(&literal, &column.ty, nullable) {
        Ok(literal)
    } else {
        Err(failure(
            index,
            Some(format!("expected {} literal", column.ty)),
        ))
    }
}

fn literal_matches_type(literal: &str, expected: &str, nullable: bool) -> bool {
    let literal_statement = format!("delete from nl_literal where id = {literal}");
    let Ok(Parsed::Statement(literal_envelope)) = parse(&literal_statement) else {
        return false;
    };
    let Statement::DeleteNode { key, .. } = literal_envelope.stmt else {
        return false;
    };
    if key.logical_type().is_none() {
        return nullable;
    }
    let value_type = value_type_spelling(expected);
    let type_statement = format!("create node table nl_type (id {value_type} primary key)");
    let Ok(Parsed::Statement(type_envelope)) = parse(&type_statement) else {
        return false;
    };
    let Statement::CreateNodeTable { columns, .. } = type_envelope.stmt else {
        return false;
    };
    columns
        .first()
        .is_some_and(|column| key.matches_type(&column.ty))
}

fn value_type_spelling(column_type: &str) -> String {
    let Some(encoded) = column_type.strip_prefix("VectorEncoded(") else {
        return column_type.to_owned();
    };
    encoded.split_once(',').map_or_else(
        || column_type.to_owned(),
        |(dimension, _)| format!("Vector({dimension})"),
    )
}

pub(crate) fn ground(
    intent: &IntentTokens,
    tokens: &[Token],
    schema: &SchemaSummary,
    database: Option<&mut Database>,
) -> Result<GroundedIntent, GroundFailure> {
    match intent {
        IntentTokens::Search {
            table,
            column,
            query,
        } => ground_search(*table, *column, *query, tokens, schema),
        IntentTokens::List { table } => ground_list(*table, tokens, schema),
        IntentTokens::Filter { table, filters } => ground_filter(*table, filters, tokens, schema),
        IntentTokens::Entity { entity } => ground_entity(*entity, tokens, schema, database),
        IntentTokens::Traversal {
            direction,
            entity,
            rel,
            filters,
        } => ground_traversal(*direction, *entity, rel, filters, tokens, schema, database),
        IntentTokens::MultiHop {
            style,
            entity,
            rels,
            intermediate_table,
        } => ground_multi_hop(
            *style,
            *entity,
            rels,
            *intermediate_table,
            tokens,
            schema,
            database,
        ),
        IntentTokens::Count {
            table,
            filters,
            group,
        } => ground_count(*table, filters, *group, tokens, schema),
        IntentTokens::Aggregate {
            function,
            head,
            column,
            base,
            group,
        } => ground_aggregate(*function, *head, *column, base, *group, tokens, schema),
        IntentTokens::Top {
            direction,
            count,
            table,
            filters,
            sort_column,
        } => ground_top(
            *direction,
            *count,
            *table,
            filters,
            *sort_column,
            tokens,
            schema,
        ),
        IntentTokens::Within {
            base,
            within,
            distance,
            unit,
            place,
        } => ground_within(
            base, *within, *distance, *unit, place, tokens, schema, database,
        ),
        IntentTokens::Time {
            base,
            column,
            phrase,
            phrase_start,
        } => ground_time(base, *column, *phrase, *phrase_start, tokens, schema),
        IntentTokens::SimilarTo {
            count,
            table,
            entity,
            like,
        } => ground_similar_to(*count, *table, *entity, *like, tokens, schema, database),
    }
}

/// Grounds an explicit String column and a closed quoted search literal.
fn ground_search(
    table: usize,
    column: usize,
    query: usize,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedIntent, GroundFailure> {
    let table = resolve_node(table, tokens, schema)?;
    let column = resolve_exact_column(column, tokens, table)?;
    if column.ty != "String" {
        return Err(failure(
            query,
            Some("full-text search requires a String column".into()),
        ));
    }
    if tokens[query].kind != TokenKind::Quoted || !closed_quote(&tokens[query].original) {
        return Err(failure(
            query,
            Some("quote the search text with double quotes".into()),
        ));
    }
    Ok(GroundedIntent::Search {
        table: table.name.clone(),
        binding: binding(&table.name),
        column: column.name.clone(),
        query: tokens[query].original.clone(),
    })
}

/// Grounds `<table> similar to <entity>` (§ 18): the table derives its one
/// embedding column, the entity grounds through the same lookup the
/// traversal templates use, and the anchor must live in the target table.
#[allow(clippy::too_many_arguments)]
fn ground_similar_to(
    count: Option<usize>,
    table_index: usize,
    entity_index: usize,
    like: Option<usize>,
    tokens: &[Token],
    schema: &SchemaSummary,
    database: Option<&mut Database>,
) -> Result<GroundedIntent, GroundFailure> {
    if let Some(like) = like {
        return Err(failure(
            like,
            Some("`like` is not a synonym for `similar to`".to_owned()),
        ));
    }
    let k = match count {
        Some(index) => tokens[index]
            .original
            .parse::<u64>()
            .map_err(|_| failure(index, None))?,
        None => SIMILAR_TO_DEFAULT_K,
    };
    let table = resolve_node(table_index, tokens, schema)?;
    let embedding = embedding_column(table_index, table)?;
    let candidates = schema
        .node_tables
        .iter()
        .filter_map(|candidate| label_column(candidate, schema).map(|label| (candidate, label)))
        .collect::<Vec<_>>();
    let (anchor_table, label) = unique_entity_table(entity_index, candidates)?;
    if fold(&anchor_table.name) != fold(&table.name) {
        return Err(failure(
            entity_index,
            Some(format!(
                "cross-table similar-to is v2.1; the anchor must be in `{}`",
                table.name
            )),
        ));
    }
    let literal = ground_entity_literal(entity_index, tokens, anchor_table, label, database)?;
    Ok(GroundedIntent::SimilarTo {
        table: table.name.clone(),
        label: label.name.clone(),
        literal,
        embedding: embedding.name.clone(),
        k,
    })
}

/// The derived embedding: exactly one `Vector(d)` column on the table
/// (ONTOLOGY.md § 8.2). `VectorEncoded(d, …)` spells as `Vector(d)`.
fn embedding_column(
    table_index: usize,
    table: &NodeTableSummary,
) -> Result<&ColumnSummary, GroundFailure> {
    let vectors = table
        .columns
        .iter()
        .filter(|column| value_type_spelling(&column.ty).starts_with("Vector("))
        .collect::<Vec<_>>();
    match vectors.as_slice() {
        [embedding] => Ok(embedding),
        [] => Err(failure(
            table_index,
            Some(format!(
                "`{}` has no vector column to compare by",
                table.name
            )),
        )),
        candidates => Err(ambiguous_embedding(table_index, &table.name, candidates)),
    }
}

fn ambiguous_embedding(
    token: usize,
    table_name: &str,
    candidates: &[&ColumnSummary],
) -> GroundFailure {
    let names = candidates
        .iter()
        .map(|column| format!("`{}`", column.name))
        .collect::<Vec<_>>()
        .join(", ");
    failure(
        token,
        Some(format!(
            "ambiguous embedding on `{table_name}`: {names} — v2.1's explicit `embedding` declaration is the escape hatch"
        )),
    )
}

fn ground_time(
    base: &IntentTokens,
    column: Option<usize>,
    phrase: TimePhrase,
    phrase_start: usize,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedIntent, GroundFailure> {
    let base = ground(base, tokens, schema, None)?;
    let table_name = source_table(&base).ok_or_else(|| failure(phrase_start, None))?;
    let table = node_by_name(table_name, schema).ok_or_else(|| failure(phrase_start, None))?;
    let column_index = column.ok_or_else(|| {
        failure(
            phrase_start,
            Some("time phrase requires a named Int64 column".to_owned()),
        )
    })?;
    let column = resolve_exact_column(column_index, tokens, table)?;
    if column.ty != "Int64" {
        return Err(failure(
            column_index,
            Some(format!(
                "time column `{}` must be Int64, found {}",
                column.name, column.ty
            )),
        ));
    }
    let phrase = ground_time_phrase(phrase, tokens)?;
    Ok(GroundedIntent::Time {
        base: Box::new(base),
        column: column.name.clone(),
        phrase,
        phrase_token: phrase_start,
    })
}

fn ground_time_phrase(
    phrase: TimePhrase,
    tokens: &[Token],
) -> Result<GroundedTimePhrase, GroundFailure> {
    match phrase {
        TimePhrase::SinceLastWeek => Ok(GroundedTimePhrase::SinceLastWeek),
        TimePhrase::LastDays { count } => {
            positive_day_count(count, tokens).map(GroundedTimePhrase::LastDays)
        }
        TimePhrase::Yesterday => Ok(GroundedTimePhrase::Yesterday),
        TimePhrase::Today => Ok(GroundedTimePhrase::Today),
        TimePhrase::SinceDaysAgo { count } => {
            positive_day_count(count, tokens).map(GroundedTimePhrase::SinceDaysAgo)
        }
    }
}

fn positive_day_count(index: usize, tokens: &[Token]) -> Result<i64, GroundFailure> {
    let count = tokens[index]
        .original
        .parse::<i64>()
        .map_err(|_| failure(index, Some("day count must be a positive Int64".to_owned())))?;
    if count <= 0 {
        return Err(failure(
            index,
            Some("day count must be greater than zero".to_owned()),
        ));
    }
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
fn ground_within(
    base: &IntentTokens,
    within: usize,
    distance: usize,
    unit: usize,
    place: &std::ops::Range<usize>,
    tokens: &[Token],
    schema: &SchemaSummary,
    database: Option<&mut Database>,
) -> Result<GroundedIntent, GroundFailure> {
    let base = ground(base, tokens, schema, None)?;
    let Some(table_name) = source_table(&base) else {
        return Err(failure(
            within,
            Some("use `<table> within <number> <unit> of <place>`".to_owned()),
        ));
    };
    let table = node_by_name(table_name, schema).ok_or_else(|| failure(within, None))?;
    let column = locatable_column(table).ok_or_else(|| {
        failure(
            within,
            Some(format!(
                "`{}` is not Locatable: expected exactly one GeoPoint column",
                table.name
            )),
        )
    })?;
    let meters = ground_meters(distance, unit, tokens)?;
    let center = ground_place(place, tokens, schema, database)?;
    Ok(GroundedIntent::Within {
        base: Box::new(base),
        column: column.name.clone(),
        center,
        meters,
    })
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

fn locatable_column(table: &NodeTableSummary) -> Option<&ColumnSummary> {
    let mut geo = table
        .columns
        .iter()
        .filter(|column| column.ty == "GeoPoint");
    let column = geo.next()?;
    geo.next().is_none().then_some(column)
}

fn ground_meters(distance: usize, unit: usize, tokens: &[Token]) -> Result<f64, GroundFailure> {
    let number = &tokens[distance];
    if number.kind != TokenKind::Number || !valid_number(&number.original) {
        return Err(failure(distance, None));
    }
    let value = number
        .original
        .parse::<f64>()
        .map_err(|_| failure(distance, None))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(failure(distance, None));
    }
    let multiplier = unit_multiplier(&tokens[unit]).ok_or_else(|| {
        failure(
            unit,
            did_you_mean(&tokens[unit].original, WITHIN_WORDS.into_iter().skip(1)),
        )
    })?;
    let meters = value * multiplier;
    if !meters.is_finite() || meters <= 0.0 {
        return Err(failure(distance, None));
    }
    Ok(meters)
}

fn unit_multiplier(token: &Token) -> Option<f64> {
    match token.folded.as_str() {
        "mile" | "miles" | "mi" => Some(1609.344),
        "kilometer" | "kilometers" | "km" => Some(1000.0),
        "meter" | "meters" | "m" => Some(1.0),
        "foot" | "feet" | "ft" => Some(0.3048),
        _ => None,
    }
}

fn ground_place(
    place: &std::ops::Range<usize>,
    tokens: &[Token],
    schema: &SchemaSummary,
    database: Option<&mut Database>,
) -> Result<String, GroundFailure> {
    if place.len() != 1 {
        return Err(GroundFailure {
            issues: place
                .clone()
                .map(|token| GroundIssue {
                    token,
                    suggestion: None,
                })
                .collect(),
        });
    }
    let token = &tokens[place.start];
    if token.kind == TokenKind::Geo {
        return canonical_geo(token).ok_or_else(|| failure(place.start, None));
    }
    let literal = entity_literal(place.start, tokens)?;
    let matches = match database {
        Some(database) => matching_places(&literal, schema, database).map_err(|error| {
            failure(
                place.start,
                Some(format!("place grounding failed: {error}")),
            )
        })?,
        None => return Err(failure(place.start, None)),
    };
    if matches.candidates.is_empty() {
        let input = stored_spelling(&literal).unwrap_or(&literal);
        return Err(failure(
            place.start,
            did_you_mean(
                input,
                matches.values.iter().map(|value| value.spelling.as_str()),
            ),
        ));
    }
    unique_place(place.start, matches.candidates)
}

fn canonical_geo(token: &Token) -> Option<String> {
    let text = token.original.as_str();
    if text.len() < 6 || !token.folded.starts_with("geo(") || !text.ends_with(')') {
        return None;
    }
    let body = &text[4..text.len() - 1];
    let mut components = body.split(',').map(str::trim);
    let lat_text = components.next()?;
    let lng_text = components.next()?;
    if components.next().is_some() || !is_plan_number(lat_text) || !is_plan_number(lng_text) {
        return None;
    }
    let lat = lat_text.parse::<f64>().ok()?;
    let mut lng = lng_text.parse::<f64>().ok()?;
    if !lat.is_finite()
        || !lng.is_finite()
        || !(-90.0..=90.0).contains(&lat)
        || !(-180.0..=180.0).contains(&lng)
    {
        return None;
    }
    if lng == 180.0 {
        lng = -180.0;
    }
    if lat == 90.0 || lat == -90.0 {
        lng = 0.0;
    }
    Some(format!("geo({lat}, {lng})"))
}

#[derive(Debug)]
struct PlaceCandidate {
    description: String,
    center: Option<String>,
}

#[derive(Debug, Clone)]
struct StoredValue {
    spelling: String,
    literal: String,
}

#[derive(Debug)]
struct StoredValueMatches {
    candidates: Vec<StoredValue>,
    values: Vec<StoredValue>,
}

#[derive(Debug)]
struct PlaceMatches {
    candidates: Vec<PlaceCandidate>,
    values: Vec<StoredValue>,
}

fn unique_place(token: usize, candidates: Vec<PlaceCandidate>) -> Result<String, GroundFailure> {
    match candidates.as_slice() {
        [
            PlaceCandidate {
                center: Some(center),
                ..
            },
        ] => Ok(center.clone()),
        [] => Err(failure(token, Some("nothing found".to_owned()))),
        [candidate] => Err(failure(
            token,
            Some(format!("{} has no stored GeoPoint", candidate.description)),
        )),
        _ => Err(failure(
            token,
            Some(
                candidates
                    .iter()
                    .map(|candidate| candidate.description.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        )),
    }
}

fn matching_places(
    literal: &str,
    schema: &SchemaSummary,
    database: &mut Database,
) -> Result<PlaceMatches, String> {
    let mut candidates = Vec::new();
    let mut values = Vec::new();
    for table in &schema.node_tables {
        let Some(location) = locatable_column(table) else {
            continue;
        };
        let Some(label) = label_column(table, schema) else {
            continue;
        };
        let matches = matching_stored_values(table, label, literal, database)?;
        for value in matches.values {
            if !values
                .iter()
                .any(|stored: &StoredValue| stored.literal == value.literal)
            {
                values.push(value);
            }
        }
        for candidate in matches.candidates {
            candidates.extend(query_place_table(
                table,
                label,
                location,
                &candidate.literal,
                database,
            )?);
        }
    }
    Ok(PlaceMatches { candidates, values })
}

fn matching_stored_values(
    table: &NodeTableSummary,
    label: &ColumnSummary,
    literal: &str,
    database: &mut Database,
) -> Result<StoredValueMatches, String> {
    let binding = binding(&table.name);
    let text = format!(
        "nodes({}) as {} | project {}",
        identifier(&table.name),
        identifier(&binding),
        column_ref(&binding, &label.name),
    );
    let Parsed::Query(plan) = parse(&text).map_err(|error| error.to_string())? else {
        return Err("stored-value lookup parsed as a statement".to_owned());
    };
    let result = database.run(&plan).map_err(|error| error.to_string())?;
    let mut values = Vec::new();
    for row in &result.rows {
        let Some(rendered) = row.first().map(ToString::to_string) else {
            continue;
        };
        let Some(spelling) = stored_spelling(&rendered) else {
            continue;
        };
        if !values
            .iter()
            .any(|value: &StoredValue| value.literal == rendered)
        {
            values.push(StoredValue {
                spelling: spelling.to_owned(),
                literal: rendered,
            });
        }
    }
    let folded = fold(literal);
    let candidates = values
        .iter()
        .filter(|value| fold(&value.literal) == folded)
        .cloned()
        .collect();
    Ok(StoredValueMatches { candidates, values })
}

fn stored_spelling(literal: &str) -> Option<&str> {
    literal.strip_prefix('"')?.strip_suffix('"')
}

fn query_place_table(
    table: &NodeTableSummary,
    label: &ColumnSummary,
    location: &ColumnSummary,
    literal: &str,
    database: &mut Database,
) -> Result<Vec<PlaceCandidate>, String> {
    let binding = binding(&table.name);
    let key = table.columns.iter().find(|column| column.primary_key);
    let mut text = format!(
        "nodes({}) as {} | filter {} = {literal} | project ",
        identifier(&table.name),
        identifier(&binding),
        column_ref(&binding, &label.name),
    );
    if let Some(key) = key {
        text.push_str(&column_ref(&binding, &key.name));
        text.push_str(", ");
    }
    text.push_str(&column_ref(&binding, &location.name));
    let Parsed::Query(plan) = parse(&text).map_err(|error| error.to_string())? else {
        return Err("place lookup parsed as a statement".to_owned());
    };
    let result = database.run(&plan).map_err(|error| error.to_string())?;
    Ok(result
        .rows
        .iter()
        .enumerate()
        .filter_map(|(row_index, row)| {
            let location = row.last()?.to_string();
            let identity = key
                .and_then(|_| row.first())
                .map_or_else(|| format!("row {}", row_index + 1), ToString::to_string);
            Some(PlaceCandidate {
                description: format!("{}({identity})", table.name),
                center: location.starts_with("geo(").then_some(location),
            })
        })
        .collect())
}

fn ground_list(
    table_index: usize,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedIntent, GroundFailure> {
    let table = resolve_node(table_index, tokens, schema)?;
    Ok(GroundedIntent::List {
        table: table.name.clone(),
        binding: binding(&table.name),
    })
}

fn ground_filter(
    table_index: usize,
    filters: &[FilterTokens],
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedIntent, GroundFailure> {
    let table = resolve_node(table_index, tokens, schema)?;
    let binding = binding(&table.name);
    let predicates = ground_filters(filters, tokens, table, &binding)?;
    Ok(GroundedIntent::Filter {
        table: table.name.clone(),
        binding,
        predicates,
    })
}

fn ground_entity(
    entity: usize,
    tokens: &[Token],
    schema: &SchemaSummary,
    database: Option<&mut Database>,
) -> Result<GroundedIntent, GroundFailure> {
    let candidates = schema
        .node_tables
        .iter()
        .filter_map(|table| label_column(table, schema).map(|label| (table, label)))
        .collect::<Vec<_>>();
    let (table, label) = unique_entity_table(entity, candidates)?;
    let literal = ground_entity_literal(entity, tokens, table, label, database)?;
    Ok(GroundedIntent::Entity {
        table: table.name.clone(),
        binding: binding(&table.name),
        label: label.name.clone(),
        literal,
    })
}

fn unique_entity_table<'a>(
    token: usize,
    candidates: Vec<(&'a NodeTableSummary, &'a ColumnSummary)>,
) -> Result<(&'a NodeTableSummary, &'a ColumnSummary), GroundFailure> {
    match candidates.as_slice() {
        [candidate] => Ok(*candidate),
        [] => Err(failure(token, None)),
        _ => Err(ambiguous(
            token,
            candidates.iter().map(|(table, _)| table.name.as_str()),
        )),
    }
}

fn ground_traversal(
    direction: Direction,
    entity: usize,
    rel_range: &std::ops::Range<usize>,
    filters: &[FilterTokens],
    tokens: &[Token],
    schema: &SchemaSummary,
    database: Option<&mut Database>,
) -> Result<GroundedIntent, GroundFailure> {
    let (rel, direction) = resolve_rel(direction, rel_range, tokens, schema)?;
    let (center_name, far_name) = match direction {
        Direction::Out => (&rel.from, &rel.to),
        Direction::In => (&rel.to, &rel.from),
    };
    let center = node_by_name(center_name, schema).ok_or_else(|| failure(entity, None))?;
    let far = node_by_name(far_name, schema).ok_or_else(|| failure(entity, None))?;
    let label = label_column(center, schema).ok_or_else(|| failure(entity, None))?;
    let far_label = label_column(far, schema).ok_or_else(|| failure(entity, None))?;
    let literal = ground_entity_literal(entity, tokens, center, label, database)?;
    let predicates = ground_filters(filters, tokens, far, "other")?;
    Ok(GroundedIntent::Traversal {
        table: center.name.clone(),
        binding: binding(&center.name),
        label: label.name.clone(),
        literal,
        rel: rel.name.clone(),
        direction,
        far_label: far_label.name.clone(),
        predicates,
    })
}

#[derive(Debug, Clone)]
struct ResolvedHop<'a> {
    rel: &'a RelTableSummary,
    direction: Direction,
    range: std::ops::Range<usize>,
}

#[derive(Debug, Clone)]
struct ResolvedPath<'a> {
    hops: Vec<ResolvedHop<'a>>,
}

#[allow(clippy::too_many_arguments)]
fn ground_multi_hop(
    style: MultiHopStyle,
    entity: usize,
    rels: &std::ops::Range<usize>,
    intermediate_table: Option<usize>,
    tokens: &[Token],
    schema: &SchemaSummary,
    database: Option<&mut Database>,
) -> Result<GroundedIntent, GroundFailure> {
    let (paths, first_failure) = resolved_paths(style, rels, intermediate_table, 2, tokens, schema);
    let path = unique_path(paths, rels.start, first_failure)?;
    let (center_name, _) = hop_tables(&path.hops[0]);
    let (_, far_name) = hop_tables(&path.hops[1]);
    let center = node_by_name(center_name, schema).ok_or_else(|| failure(entity, None))?;
    let far = node_by_name(far_name, schema).ok_or_else(|| failure(entity, None))?;
    let label = label_column(center, schema).ok_or_else(|| failure(entity, None))?;
    let far_label = label_column(far, schema).ok_or_else(|| failure(entity, None))?;
    let literal = ground_entity_literal(entity, tokens, center, label, database)?;
    Ok(GroundedIntent::MultiHop {
        table: center.name.clone(),
        binding: binding(&center.name),
        label: label.name.clone(),
        literal,
        hops: [grounded_hop(&path.hops[0]), grounded_hop(&path.hops[1])],
        far_label: far_label.name.clone(),
    })
}

fn grounded_hop(hop: &ResolvedHop<'_>) -> GroundedHop {
    GroundedHop {
        rel: hop.rel.name.clone(),
        direction: hop.direction,
    }
}

fn unique_path<'a>(
    paths: Vec<ResolvedPath<'a>>,
    token: usize,
    first_failure: Option<GroundFailure>,
) -> Result<ResolvedPath<'a>, GroundFailure> {
    match paths.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(first_failure.unwrap_or_else(|| failure(token, None))),
        _ => Err(failure(
            token,
            Some("ambiguous two-hop relationship phrases".to_owned()),
        )),
    }
}

pub(crate) fn multi_hop_cap(
    intent: &IntentTokens,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Option<MultiHopCap> {
    let IntentTokens::MultiHop {
        style,
        entity,
        rels,
        intermediate_table,
    } = intent
    else {
        return None;
    };
    if entity_literal(*entity, tokens).is_err() {
        return None;
    }
    let intermediate = match intermediate_table {
        Some(index) => Some(resolve_node(*index, tokens, schema).ok()?.name.as_str()),
        None => None,
    };
    let mut search = CapSearch::new(*style, rels.clone(), intermediate, tokens, schema);
    let mut frontier = vec![None];
    for hop_count in 1..=rels.len() {
        frontier = search.advance(&frontier);
        if frontier.is_empty() {
            break;
        }
        if hop_count >= 3
            && let Some(path) = search.first_complete(&frontier)
            && path_can_project(&path, schema)
        {
            return cap_from_path(*style, *entity, *intermediate_table, &path, tokens);
        }
    }
    None
}

/// A linked prefix avoids copying every path while exploring the span DAG.
struct CapPrefix<'a> {
    previous: Option<usize>,
    hop: ResolvedHop<'a>,
}

// Each frontier contains the lexicographically first textual partition for
// (position, endpoint) at one hop count. Equivalent prefixes admit exactly
// the same suffixes, so discarding later prefixes preserves first-path order.
// At most O(n² * t) prefixes survive, where t is the number of endpoint names;
// outgoing spans are bounded by actual catalog phrase lengths, not a cutoff.
struct CapSearch<'a> {
    style: MultiHopStyle,
    range: std::ops::Range<usize>,
    intermediate: Option<&'a str>,
    spans: Vec<Vec<ResolvedHop<'a>>>,
    prefixes: Vec<CapPrefix<'a>>,
}

impl<'a> CapSearch<'a> {
    fn new(
        style: MultiHopStyle,
        range: std::ops::Range<usize>,
        intermediate: Option<&'a str>,
        tokens: &[Token],
        schema: &'a SchemaSummary,
    ) -> Self {
        let lengths = relationship_phrase_lengths(schema);
        let spans = range
            .clone()
            .map(|start| {
                lengths
                    .iter()
                    .filter_map(|length| {
                        let end = start.checked_add(*length).filter(|end| *end <= range.end)?;
                        let (rel, direction) =
                            resolve_rel(Direction::Out, &(start..end), tokens, schema).ok()?;
                        Some(ResolvedHop {
                            rel,
                            direction,
                            range: start..end,
                        })
                    })
                    .collect()
            })
            .collect();
        Self {
            style,
            range,
            intermediate,
            spans,
            prefixes: Vec::new(),
        }
    }

    fn advance(&mut self, frontier: &[Option<usize>]) -> Vec<Option<usize>> {
        let mut next = Vec::new();
        let mut seen = BTreeSet::new();
        for &previous in frontier {
            let start =
                previous.map_or(self.range.start, |index| self.prefixes[index].hop.range.end);
            if start == self.range.end {
                continue;
            }
            for hop in &self.spans[start - self.range.start] {
                if !self.accepts(previous, hop) {
                    continue;
                }
                let endpoint = match self.style {
                    MultiHopStyle::Clause => hop_tables(hop).1,
                    MultiHopStyle::Of => hop_tables(hop).0,
                };
                if !seen.insert((hop.range.end, fold(endpoint).into_owned())) {
                    continue;
                }
                let index = self.prefixes.len();
                self.prefixes.push(CapPrefix {
                    previous,
                    hop: hop.clone(),
                });
                next.push(Some(index));
            }
        }
        next
    }

    fn accepts(&self, previous: Option<usize>, hop: &ResolvedHop<'_>) -> bool {
        let (center, far) = hop_tables(hop);
        if let Some(index) = previous {
            let (old_center, old_far) = hop_tables(&self.prefixes[index].hop);
            let (left, right) = match self.style {
                MultiHopStyle::Clause => (old_far, center),
                MultiHopStyle::Of => (old_center, far),
            };
            if fold(left) != fold(right) {
                return false;
            }
        }
        let first_semantic_hop = match self.style {
            MultiHopStyle::Clause => previous.is_none(),
            MultiHopStyle::Of => hop.range.end == self.range.end,
        };
        !first_semantic_hop
            || self
                .intermediate
                .is_none_or(|table| fold(table) == fold(far))
    }

    fn first_complete(&self, frontier: &[Option<usize>]) -> Option<ResolvedPath<'a>> {
        let mut current = frontier
            .iter()
            .flatten()
            .copied()
            .find(|index| self.prefixes[*index].hop.range.end == self.range.end);
        current?;
        let mut hops = Vec::new();
        while let Some(index) = current {
            let prefix = &self.prefixes[index];
            hops.push(prefix.hop.clone());
            current = prefix.previous;
        }
        if matches!(self.style, MultiHopStyle::Clause) {
            hops.reverse();
        }
        Some(ResolvedPath { hops })
    }
}

fn relationship_phrase_lengths(schema: &SchemaSummary) -> BTreeSet<usize> {
    let mut lengths = schema
        .rel_tables
        .iter()
        .map(|rel| rel.name.split('_').count())
        .collect::<BTreeSet<_>>();
    if let Some(classes) = &schema.classes {
        for class in &classes.rel_classes {
            for phrase in [class.verb.as_deref(), class.inverse.as_deref()]
                .into_iter()
                .flatten()
            {
                let length = phrase.split_whitespace().count();
                if length != 0 {
                    lengths.insert(length);
                }
            }
        }
    }
    lengths
}

fn path_can_project(path: &ResolvedPath<'_>, schema: &SchemaSummary) -> bool {
    let Some(first) = path.hops.first() else {
        return false;
    };
    let Some(last) = path.hops.last() else {
        return false;
    };
    let (center, _) = hop_tables(first);
    let (_, far) = hop_tables(last);
    node_by_name(center, schema)
        .and_then(|table| label_column(table, schema))
        .is_some()
        && node_by_name(far, schema)
            .and_then(|table| label_column(table, schema))
            .is_some()
}

fn cap_from_path(
    style: MultiHopStyle,
    entity: usize,
    intermediate_table: Option<usize>,
    path: &ResolvedPath<'_>,
    tokens: &[Token],
) -> Option<MultiHopCap> {
    let blocked_token = path.hops.get(2)?.range.start;
    let inner = token_phrase(&path.hops[0].range, tokens);
    let outer = token_phrase(&path.hops[1].range, tokens);
    let nearest = match style {
        MultiHopStyle::Of => format!("{outer} of {inner} of {}", tokens[entity].original),
        MultiHopStyle::Clause => format!(
            "who do the {} {} {inner} {outer}",
            tokens[intermediate_table?].original, tokens[entity].original
        ),
    };
    Some(MultiHopCap {
        blocked_token,
        entity,
        nearest,
    })
}

fn token_phrase(range: &std::ops::Range<usize>, tokens: &[Token]) -> String {
    tokens[range.clone()]
        .iter()
        .map(|token| token.original.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolved_paths<'a>(
    style: MultiHopStyle,
    rels: &std::ops::Range<usize>,
    intermediate_table: Option<usize>,
    hop_count: usize,
    tokens: &[Token],
    schema: &'a SchemaSummary,
) -> (Vec<ResolvedPath<'a>>, Option<GroundFailure>) {
    let mut paths = Vec::new();
    let mut first_failure = None;
    for ranges in partition_ranges(rels, hop_count) {
        match resolve_path(style, ranges, intermediate_table, tokens, schema) {
            Ok(Some(path)) if !paths.iter().any(|other| same_path(other, &path)) => {
                paths.push(path);
            }
            Ok(_) => {}
            Err(error) => {
                first_failure.get_or_insert(error);
            }
        }
    }
    (paths, first_failure)
}

fn partition_ranges(
    range: &std::ops::Range<usize>,
    count: usize,
) -> Vec<Vec<std::ops::Range<usize>>> {
    if count == 0 || range.len() < count {
        return Vec::new();
    }
    let mut output = Vec::new();
    collect_partitions(range.start, range.end, count, &mut Vec::new(), &mut output);
    output
}

fn collect_partitions(
    start: usize,
    end: usize,
    remaining: usize,
    current: &mut Vec<std::ops::Range<usize>>,
    output: &mut Vec<Vec<std::ops::Range<usize>>>,
) {
    if remaining == 1 {
        current.push(start..end);
        output.push(current.clone());
        current.pop();
        return;
    }
    let last_split = end - (remaining - 1);
    for split in start + 1..=last_split {
        current.push(start..split);
        collect_partitions(split, end, remaining - 1, current, output);
        current.pop();
    }
}

fn resolve_path<'a>(
    style: MultiHopStyle,
    ranges: Vec<std::ops::Range<usize>>,
    intermediate_table: Option<usize>,
    tokens: &[Token],
    schema: &'a SchemaSummary,
) -> Result<Option<ResolvedPath<'a>>, GroundFailure> {
    let ordered = match style {
        MultiHopStyle::Of => ranges.into_iter().rev().collect::<Vec<_>>(),
        MultiHopStyle::Clause => ranges,
    };
    let mut hops = Vec::with_capacity(ordered.len());
    for range in ordered {
        let (rel, direction) = resolve_rel(Direction::Out, &range, tokens, schema)?;
        hops.push(ResolvedHop {
            rel,
            direction,
            range,
        });
    }
    if !path_endpoints_compose(&hops) {
        let token = hops.get(1).map_or(0, |hop| hop.range.start);
        return Err(failure(
            token,
            Some("relationship hop endpoints do not compose".to_owned()),
        ));
    }
    if let Some(index) = intermediate_table {
        let table = resolve_node(index, tokens, schema)?;
        let (_, expected) = hop_tables(&hops[0]);
        if fold(&table.name) != fold(expected) {
            return Err(failure(index, Some(expected.to_owned())));
        }
    }
    Ok(Some(ResolvedPath { hops }))
}

fn path_endpoints_compose(hops: &[ResolvedHop<'_>]) -> bool {
    hops.windows(2).all(|pair| {
        let (_, left_far) = hop_tables(&pair[0]);
        let (right_center, _) = hop_tables(&pair[1]);
        fold(left_far) == fold(right_center)
    })
}

fn hop_tables<'a>(hop: &'a ResolvedHop<'a>) -> (&'a str, &'a str) {
    match hop.direction {
        Direction::Out => (&hop.rel.from, &hop.rel.to),
        Direction::In => (&hop.rel.to, &hop.rel.from),
    }
}

fn same_path(left: &ResolvedPath<'_>, right: &ResolvedPath<'_>) -> bool {
    left.hops.len() == right.hops.len()
        && left.hops.iter().zip(&right.hops).all(|(left, right)| {
            left.direction == right.direction && fold(&left.rel.name) == fold(&right.rel.name)
        })
}

fn ground_entity_literal(
    entity: usize,
    tokens: &[Token],
    table: &NodeTableSummary,
    label: &ColumnSummary,
    database: Option<&mut Database>,
) -> Result<String, GroundFailure> {
    let literal = entity_literal(entity, tokens)?;
    let Some(database) = database else {
        return Ok(literal);
    };
    let matches = matching_stored_values(table, label, &literal, database)
        .map_err(|error| failure(entity, Some(format!("value grounding failed: {error}"))))?;
    unique_stored_value(entity, &literal, matches)
}

fn unique_stored_value(
    token: usize,
    literal: &str,
    matches: StoredValueMatches,
) -> Result<String, GroundFailure> {
    match matches.candidates.as_slice() {
        [candidate] => Ok(candidate.literal.clone()),
        [] => {
            let input = stored_spelling(literal).unwrap_or(literal);
            Err(failure(
                token,
                did_you_mean(
                    input,
                    matches.values.iter().map(|value| value.spelling.as_str()),
                ),
            ))
        }
        candidates => Err(ambiguous(
            token,
            candidates
                .iter()
                .map(|candidate| candidate.spelling.as_str()),
        )),
    }
}

fn ground_count(
    table_index: usize,
    filters: &[FilterTokens],
    group: Option<usize>,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedIntent, GroundFailure> {
    let table = resolve_node(table_index, tokens, schema)?;
    let binding = binding(&table.name);
    let predicates = ground_filters(filters, tokens, table, &binding)?;
    let count_column = table
        .columns
        .iter()
        .find(|column| column.primary_key)
        .ok_or_else(|| failure(table_index, None))?;
    let group = ground_group_column(group, &count_column.name, tokens, table)?;
    Ok(GroundedIntent::Count {
        table: table.name.clone(),
        binding,
        count_column: count_column.name.clone(),
        predicates,
        group,
    })
}

/// Grounds a § 17 aggregate over a Q1/Q2 entity-set. Column typing is the
/// engine's law: only grounding failures refuse here.
fn ground_aggregate(
    function: AggregateFunction,
    head: usize,
    column: Option<usize>,
    base: &IntentTokens,
    group: Option<usize>,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedIntent, GroundFailure> {
    let base = ground(base, tokens, schema, None)?;
    let (table_name, predicates) = match &base {
        GroundedIntent::List { table, .. } => (table.as_str(), Vec::new()),
        GroundedIntent::Filter {
            table, predicates, ..
        } => (table.as_str(), predicates.clone()),
        _ => return Err(failure(head, None)),
    };
    let table = node_by_name(table_name, schema).ok_or_else(|| failure(head, None))?;
    let column_index = column.ok_or_else(|| {
        failure(
            head,
            Some("aggregate head requires a column to aggregate".to_owned()),
        )
    })?;
    let column = resolve_exact_column(column_index, tokens, table)?;
    let group = ground_group_column(group, &column.name, tokens, table)?;
    Ok(GroundedIntent::Aggregate {
        table: table.name.clone(),
        binding: binding(&table.name),
        function,
        column: column.name.clone(),
        predicates,
        group,
    })
}

/// Resolves the optional `by <column>` group clause by exact fold. A group
/// column equal to the aggregated column refuses (§ 17).
fn ground_group_column(
    group: Option<usize>,
    aggregated: &str,
    tokens: &[Token],
    table: &NodeTableSummary,
) -> Result<Option<String>, GroundFailure> {
    let Some(group_index) = group else {
        return Ok(None);
    };
    let group = resolve_exact_column(group_index, tokens, table)?;
    if fold(&group.name) == fold(aggregated) {
        return Err(failure(
            group_index,
            Some("group column must differ from the aggregated column".to_owned()),
        ));
    }
    Ok(Some(group.name.clone()))
}

fn ground_top(
    direction: SortDirection,
    count_index: usize,
    table_index: usize,
    filters: &[FilterTokens],
    sort_index: usize,
    tokens: &[Token],
    schema: &SchemaSummary,
) -> Result<GroundedIntent, GroundFailure> {
    let table = resolve_node(table_index, tokens, schema)?;
    let count = tokens[count_index]
        .original
        .parse::<u64>()
        .map_err(|_| failure(count_index, None))?;
    let sort_column = resolve_column(sort_index, tokens, table)?;
    let binding = binding(&table.name);
    let predicates = ground_filters(filters, tokens, table, &binding)?;
    Ok(GroundedIntent::Top {
        table: table.name.clone(),
        binding,
        predicates,
        sort_column: sort_column.name.clone(),
        direction,
        count,
    })
}

fn ground_filters(
    filters: &[FilterTokens],
    tokens: &[Token],
    table: &NodeTableSummary,
    binding: &str,
) -> Result<Vec<Predicate>, GroundFailure> {
    filters
        .iter()
        .map(|filter| {
            let column = resolve_column(filter.column, tokens, table)?;
            let literal = typed_literal(filter.value, tokens, column)?;
            Ok(Predicate {
                binding: binding.to_owned(),
                column: column.name.clone(),
                comparator: filter.comparator,
                literal,
            })
        })
        .collect()
}

fn resolve_node<'a>(
    index: usize,
    tokens: &[Token],
    schema: &'a SchemaSummary,
) -> Result<&'a NodeTableSummary, GroundFailure> {
    let token = &tokens[index];
    let direct = direct_node_candidates(token, schema);
    if !direct.is_empty() {
        return unique(index, direct, &|table: &NodeTableSummary| {
            table.name.as_str()
        });
    }
    let plural = schema
        .node_tables
        .iter()
        .filter(|table| plural_equal(&token.folded, &table.name))
        .collect::<Vec<_>>();
    if !plural.is_empty() {
        return unique(index, plural, &|table: &NodeTableSummary| {
            table.name.as_str()
        });
    }
    let suggestion = did_you_mean(&token.original, node_grounding_names(schema));
    Err(failure(index, suggestion))
}

fn direct_node_candidates<'a>(
    token: &Token,
    schema: &'a SchemaSummary,
) -> Vec<&'a NodeTableSummary> {
    let mut candidates = schema
        .node_tables
        .iter()
        .filter(|table| fold(&table.name).as_ref() == token.folded)
        .collect::<Vec<_>>();
    let Some(classes) = &schema.classes else {
        return candidates;
    };
    for class in &classes.node_classes {
        let matches_display = fold(&class.display).as_ref() == token.folded;
        let matches_plural = class
            .plural
            .as_deref()
            .is_some_and(|plural| fold(plural).as_ref() == token.folded);
        if (matches_display || matches_plural)
            && let Some(table) = node_by_name(&class.table, schema)
            && !candidates
                .iter()
                .any(|candidate| fold(&candidate.name).as_ref() == fold(&table.name).as_ref())
        {
            candidates.push(table);
        }
    }
    candidates
}

fn node_grounding_names(schema: &SchemaSummary) -> Vec<&str> {
    let mut names = schema
        .node_tables
        .iter()
        .map(|table| table.name.as_str())
        .collect::<Vec<_>>();
    if let Some(classes) = &schema.classes {
        for class in &classes.node_classes {
            names.push(&class.display);
            names.extend(class.plural.as_deref());
        }
    }
    names
}

fn resolve_column<'a>(
    index: usize,
    tokens: &[Token],
    table: &'a NodeTableSummary,
) -> Result<&'a ColumnSummary, GroundFailure> {
    resolve_named(index, &tokens[index], &table.columns, |column| {
        column.name.as_str()
    })
}

/// Exact-fold column resolution with no plural morphology (§§ 16-17).
fn resolve_exact_column<'a>(
    index: usize,
    tokens: &[Token],
    table: &'a NodeTableSummary,
) -> Result<&'a ColumnSummary, GroundFailure> {
    let token = &tokens[index];
    let exact = table
        .columns
        .iter()
        .filter(|column| fold(&column.name).as_ref() == token.folded)
        .collect::<Vec<_>>();
    match exact.as_slice() {
        [column] => Ok(*column),
        [] => Err(failure(
            index,
            did_you_mean(
                &token.original,
                table.columns.iter().map(|column| column.name.as_str()),
            ),
        )),
        _ => Err(ambiguous(
            index,
            exact.iter().map(|column| column.name.as_str()),
        )),
    }
}

fn resolve_named<'a, T, F>(
    index: usize,
    token: &Token,
    candidates: &'a [T],
    name: F,
) -> Result<&'a T, GroundFailure>
where
    F: Fn(&T) -> &str,
{
    let exact = candidates
        .iter()
        .filter(|candidate| fold(name(candidate)).as_ref() == token.folded)
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return unique(index, exact, &name);
    }
    let plural = candidates
        .iter()
        .filter(|candidate| plural_equal(&token.folded, name(candidate)))
        .collect::<Vec<_>>();
    if !plural.is_empty() {
        return unique(index, plural, &name);
    }
    let suggestion = did_you_mean(&token.original, candidates.iter().map(&name));
    Err(failure(index, suggestion))
}

fn unique<'a, T, F>(index: usize, candidates: Vec<&'a T>, name: &F) -> Result<&'a T, GroundFailure>
where
    F: Fn(&T) -> &str,
{
    match candidates.as_slice() {
        [candidate] => Ok(*candidate),
        _ => Err(ambiguous(
            index,
            candidates.iter().map(|candidate| name(candidate)),
        )),
    }
}

#[derive(Debug, Clone, Copy)]
struct DeclaredRelMatch<'a> {
    rel: &'a RelTableSummary,
    inverse: bool,
}

fn resolve_rel<'a>(
    direction: Direction,
    range: &std::ops::Range<usize>,
    tokens: &[Token],
    schema: &'a SchemaSummary,
) -> Result<(&'a RelTableSummary, Direction), GroundFailure> {
    let phrase = tokens[range.clone()]
        .iter()
        .map(|token| token.folded.as_str())
        .collect::<Vec<_>>();
    let declared = declared_rel_matches(&phrase, schema);
    if !declared.is_empty() {
        let matched = unique_declared_rel(range.start, declared)?;
        let direction = if matched.inverse {
            opposite(direction)
        } else {
            direction
        };
        return Ok((matched.rel, direction));
    }
    let exact = schema
        .rel_tables
        .iter()
        .filter(|rel| rel_phrase_exact(&phrase, &rel.name))
        .collect::<Vec<_>>();
    let candidates = if exact.is_empty() {
        schema
            .rel_tables
            .iter()
            .filter(|rel| rel_phrase_equal(&phrase, &rel.name))
            .collect()
    } else {
        exact
    };
    if candidates.is_empty() {
        let input = phrase.join("_");
        let suggestion = did_you_mean(
            &input,
            schema.rel_tables.iter().map(|rel| rel.name.as_str()),
        );
        return Err(failure(range.start, suggestion));
    }
    unique(range.start, candidates, &|rel: &RelTableSummary| {
        rel.name.as_str()
    })
    .map(|rel| (rel, direction))
}

fn declared_rel_matches<'a>(
    phrase: &[&str],
    schema: &'a SchemaSummary,
) -> Vec<DeclaredRelMatch<'a>> {
    let mut matches = Vec::new();
    let Some(classes) = &schema.classes else {
        return matches;
    };
    for class in &classes.rel_classes {
        let Some(rel) = rel_by_name(&class.table, schema) else {
            continue;
        };
        if class
            .verb
            .as_deref()
            .is_some_and(|verb| declared_phrase_exact(phrase, verb))
        {
            matches.push(DeclaredRelMatch {
                rel,
                inverse: false,
            });
        }
        if class
            .inverse
            .as_deref()
            .is_some_and(|inverse| declared_phrase_exact(phrase, inverse))
        {
            matches.push(DeclaredRelMatch { rel, inverse: true });
        }
    }
    matches
}

fn unique_declared_rel(
    token: usize,
    candidates: Vec<DeclaredRelMatch<'_>>,
) -> Result<DeclaredRelMatch<'_>, GroundFailure> {
    match candidates.as_slice() {
        [candidate] => Ok(*candidate),
        _ => Err(ambiguous(
            token,
            candidates
                .iter()
                .map(|candidate| candidate.rel.name.as_str()),
        )),
    }
}

fn declared_phrase_exact(phrase: &[&str], declared: &str) -> bool {
    let parts = declared.split_whitespace().collect::<Vec<_>>();
    phrase.len() == parts.len()
        && phrase
            .iter()
            .zip(parts)
            .all(|(word, part)| *word == fold(part).as_ref())
}

fn opposite(direction: Direction) -> Direction {
    match direction {
        Direction::Out => Direction::In,
        Direction::In => Direction::Out,
    }
}

fn rel_phrase_exact(phrase: &[&str], name: &str) -> bool {
    let parts = name.split('_').collect::<Vec<_>>();
    phrase.len() == parts.len()
        && phrase
            .iter()
            .zip(parts)
            .all(|(word, part)| *word == fold(part).as_ref())
}

fn rel_phrase_equal(phrase: &[&str], name: &str) -> bool {
    let parts = name.split('_').collect::<Vec<_>>();
    phrase.len() == parts.len()
        && phrase
            .iter()
            .zip(parts)
            .all(|(word, part)| plural_equal(word, part))
}

fn plural_equal(left: &str, right: &str) -> bool {
    let left = plural_forms(left);
    let right = plural_forms(fold(right).as_ref());
    left.iter().any(|form| right.contains(form))
}

pub(crate) fn catalog_targets(token: &Token, schema: &SchemaSummary) -> Vec<String> {
    let names = catalog_names(schema);
    let mut direct = names
        .iter()
        .filter(|(surface, _)| fold(surface).as_ref() == token.folded)
        .map(|(_, target)| *target)
        .collect::<Vec<_>>();
    if let Some(classes) = &schema.classes {
        for class in &classes.node_classes {
            let matches_display = fold(&class.display).as_ref() == token.folded;
            let matches_plural = class
                .plural
                .as_deref()
                .is_some_and(|plural| fold(plural).as_ref() == token.folded);
            if (matches_display || matches_plural) && !direct.contains(&class.table.as_str()) {
                direct.push(&class.table);
            }
        }
    }
    let matches = if direct.is_empty() {
        names
            .into_iter()
            .filter(|(surface, _)| plural_equal(&token.folded, surface))
            .map(|(_, target)| target)
            .collect()
    } else {
        direct
    };
    let mut targets = Vec::new();
    for name in matches {
        if !targets.iter().any(|target| target == name) {
            targets.push(name.to_owned());
        }
    }
    targets
}

fn catalog_names(schema: &SchemaSummary) -> Vec<(&str, &str)> {
    let mut names = schema
        .node_tables
        .iter()
        .map(|table| (table.name.as_str(), table.name.as_str()))
        .chain(schema.rel_tables.iter().filter_map(|rel| {
            (!rel.name.contains('_')).then_some((rel.name.as_str(), rel.name.as_str()))
        }))
        .collect::<Vec<_>>();
    for table in &schema.node_tables {
        names.extend(
            table
                .columns
                .iter()
                .map(|column| (column.name.as_str(), column.name.as_str())),
        );
    }
    names
}

pub(crate) fn relationship_spans(
    tokens: &[Token],
    schema: &SchemaSummary,
) -> std::collections::BTreeMap<usize, String> {
    let mut targets = std::collections::BTreeMap::new();
    if let Some(classes) = &schema.classes {
        for class in &classes.rel_classes {
            let Some(rel) = rel_by_name(&class.table, schema) else {
                continue;
            };
            for phrase in [class.verb.as_deref(), class.inverse.as_deref()]
                .into_iter()
                .flatten()
            {
                mark_declared_relationship_spans(tokens, phrase, &rel.name, &mut targets);
            }
        }
    }
    for rel in &schema.rel_tables {
        let length = rel.name.split('_').count();
        if length > tokens.len() {
            continue;
        }
        for start in 0..=tokens.len().saturating_sub(length) {
            let phrase = tokens[start..start + length]
                .iter()
                .map(|token| token.folded.as_str())
                .collect::<Vec<_>>();
            if rel_phrase_exact(&phrase, &rel.name) || rel_phrase_equal(&phrase, &rel.name) {
                for index in start..start + length {
                    targets.entry(index).or_insert_with(|| rel.name.clone());
                }
            }
        }
    }
    targets
}

fn mark_declared_relationship_spans(
    tokens: &[Token],
    phrase: &str,
    target: &str,
    targets: &mut std::collections::BTreeMap<usize, String>,
) {
    let parts = phrase.split_whitespace().collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > tokens.len() {
        return;
    }
    for start in 0..=tokens.len().saturating_sub(parts.len()) {
        let candidate = tokens[start..start + parts.len()]
            .iter()
            .map(|token| token.folded.as_str())
            .collect::<Vec<_>>();
        if declared_phrase_exact(&candidate, phrase) {
            for index in start..start + parts.len() {
                targets.entry(index).or_insert_with(|| target.to_owned());
            }
        }
    }
}

fn plural_forms(word: &str) -> BTreeSet<String> {
    let mut forms = BTreeSet::from([word.to_owned()]);
    if let Some((_, singular)) = IRREGULARS.iter().find(|(plural, _)| *plural == word) {
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

fn binding(table_name: &str) -> String {
    let folded = fold(table_name).into_owned();
    if let Some((_, singular)) = IRREGULARS.iter().find(|(plural, _)| *plural == folded) {
        return (*singular).to_owned();
    }
    if let Some(stem) = folded.strip_suffix("ies") {
        return format!("{stem}y");
    }
    folded
        .strip_suffix("es")
        .or_else(|| folded.strip_suffix('s'))
        .unwrap_or(&folded)
        .to_owned()
}

fn label_column<'a>(
    table: &'a NodeTableSummary,
    schema: &SchemaSummary,
) -> Option<&'a ColumnSummary> {
    let declared = schema
        .classes
        .as_ref()
        .and_then(|classes| {
            classes
                .node_classes
                .iter()
                .find(|class| fold(&class.table).as_ref() == fold(&table.name).as_ref())
        })
        .and_then(|class| class.label.as_deref())
        .and_then(|label| {
            table
                .columns
                .iter()
                .find(|column| fold(&column.name).as_ref() == fold(label).as_ref())
        });
    if declared.is_some() {
        return declared;
    }
    ["name", "title", "label"].into_iter().find_map(|name| {
        table
            .columns
            .iter()
            .find(|column| column.ty == "String" && fold(&column.name).as_ref() == name)
    })
}

fn node_by_name<'a>(name: &str, schema: &'a SchemaSummary) -> Option<&'a NodeTableSummary> {
    schema
        .node_tables
        .iter()
        .find(|table| fold(&table.name) == fold(name))
}

fn rel_by_name<'a>(name: &str, schema: &'a SchemaSummary) -> Option<&'a RelTableSummary> {
    schema
        .rel_tables
        .iter()
        .find(|rel| fold(&rel.name) == fold(name))
}

fn typed_literal(
    index: usize,
    tokens: &[Token],
    column: &ColumnSummary,
) -> Result<String, GroundFailure> {
    let token = &tokens[index];
    match (column.ty.as_str(), token.kind) {
        ("Int64" | "Float64", TokenKind::Number) if valid_number(&token.original) => {
            Ok(token.original.clone())
        }
        ("String", TokenKind::Quoted) if closed_quote(&token.original) => {
            Ok(token.original.clone())
        }
        ("String", TokenKind::Word) if starts_uppercase(&token.original) => {
            Ok(quote_string(&token.original))
        }
        _ => Err(failure(index, None)),
    }
}

fn entity_literal(index: usize, tokens: &[Token]) -> Result<String, GroundFailure> {
    let token = &tokens[index];
    match token.kind {
        TokenKind::Quoted if closed_quote(&token.original) => Ok(token.original.clone()),
        TokenKind::Word if !token.original.is_empty() => Ok(quote_string(&token.original)),
        _ => Err(failure(index, None)),
    }
}

fn valid_number(text: &str) -> bool {
    if text.contains(['.', 'e', 'E']) {
        text.parse::<f64>().is_ok_and(f64::is_finite)
    } else {
        text.parse::<i64>().is_ok()
    }
}

fn starts_uppercase(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

fn closed_quote(text: &str) -> bool {
    text.len() >= 2 && text.ends_with('"')
}

fn failure(token: usize, suggestion: Option<String>) -> GroundFailure {
    GroundFailure {
        issues: vec![GroundIssue { token, suggestion }],
    }
}

fn ambiguous<'a>(token: usize, names: impl Iterator<Item = &'a str>) -> GroundFailure {
    failure(token, Some(names.collect::<Vec<_>>().join(", ")))
}

#[cfg(test)]
mod bounded_cap_tests {
    use super::*;
    use crate::normalize::normalize;
    use devondb::introspect::{ClassesSummary, RelClassSummary};

    fn schema() -> SchemaSummary {
        SchemaSummary {
            node_tables: ["P", "Q"]
                .into_iter()
                .map(|name| NodeTableSummary {
                    name: name.into(),
                    columns: Vec::new(),
                })
                .collect(),
            rel_tables: [
                ("x", "P", "P"),
                ("x_x", "P", "Q"),
                ("x_x_x", "Q", "P"),
                ("y", "Q", "P"),
            ]
            .into_iter()
            .map(|(name, from, to)| RelTableSummary {
                name: name.into(),
                from: from.into(),
                to: to.into(),
                columns: Vec::new(),
            })
            .collect(),
            classes: Some(ClassesSummary {
                interfaces: Vec::new(),
                node_classes: Vec::new(),
                rel_classes: vec![
                    RelClassSummary {
                        table: "x".into(),
                        verb: Some("friends".into()),
                        inverse: Some("back".into()),
                    },
                    RelClassSummary {
                        table: "x_x".into(),
                        verb: Some("ambiguous".into()),
                        inverse: None,
                    },
                    RelClassSummary {
                        table: "y".into(),
                        verb: Some("ambiguous".into()),
                        inverse: None,
                    },
                ],
            }),
            pins: Vec::new(),
        }
    }

    fn signature(path: &ResolvedPath<'_>) -> Vec<(String, Direction, std::ops::Range<usize>)> {
        path.hops
            .iter()
            .map(|hop| (hop.rel.name.clone(), hop.direction, hop.range.clone()))
            .collect()
    }

    #[test]
    fn frontier_preserves_exhaustive_first_partition_for_every_hop_count() {
        let schema = schema();
        for length in 1..=5_u32 {
            for encoding in 0..3_usize.pow(length) {
                let words = (0..length)
                    .map(|index| ["x", "y", "bad"][encoding / 3_usize.pow(index) % 3])
                    .collect::<Vec<_>>()
                    .join(" ");
                for style in [MultiHopStyle::Of, MultiHopStyle::Clause] {
                    for intermediate in [None, Some("P"), Some("Q")] {
                        compare_frontiers(&schema, &words, style, intermediate);
                    }
                }
            }
        }
        for words in [
            "x x x x x x x x",
            "back back back",
            "friends x back y",
            "ambiguous x x",
            "x ambiguous back",
        ] {
            for style in [MultiHopStyle::Of, MultiHopStyle::Clause] {
                for intermediate in [None, Some("P"), Some("Q")] {
                    compare_frontiers(&schema, words, style, intermediate);
                }
            }
        }
    }

    fn compare_frontiers(
        schema: &SchemaSummary,
        words: &str,
        style: MultiHopStyle,
        intermediate: Option<&str>,
    ) {
        let tokens = normalize(&format!("{words} {}", intermediate.unwrap_or("P")));
        let range = 0..tokens.len() - 1;
        let intermediate_index = intermediate.map(|_| range.end);
        let mut search = CapSearch::new(style, range.clone(), intermediate, &tokens, schema);
        let mut frontier = vec![None];
        for count in 1..=range.len() {
            frontier = search.advance(&frontier);
            let (legacy, _) =
                resolved_paths(style, &range, intermediate_index, count, &tokens, schema);
            assert_eq!(
                search.first_complete(&frontier).as_ref().map(signature),
                legacy.first().map(signature),
                "{words:?}, {style:?}, {intermediate:?}, {count}"
            );
        }
    }
}
