//! Closed vocabulary tables. Their contents are part of [`crate::NL_VERSION`].

use std::collections::BTreeSet;

use crate::normalize::{Token, TokenKind};

pub(crate) const LIST_VERBS: [&str; 9] = [
    "show", "list", "find", "get", "display", "give", "who", "what", "which",
];
pub(crate) const COUNT_WORDS: [&str; 4] = ["how", "many", "count", "number"];
pub(crate) const COMPARATOR_WORDS: [&str; 18] = [
    "over", "above", "more", "greater", "after", "under", "below", "less", "fewer", "before", "at",
    "least", "most", "is", "equals", "equal", "exactly", "not",
];
pub(crate) const FILTER_CONNECTORS: [&str; 6] = ["with", "whose", "where", "having", "has", "have"];
pub(crate) const OTHER_CONNECTORS: [&str; 6] = ["and", "by", "top", "first", "bottom", "last"];
pub(crate) const TRAVERSAL_WORDS: [&str; 2] = ["does", "do"];
pub(crate) const WITHIN_WORDS: [&str; 13] = [
    "within",
    "mile",
    "miles",
    "mi",
    "kilometer",
    "kilometers",
    "km",
    "meter",
    "meters",
    "m",
    "foot",
    "feet",
    "ft",
];
pub(crate) const TIME_WORDS: [&str; 10] = [
    "since",
    "week",
    "in",
    "day",
    "days",
    "ago",
    "today",
    "yesterday",
    "last",
    "with",
];
pub(crate) const STATEMENT_WORDS: [&str; 5] = ["delete", "remove", "set", "change", "to"];
// The similar-to junction words (`docs/NL.md` § 18). `like` is deliberately
// absent: it is not a synonym, so it stays unrecognized vocabulary-wise and
// grounding refuses it by name.
pub(crate) const SEARCH_WORDS: [&str; 1] = ["search"];
pub(crate) const SIMILAR_TO_WORDS: [&str; 2] = ["similar", "to"];
// NL_VERSION 9 edge-case vocabulary. These words are deliberately split
// from the older comparator/connectors because their multi-token frames have
// closed meanings: `with no` is edge absence, `without` is property NULL,
// `not in` is property inequality, and calendar months occur only in the
// `from <month> to <month> <year>` range frame.
pub(crate) const NEGATION_WORDS: [&str; 3] = ["no", "without", "in"];
pub(crate) const RANGE_WORDS: [&str; 15] = [
    "between",
    "from",
    "to",
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
];
// Aggregate heads and their syntax-only `for` (`docs/NL.md` § 17). The
// canonical function and output name derive from the head in template.rs.
pub(crate) const AGGREGATE_WORDS: [&str; 13] = [
    "total", "sum", "average", "mean", "highest", "maximum", "largest", "max", "lowest", "minimum",
    "smallest", "min", "for",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Comparator {
    Gt,
    Lt,
    Ge,
    Le,
    Eq,
    Ne,
}

impl Comparator {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Self::Gt => ">",
            Self::Lt => "<",
            Self::Ge => ">=",
            Self::Le => "<=",
            Self::Eq => "=",
            Self::Ne => "!=",
        }
    }
}

pub(crate) fn is_list(token: &Token) -> bool {
    LIST_VERBS.contains(&token.folded.as_str())
}

pub(crate) fn is_question_head(token: &Token) -> bool {
    ["who", "what", "which"].contains(&token.folded.as_str())
}

pub(crate) fn is_action_list(token: &Token) -> bool {
    ["show", "list", "find", "get", "display", "give"].contains(&token.folded.as_str())
}

pub(crate) fn is_filter_connector(token: &Token) -> bool {
    FILTER_CONNECTORS.contains(&token.folded.as_str())
}

pub(crate) fn is_vocabulary(token: &Token) -> bool {
    if token.kind != TokenKind::Word {
        return false;
    }
    [
        LIST_VERBS.as_slice(),
        COUNT_WORDS.as_slice(),
        COMPARATOR_WORDS.as_slice(),
        FILTER_CONNECTORS.as_slice(),
        OTHER_CONNECTORS.as_slice(),
        TRAVERSAL_WORDS.as_slice(),
        WITHIN_WORDS.as_slice(),
        TIME_WORDS.as_slice(),
        STATEMENT_WORDS.as_slice(),
        AGGREGATE_WORDS.as_slice(),
        SIMILAR_TO_WORDS.as_slice(),
        SEARCH_WORDS.as_slice(),
        NEGATION_WORDS.as_slice(),
        RANGE_WORDS.as_slice(),
    ]
    .into_iter()
    .any(|words| words.contains(&token.folded.as_str()))
}

pub(crate) fn comparator(tokens: &[Token], index: usize) -> Option<(Comparator, usize)> {
    let first = tokens
        .get(index)
        .filter(|token| token.kind == TokenKind::Word)?
        .folded
        .as_str();
    let second = tokens
        .get(index + 1)
        .filter(|token| token.kind == TokenKind::Word)
        .map(|token| token.folded.as_str());
    match (first, second) {
        ("at", Some("least")) => Some((Comparator::Ge, 2)),
        ("at", Some("most")) => Some((Comparator::Le, 2)),
        ("is", Some("not")) => Some((Comparator::Ne, 2)),
        ("over" | "above" | "more" | "greater" | "after", _) => Some((Comparator::Gt, 1)),
        ("under" | "below" | "less" | "fewer" | "before", _) => Some((Comparator::Lt, 1)),
        ("is" | "equals" | "equal" | "exactly", _) => Some((Comparator::Eq, 1)),
        ("not", _) => Some((Comparator::Ne, 1)),
        _ => None,
    }
}

pub(crate) fn all_words() -> impl Iterator<Item = &'static str> {
    let words = LIST_VERBS
        .into_iter()
        .chain(COUNT_WORDS)
        .chain(COMPARATOR_WORDS)
        .chain(FILTER_CONNECTORS)
        .chain(OTHER_CONNECTORS)
        .chain(TRAVERSAL_WORDS)
        .chain(WITHIN_WORDS)
        .chain(TIME_WORDS)
        .chain(STATEMENT_WORDS)
        .chain(AGGREGATE_WORDS)
        .chain(SIMILAR_TO_WORDS)
        .chain(SEARCH_WORDS)
        .chain(NEGATION_WORDS)
        .chain(RANGE_WORDS);
    let mut seen = BTreeSet::new();
    words.filter(move |word| seen.insert(*word))
}
