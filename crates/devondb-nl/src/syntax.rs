//! Canonical DevonPlan text fragments shared by grounding and emission.

use std::fmt::Write as _;

use devondb::fold;

pub(crate) fn identifier(name: &str) -> String {
    if bare_identifier(name) && !reserved(fold(name).as_ref()) {
        return name.to_owned();
    }
    let mut output = String::from("`");
    for ch in name.chars() {
        match ch {
            '`' => output.push_str("\\`"),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(output, "\\u{{{:x}}}", u32::from(ch));
            }
            _ => output.push(ch),
        }
    }
    output.push('`');
    output
}

pub(crate) fn column_ref(binding: &str, column: &str) -> String {
    format!("{}.{}", identifier(binding), identifier(column))
}

pub(crate) fn quote_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch <= '\u{1f}' => {
                let _ = write!(output, "\\u{{{:x}}}", u32::from(ch));
            }
            _ => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn bare_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn reserved(name: &str) -> bool {
    const WORDS: [&str; 56] = [
        "and",
        "or",
        "not",
        "true",
        "false",
        "null",
        "as",
        "by",
        "asc",
        "desc",
        "out",
        "in",
        "both",
        "from",
        "to",
        "nodes",
        "expand",
        "filter",
        "project",
        "sort",
        "limit",
        "offset",
        "aggregate",
        "knn",
        "distance",
        "if",
        "coalesce",
        "least",
        "greatest",
        "date_trunc",
        "cosine",
        "l2",
        "count",
        "sum",
        "min",
        "max",
        "avg",
        "create",
        "insert",
        "upsert",
        "copy",
        "update",
        "set",
        "delete",
        "where",
        "into",
        "values",
        "node",
        "rel",
        "table",
        "primary",
        "key",
        "let",
        "join",
        "on",
        "approximate",
    ];
    WORDS.contains(&name)
}
