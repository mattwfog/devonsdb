//! Ordered, all-token-consuming template grammar (`docs/NL.md` §§ 3, 5).

use crate::normalize::Token;
use crate::vocabulary::{
    Comparator, comparator, is_action_list, is_filter_connector, is_list, is_question_head,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Out,
    In,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortDirection {
    Desc,
    Asc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MultiHopStyle {
    Of,
    Clause,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilterTokens {
    pub(crate) column: usize,
    pub(crate) comparator: Comparator,
    pub(crate) value: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimePhrase {
    SinceLastWeek,
    LastDays { count: usize },
    Yesterday,
    Today,
    SinceDaysAgo { count: usize },
}

/// The canonical aggregate function a head word maps to (`docs/NL.md` § 17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggregateFunction {
    Sum,
    Avg,
    Max,
    Min,
}

impl AggregateFunction {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Avg => "avg",
            Self::Max => "max",
            Self::Min => "min",
        }
    }

    /// The fixed output column name for the function (§ 17).
    pub(crate) fn output(self) -> &'static str {
        match self {
            Self::Sum => "total",
            Self::Avg => "average",
            Self::Max => "highest",
            Self::Min => "lowest",
        }
    }
}

/// Head-first aggregate heads: `total`, `average`, `highest`, `lowest` and
/// their synonyms (§ 17).
fn aggregate_head(token: &Token) -> Option<AggregateFunction> {
    match token.folded.as_str() {
        "total" | "sum" => Some(AggregateFunction::Sum),
        "average" | "mean" => Some(AggregateFunction::Avg),
        "highest" | "maximum" | "largest" | "max" => Some(AggregateFunction::Max),
        "lowest" | "minimum" | "smallest" | "min" => Some(AggregateFunction::Min),
        _ => None,
    }
    .filter(|_| token.kind == crate::normalize::TokenKind::Word)
}

/// Table-first aggregate heads: only `total` and `average` (§ 17 — the
/// synonym heads are head-first only).
fn table_first_head(token: &Token) -> Option<AggregateFunction> {
    match token.folded.as_str() {
        "total" => Some(AggregateFunction::Sum),
        "average" => Some(AggregateFunction::Avg),
        _ => None,
    }
    .filter(|_| token.kind == crate::normalize::TokenKind::Word)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IntentTokens {
    Search {
        table: usize,
        column: usize,
        query: usize,
    },
    List {
        table: usize,
    },
    Filter {
        table: usize,
        filters: Vec<FilterTokens>,
    },
    Entity {
        entity: usize,
    },
    Traversal {
        direction: Direction,
        entity: usize,
        rel: std::ops::Range<usize>,
        filters: Vec<FilterTokens>,
    },
    MultiHop {
        style: MultiHopStyle,
        entity: usize,
        rels: std::ops::Range<usize>,
        intermediate_table: Option<usize>,
    },
    Count {
        table: usize,
        filters: Vec<FilterTokens>,
        group: Option<usize>,
    },
    Aggregate {
        function: AggregateFunction,
        head: usize,
        column: Option<usize>,
        base: Box<IntentTokens>,
        group: Option<usize>,
    },
    Top {
        direction: SortDirection,
        count: usize,
        table: usize,
        filters: Vec<FilterTokens>,
        sort_column: usize,
    },
    Within {
        base: Box<IntentTokens>,
        within: usize,
        distance: usize,
        unit: usize,
        place: std::ops::Range<usize>,
    },
    Time {
        base: Box<IntentTokens>,
        column: Option<usize>,
        phrase: TimePhrase,
        phrase_start: usize,
    },
    SimilarTo {
        count: Option<usize>,
        table: usize,
        entity: usize,
        like: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatementIntentTokens {
    Update {
        table: usize,
        key: usize,
        column: usize,
        value: usize,
    },
    Delete {
        table: usize,
        key: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatementFamily {
    Update,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatementTemplate {
    pub(crate) family: StatementFamily,
    verb: &'static str,
    pub(crate) example: &'static str,
}

// Most-specific-first order is versioned by NL_VERSION. Update templates
// precede delete templates because they contain two independently typed
// literal slots and a catalog column slot.
pub(crate) const STATEMENT_TEMPLATES: [StatementTemplate; 4] = [
    StatementTemplate {
        family: StatementFamily::Update,
        verb: "set",
        example: "set <table> <pk-value> <column> to <value>",
    },
    StatementTemplate {
        family: StatementFamily::Update,
        verb: "change",
        example: "change <table> <pk-value> <column> to <value>",
    },
    StatementTemplate {
        family: StatementFamily::Delete,
        verb: "delete",
        example: "delete <table> <pk-value>",
    },
    StatementTemplate {
        family: StatementFamily::Delete,
        verb: "remove",
        example: "remove <table> <pk-value>",
    },
];

pub(crate) fn statement_matches(tokens: &[Token]) -> Vec<(usize, StatementIntentTokens)> {
    STATEMENT_TEMPLATES
        .iter()
        .enumerate()
        .filter_map(|(index, template)| {
            match_statement_template(template, tokens).map(|intent| (index, intent))
        })
        .collect()
}

fn match_statement_template(
    template: &StatementTemplate,
    tokens: &[Token],
) -> Option<StatementIntentTokens> {
    match template.family {
        StatementFamily::Update => {
            if tokens.len() != 6
                || !tokens.first()?.is_word(template.verb)
                || !tokens.get(4)?.is_word("to")
            {
                return None;
            }
            Some(StatementIntentTokens::Update {
                table: 1,
                key: 2,
                column: 3,
                value: 5,
            })
        }
        StatementFamily::Delete => (tokens.len() == 3 && tokens.first()?.is_word(template.verb))
            .then_some(StatementIntentTokens::Delete { table: 1, key: 2 }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateKind {
    Search,
    TopFiltered,
    CountFiltered,
    TraversalFilteredOut,
    TraversalFilteredIn,
    MultiHopClause,
    MultiHopOf,
    Filter,
    Top,
    Count,
    TraversalOut,
    TraversalIn,
    EntityWhoIs,
    List,
    EntityListVerb,
    BareTable,
    BareEntity,
    Within,
    Time,
    AggregateHead,
    AggregateTableFirst,
    CountBy,
    SimilarToCounted,
    SimilarToDefault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Template {
    kind: TemplateKind,
    pub(crate) example: &'static str,
}

// Most-specific-first order is versioned by NL_VERSION. The aggregate
// family (NL_VERSION 7, `docs/NL.md` § 17) is appended last so existing
// refusal-hint ranking, which breaks score ties by template order, is
// unchanged for the older families. The similar-to family (NL_VERSION 8,
// `docs/NL.md` § 18) follows the same rule; both families match ahead of
// the main loop in `matches` and win hint ranking through the `preferred`
// flag instead of position.
pub(crate) const TEMPLATES: [Template; 24] = [
    Template {
        kind: TemplateKind::TopFiltered,
        example: "top <n> <table> with <column> over <value> by <sort-column>",
    },
    Template {
        kind: TemplateKind::CountFiltered,
        example: "how many <table> have <column> over <value>",
    },
    Template {
        kind: TemplateKind::TraversalFilteredOut,
        example: "who does <entity> <rel> with <column> over <value>",
    },
    Template {
        kind: TemplateKind::TraversalFilteredIn,
        example: "who <rel> <entity> with <column> over <value>",
    },
    Template {
        kind: TemplateKind::MultiHopClause,
        example: "who do the <table> <entity> <rel> <rel>",
    },
    Template {
        kind: TemplateKind::MultiHopOf,
        example: "<rel> of <rel> of <entity>",
    },
    Template {
        kind: TemplateKind::Filter,
        example: "<table> with <column> over <value>",
    },
    Template {
        kind: TemplateKind::Top,
        example: "top <n> <table> by <column>",
    },
    Template {
        kind: TemplateKind::Count,
        example: "how many <table>",
    },
    Template {
        kind: TemplateKind::TraversalOut,
        example: "who does <entity> <rel>",
    },
    Template {
        kind: TemplateKind::TraversalIn,
        example: "who <rel> <entity>",
    },
    Template {
        kind: TemplateKind::EntityWhoIs,
        example: "who is <entity>",
    },
    Template {
        kind: TemplateKind::List,
        example: "show <table>",
    },
    Template {
        kind: TemplateKind::EntityListVerb,
        example: "show <entity>",
    },
    // Bare names are deliberately last. The table form precedes the entity
    // form because catalog tables are enumerable while entity values are open.
    Template {
        kind: TemplateKind::BareTable,
        example: "<table>",
    },
    Template {
        kind: TemplateKind::BareEntity,
        example: "<entity>",
    },
    Template {
        kind: TemplateKind::Within,
        example: "<table> within <n> miles of <place>",
    },
    Template {
        kind: TemplateKind::Time,
        example: "<table> with <column> since last week",
    },
    Template {
        kind: TemplateKind::AggregateHead,
        example: "total <column> of <table>",
    },
    Template {
        kind: TemplateKind::AggregateTableFirst,
        example: "<table> total <column>",
    },
    Template {
        kind: TemplateKind::CountBy,
        example: "how many <table> by <column>",
    },
    Template {
        kind: TemplateKind::SimilarToCounted,
        example: "top <n> <table> similar to <entity>",
    },
    Template {
        kind: TemplateKind::SimilarToDefault,
        example: "<table> similar to <entity>",
    },
    Template {
        kind: TemplateKind::Search,
        example: "search <table> <column> for \"<literal>\"",
    },
];

pub(crate) fn matches(tokens: &[Token]) -> Vec<(usize, IntentTokens)> {
    // Historical: `time_index`/`within_index` trail the appended § 17/§ 18
    // families; their effective values (CountBy/AggregateTableFirst slots)
    // are pinned by the corpus refusal hints and must not drift.
    let time_index = TEMPLATES.len() - 4;
    let time = match_time(tokens);
    if !time.is_empty() {
        return time
            .into_iter()
            .map(|intent| (time_index, intent))
            .collect();
    }
    let within_index = TEMPLATES.len() - 5;
    let within = match_within(tokens);
    if !within.is_empty() {
        return within
            .into_iter()
            .map(|intent| (within_index, intent))
            .collect();
    }
    let similar = match_similar_to(tokens);
    if !similar.is_empty() {
        return similar
            .into_iter()
            .map(|intent| (similar_to_index(&intent), intent))
            .collect();
    }
    let mut candidates = Vec::new();
    for (index, template) in TEMPLATES.iter().enumerate() {
        match template.kind {
            TemplateKind::AggregateHead => candidates.extend(
                match_aggregate_head(tokens)
                    .into_iter()
                    .map(|intent| (index, intent)),
            ),
            TemplateKind::AggregateTableFirst => candidates.extend(
                match_aggregate_table_first(tokens)
                    .into_iter()
                    .map(|intent| (index, intent)),
            ),
            TemplateKind::CountBy => {
                if let Some(intent) = match_count_by(tokens) {
                    candidates.push((index, intent));
                }
            }
            _ => {
                if let Some(intent) = match_template(template.kind, tokens) {
                    candidates.push((index, intent));
                }
            }
        }
    }
    candidates
}

fn match_template(kind: TemplateKind, tokens: &[Token]) -> Option<IntentTokens> {
    match kind {
        TemplateKind::Search => {
            (tokens.len() == 5 && tokens[0].is_word("search") && tokens[3].is_word("for"))
                .then_some(IntentTokens::Search {
                    table: 1,
                    column: 2,
                    query: 4,
                })
        }
        TemplateKind::TopFiltered => match_top(tokens, true),
        TemplateKind::CountFiltered => match_count(tokens, true),
        TemplateKind::TraversalFilteredOut => match_traversal_out(tokens, true),
        TemplateKind::TraversalFilteredIn => match_traversal_in(tokens, true),
        TemplateKind::MultiHopClause => match_multi_hop_clause(tokens),
        TemplateKind::MultiHopOf => match_multi_hop_of(tokens),
        TemplateKind::Filter => match_filter(tokens),
        TemplateKind::Top => match_top(tokens, false),
        TemplateKind::Count => match_count(tokens, false),
        TemplateKind::TraversalOut => match_traversal_out(tokens, false),
        TemplateKind::TraversalIn => match_traversal_in(tokens, false),
        TemplateKind::EntityWhoIs => match_entity_who_is(tokens),
        TemplateKind::List => match_list(tokens),
        TemplateKind::EntityListVerb => match_entity_list_verb(tokens),
        TemplateKind::BareTable => match_bare_table(tokens),
        TemplateKind::BareEntity => match_bare_entity(tokens),
        TemplateKind::Within
        | TemplateKind::Time
        | TemplateKind::AggregateHead
        | TemplateKind::AggregateTableFirst
        | TemplateKind::CountBy
        | TemplateKind::SimilarToCounted
        | TemplateKind::SimilarToDefault => None,
    }
}

/// The template slot a similar-to intent reports as: counted shapes pin the
/// `top <n>` example, default shapes the bare one (`docs/NL.md` § 18).
fn similar_to_index(intent: &IntentTokens) -> usize {
    match intent {
        IntentTokens::SimilarTo { count: Some(_), .. } => TEMPLATES.len() - 3,
        _ => TEMPLATES.len() - 2,
    }
}

/// `<table> similar to <entity>` and its counted forms (§ 18). The count
/// slot accepts any single token so grounding can refuse number words with
/// the digits-only rule; the `like` near-synonym matches the same shape so
/// grounding can refuse it by name. Neither compiles.
fn match_similar_to(tokens: &[Token]) -> Vec<IntentTokens> {
    let Some((junction, like)) = similar_junction(tokens) else {
        return Vec::new();
    };
    let entity = junction + if like.is_some() { 1 } else { 2 };
    if entity + 1 != tokens.len() {
        return Vec::new();
    }
    let (count, table) = match junction {
        1 => (None, 0),
        2 if is_action_list(&tokens[0]) => (None, 1),
        2 => (Some(0), 1),
        3 if tokens[0].is_word("top") => (Some(1), 2),
        _ => return Vec::new(),
    };
    vec![IntentTokens::SimilarTo {
        count,
        table,
        entity,
        like,
    }]
}

/// Finds the junction word: `similar` followed by `to`, or the refused
/// `like` near-synonym (its token index is returned for the named refusal).
fn similar_junction(tokens: &[Token]) -> Option<(usize, Option<usize>)> {
    for (index, token) in tokens.iter().enumerate() {
        if token.is_word("similar") && tokens.get(index + 1).is_some_and(|next| next.is_word("to"))
        {
            return Some((index, None));
        }
        if token.is_word("like") {
            return Some((index, Some(index)));
        }
    }
    None
}

fn match_multi_hop_clause(tokens: &[Token]) -> Option<IntentTokens> {
    if tokens.len() < 6 || !is_question_head(&tokens[0]) || !tokens[1].is_word("do") {
        return None;
    }
    Some(IntentTokens::MultiHop {
        style: MultiHopStyle::Clause,
        entity: 3,
        rels: 4..tokens.len(),
        intermediate_table: Some(2),
    })
}

fn match_multi_hop_of(tokens: &[Token]) -> Option<IntentTokens> {
    if tokens.len() < 3 || tokens.first().is_some_and(is_question_head) {
        return None;
    }
    Some(IntentTokens::MultiHop {
        style: MultiHopStyle::Of,
        entity: tokens.len() - 1,
        rels: 0..tokens.len() - 1,
        intermediate_table: None,
    })
}

fn match_within(tokens: &[Token]) -> Vec<IntentTokens> {
    let positions = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| token.is_word("within").then_some(index))
        .collect::<Vec<_>>();
    let [within] = positions.as_slice() else {
        return Vec::new();
    };
    if *within == 0 || within + 3 >= tokens.len() {
        return Vec::new();
    }
    base_matches(&tokens[..*within])
        .into_iter()
        .map(|base| IntentTokens::Within {
            base: Box::new(base),
            within: *within,
            distance: within + 1,
            unit: within + 2,
            place: within + 3..tokens.len(),
        })
        .collect()
}

fn match_time(tokens: &[Token]) -> Vec<IntentTokens> {
    let Some((phrase_start, phrase)) = time_phrase(tokens) else {
        return Vec::new();
    };
    let prefix = &tokens[..phrase_start];
    let mut matches = Vec::new();
    for base in entity_set_matches(prefix) {
        matches.push(IntentTokens::Time {
            base: Box::new(base),
            column: None,
            phrase,
            phrase_start,
        });
    }
    let Some(column) = phrase_start.checked_sub(1) else {
        return matches;
    };
    let base_end = column
        .checked_sub(1)
        .filter(|index| matches!(tokens[*index].folded.as_str(), "with" | "and"))
        .unwrap_or(column);
    for base in entity_set_matches(&tokens[..base_end]) {
        matches.insert(
            0,
            IntentTokens::Time {
                base: Box::new(base),
                column: Some(column),
                phrase,
                phrase_start,
            },
        );
    }
    matches
}

fn time_phrase(tokens: &[Token]) -> Option<(usize, TimePhrase)> {
    let length = tokens.len();
    if length >= 3
        && tokens[length - 3].is_word("since")
        && tokens[length - 2].is_word("last")
        && tokens[length - 1].is_word("week")
    {
        return Some((length - 3, TimePhrase::SinceLastWeek));
    }
    if length >= 4
        && tokens[length - 4].is_word("in")
        && tokens[length - 3].is_word("last")
        && matches!(tokens[length - 1].folded.as_str(), "day" | "days")
    {
        return Some((length - 4, TimePhrase::LastDays { count: length - 2 }));
    }
    if length >= 4
        && tokens[length - 4].is_word("since")
        && matches!(tokens[length - 2].folded.as_str(), "day" | "days")
        && tokens[length - 1].is_word("ago")
    {
        return Some((length - 4, TimePhrase::SinceDaysAgo { count: length - 3 }));
    }
    match tokens.last()?.folded.as_str() {
        "yesterday" => Some((length - 1, TimePhrase::Yesterday)),
        "today" => Some((length - 1, TimePhrase::Today)),
        _ => None,
    }
}

/// The `<entity-set>` source of §§ 16-17: a Q1 list source or a Q2
/// property-filter source.
fn entity_set_matches(tokens: &[Token]) -> Vec<IntentTokens> {
    base_matches(tokens)
        .into_iter()
        .filter(|intent| {
            matches!(
                intent,
                IntentTokens::List { .. } | IntentTokens::Filter { .. }
            )
        })
        .collect()
}

fn base_matches(tokens: &[Token]) -> Vec<IntentTokens> {
    TEMPLATES
        .iter()
        .filter(|template| {
            !matches!(
                template.kind,
                TemplateKind::Within
                    | TemplateKind::Time
                    | TemplateKind::AggregateHead
                    | TemplateKind::AggregateTableFirst
                    | TemplateKind::CountBy
                    | TemplateKind::SimilarToCounted
                    | TemplateKind::SimilarToDefault
            )
        })
        .filter_map(|template| match_template(template.kind, tokens))
        .collect()
}

impl Template {
    pub(crate) fn is_within(self) -> bool {
        self.kind == TemplateKind::Within
    }

    pub(crate) fn is_time(self) -> bool {
        self.kind == TemplateKind::Time
    }

    pub(crate) fn is_aggregate(self) -> bool {
        matches!(
            self.kind,
            TemplateKind::AggregateHead | TemplateKind::AggregateTableFirst | TemplateKind::CountBy
        )
    }

    pub(crate) fn is_similar_to(self) -> bool {
        matches!(
            self.kind,
            TemplateKind::SimilarToCounted | TemplateKind::SimilarToDefault
        )
    }

    pub(crate) fn is_multi_hop(self) -> bool {
        matches!(
            self.kind,
            TemplateKind::MultiHopClause | TemplateKind::MultiHopOf
        )
    }
}

impl IntentTokens {
    pub(crate) fn multi_hop_rels(&self) -> Option<&std::ops::Range<usize>> {
        match self {
            Self::MultiHop { rels, .. } => Some(rels),
            Self::Time { base, .. } => base.multi_hop_rels(),
            Self::Aggregate { base, .. } => base.multi_hop_rels(),
            _ => None,
        }
    }
}

fn match_top(tokens: &[Token], filtered: bool) -> Option<IntentTokens> {
    if tokens.len() < 5 {
        return None;
    }
    let direction = match tokens.first()?.folded.as_str() {
        "top" | "first" => SortDirection::Desc,
        "bottom" | "last" => SortDirection::Asc,
        _ => return None,
    };
    let by = tokens.iter().rposition(|token| token.is_word("by"))?;
    if by + 2 != tokens.len() {
        return None;
    }
    let filters = parse_optional_filters(tokens, 3, by)?;
    if filtered == filters.is_empty() {
        return None;
    }
    Some(IntentTokens::Top {
        direction,
        count: 1,
        table: 2,
        filters,
        sort_column: by + 1,
    })
}

fn match_count(tokens: &[Token], filtered: bool) -> Option<IntentTokens> {
    let table = count_head(tokens)?;
    if table >= tokens.len() {
        return None;
    }
    let filters = parse_optional_filters(tokens, table + 1, tokens.len())?;
    if filtered == filters.is_empty() {
        return None;
    }
    Some(IntentTokens::Count {
        table,
        filters,
        group: None,
    })
}

/// `how many <entity-set> by <column>` — count per group (§ 17). The
/// trailing `by` here is GROUP, never Q6 sort; Q6 requires a `top` head.
fn match_count_by(tokens: &[Token]) -> Option<IntentTokens> {
    let table = count_head(tokens)?;
    let by = tokens.iter().rposition(|token| token.is_word("by"))?;
    if by + 2 != tokens.len() || by <= table {
        return None;
    }
    let filters = parse_optional_filters(tokens, table + 1, by)?;
    Some(IntentTokens::Count {
        table,
        filters,
        group: Some(by + 1),
    })
}

/// `<head> <column> [for] <entity-set> [by <column2>]` (§ 17).
fn match_aggregate_head(tokens: &[Token]) -> Vec<IntentTokens> {
    let Some(function) = tokens.first().and_then(aggregate_head) else {
        return Vec::new();
    };
    let (column, mut start) = match tokens.get(1) {
        Some(token) if !is_aggregate_syntax(token) => (Some(1), 2),
        _ => (None, 1),
    };
    if tokens.get(start).is_some_and(|token| token.is_word("for")) {
        start += 1;
    }
    let Some((set, group)) = split_group(tokens, start) else {
        return Vec::new();
    };
    entity_set_matches(&tokens[set.clone()])
        .into_iter()
        .map(|base| IntentTokens::Aggregate {
            function,
            head: 0,
            column,
            base: Box::new(offset_base(base, set.start)),
            group,
        })
        .collect()
}

/// The entity-set matchers index into their token slice; a head-first
/// aggregate's entity-set starts mid-question, so slot indices shift.
fn offset_base(base: IntentTokens, offset: usize) -> IntentTokens {
    match base {
        IntentTokens::List { table } => IntentTokens::List {
            table: table + offset,
        },
        IntentTokens::Filter { table, filters } => IntentTokens::Filter {
            table: table + offset,
            filters: filters
                .into_iter()
                .map(|filter| FilterTokens {
                    column: filter.column + offset,
                    comparator: filter.comparator,
                    value: filter.value + offset,
                })
                .collect(),
        },
        other => other,
    }
}

/// `<entity-set> total|average <column> [by <column2>]` (§ 17).
fn match_aggregate_table_first(tokens: &[Token]) -> Vec<IntentTokens> {
    let Some(head) = tokens
        .iter()
        .skip(1)
        .position(|token| table_first_head(token).is_some())
        .map(|index| index + 1)
    else {
        return Vec::new();
    };
    let Some(function) = table_first_head(&tokens[head]) else {
        return Vec::new();
    };
    let (column, start) = match tokens.get(head + 1) {
        Some(token) if !is_aggregate_syntax(token) => (Some(head + 1), head + 2),
        _ => (None, head + 1),
    };
    let group = match &tokens[start..] {
        [] => None,
        [by, _] if by.is_word("by") => Some(start + 1),
        _ => return Vec::new(),
    };
    entity_set_matches(&tokens[..head])
        .into_iter()
        .map(|base| IntentTokens::Aggregate {
            function,
            head,
            column,
            base: Box::new(base),
            group,
        })
        .collect()
}

/// Splits an entity-set slice from a trailing `by <column>` group clause.
/// A `by` that is not exactly one token from the end is left inside the
/// slice so the entity-set match — and with it the template — fails (R2).
fn split_group(tokens: &[Token], start: usize) -> Option<(std::ops::Range<usize>, Option<usize>)> {
    let by = tokens[start..]
        .iter()
        .rposition(|token| token.is_word("by"))
        .map(|index| index + start);
    match by {
        Some(by) if by + 2 == tokens.len() && by > start => Some((start..by, Some(by + 1))),
        Some(_) => None,
        None => Some((start..tokens.len(), None)),
    }
}

fn is_aggregate_syntax(token: &Token) -> bool {
    if token.kind != crate::normalize::TokenKind::Word {
        return false;
    }
    matches!(token.folded.as_str(), "for" | "by" | "and") || is_filter_connector(token)
}

fn count_head(tokens: &[Token]) -> Option<usize> {
    if tokens.first()?.is_word("how") && tokens.get(1)?.is_word("many") {
        Some(2)
    } else if tokens.first()?.is_word("count") || tokens.first()?.is_word("number") {
        Some(1)
    } else {
        None
    }
}

fn match_filter(tokens: &[Token]) -> Option<IntentTokens> {
    let table = usize::from(tokens.first().is_some_and(is_list));
    if table >= tokens.len() {
        return None;
    }
    let filters = parse_optional_filters(tokens, table + 1, tokens.len())?;
    (!filters.is_empty()).then_some(IntentTokens::Filter { table, filters })
}

fn parse_optional_filters(tokens: &[Token], start: usize, end: usize) -> Option<Vec<FilterTokens>> {
    if start == end {
        return Some(Vec::new());
    }
    if start > end || !tokens.get(start).is_some_and(is_filter_connector) {
        return None;
    }
    parse_filters(tokens, start + 1, end)
}

fn parse_filters(tokens: &[Token], mut index: usize, end: usize) -> Option<Vec<FilterTokens>> {
    let mut filters = Vec::new();
    while index < end {
        let column = index;
        let (comparison, consumed) = comparator(tokens, index + 1)?;
        let value = index + 1 + consumed;
        if value >= end {
            return None;
        }
        filters.push(FilterTokens {
            column,
            comparator: comparison,
            value,
        });
        index = value + 1;
        if index == end {
            break;
        }
        if !tokens.get(index)?.is_word("and") {
            return None;
        }
        index += 1;
    }
    Some(filters)
}

fn match_traversal_out(tokens: &[Token], filtered: bool) -> Option<IntentTokens> {
    if tokens.len() < 4 || !is_question_head(&tokens[0]) || !tokens[1].is_word("does") {
        return None;
    }
    let filter_start = tokens
        .iter()
        .enumerate()
        .skip(3)
        .find_map(|(index, token)| is_filter_connector(token).then_some(index));
    let rel_end = filter_start.unwrap_or(tokens.len());
    if rel_end == 3 {
        return None;
    }
    let filters = parse_optional_filters(tokens, rel_end, tokens.len())?;
    if filtered == filters.is_empty() {
        return None;
    }
    Some(IntentTokens::Traversal {
        direction: Direction::Out,
        entity: 2,
        rel: 3..rel_end,
        filters,
    })
}

fn match_traversal_in(tokens: &[Token], filtered: bool) -> Option<IntentTokens> {
    if tokens.len() < 3 || !is_question_head(&tokens[0]) || tokens[1].is_word("does") {
        return None;
    }
    let filter_start = tokens
        .iter()
        .enumerate()
        .skip(2)
        .find_map(|(index, token)| is_filter_connector(token).then_some(index));
    let entity = filter_start.unwrap_or(tokens.len()).checked_sub(1)?;
    if entity <= 1 {
        return None;
    }
    let filters = parse_optional_filters(tokens, entity + 1, tokens.len())?;
    if filtered == filters.is_empty() {
        return None;
    }
    Some(IntentTokens::Traversal {
        direction: Direction::In,
        entity,
        rel: 1..entity,
        filters,
    })
}

fn match_entity_who_is(tokens: &[Token]) -> Option<IntentTokens> {
    (tokens.len() == 3 && is_question_head(&tokens[0]) && tokens[1].is_word("is"))
        .then_some(IntentTokens::Entity { entity: 2 })
}

fn match_list(tokens: &[Token]) -> Option<IntentTokens> {
    (tokens.len() == 2 && is_list(&tokens[0])).then_some(IntentTokens::List { table: 1 })
}

fn match_entity_list_verb(tokens: &[Token]) -> Option<IntentTokens> {
    (tokens.len() == 2 && is_action_list(&tokens[0])).then_some(IntentTokens::Entity { entity: 1 })
}

fn match_bare_table(tokens: &[Token]) -> Option<IntentTokens> {
    (tokens.len() == 1).then_some(IntentTokens::List { table: 0 })
}

fn match_bare_entity(tokens: &[Token]) -> Option<IntentTokens> {
    (tokens.len() == 1).then_some(IntentTokens::Entity { entity: 0 })
}
