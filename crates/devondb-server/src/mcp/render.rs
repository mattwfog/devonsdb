//! Compact renderers: the tree, the result table, the refusal
//! (`docs/MCP.md` §1 laws 2-5 and 7, §4). Every byte here is paid for in
//! the model's context on every later turn.

use std::fmt::Write as _;

use devondb::introspect::{NodeClassSummary, PinSummary, RelClassSummary};
use devondb::{ColumnSummary, QueryResult, SchemaSummary};
use devondb_nl::NoParse;
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

use super::tools::Counts;

/// Row window when the caller passes no `limit`.
pub const DEFAULT_LIMIT: usize = 50;
/// Largest row window a caller may request.
pub const MAX_LIMIT: usize = 500;
/// Result-body byte cap; the footer explains the cut (law 2).
pub const BODY_BYTES: usize = 8 * 1024;
/// Longest rendered cell before clipping (law 5).
pub const CELL_CHARS: usize = 160;

/// A row window over a materialized result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub limit: usize,
    pub offset: usize,
}

/// Typed header, tab-separated bare cells, a footer that states scale.
pub fn result_table(result: &QueryResult, window: Window) -> String {
    let total = result.rows.len();
    let mut body = header_line(result);
    body.push('\n');
    let mut shown = 0;
    let mut capped = false;
    for row in result.rows.iter().skip(window.offset).take(window.limit) {
        let line = row_line(row);
        if body.len() + line.len() + 1 > BODY_BYTES {
            capped = true;
            break;
        }
        body.push_str(&line);
        body.push('\n');
        shown += 1;
    }
    body.push_str(&footer(total, window.offset, shown, capped));
    body
}

fn header_line(result: &QueryResult) -> String {
    result
        .columns
        .iter()
        .enumerate()
        .map(|(index, name)| match column_type(result, index) {
            Some(ty) => format!("{name}:{ty}"),
            None => name.clone(),
        })
        .collect::<Vec<_>>()
        .join("\t")
}

/// The plan-IR type spelling of the first non-null value in the column.
/// A Decimal value carries its own precision, not the column's declared
/// one, so the header says `Decimal` and lets the cell show the scale.
fn column_type(result: &QueryResult, index: usize) -> Option<String> {
    let ty = result
        .rows
        .iter()
        .find_map(|row| row.get(index).and_then(Value::logical_type))?;
    Some(match ty {
        LogicalType::Decimal { .. } => "Decimal".to_owned(),
        other => other.to_string(),
    })
}

fn row_line(row: &[Value]) -> String {
    row.iter().map(cell).collect::<Vec<_>>().join("\t")
}

fn footer(total: usize, offset: usize, shown: usize, capped: bool) -> String {
    if total == 0 {
        return "(0 rows)".to_owned();
    }
    if offset == 0 && shown == total {
        return format!("({total} rows)");
    }
    if shown == 0 {
        return format!("rows none of {total} (offset {offset} is past the end)");
    }
    let range = format!("rows {}-{} of {total}", offset + 1, offset + shown);
    let next = offset + shown;
    if capped {
        format!(
            "{range} (body cap hit; narrow with a filter or aggregate, or page with offset={next})"
        )
    } else if next < total {
        format!("{range} (more: offset={next}, or narrow with a filter or aggregate)")
    } else {
        range
    }
}

/// One bare cell: types live in the header (law 3), vectors are never
/// printed (law 4), long values are clipped (law 5).
pub fn cell(value: &Value) -> String {
    let text = match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Int64(number) => number.to_string(),
        Value::Float64(number) => number.to_string(),
        Value::String(text) | Value::Json(text) => escape(text),
        Value::Vector(vector) => format!("<vector {}>", vector.len()),
        Value::Decimal(decimal) => decimal.to_string(),
        Value::GeoPoint(_) | Value::Timestamp(_) => bare(value),
        Value::Bytes(bytes) => format!("0x{}", hex(bytes)),
    };
    clip(&text)
}

/// Strips the literal wrapper from a `Display` spelling such as
/// `timestamp("2026-08-30T00:00:00Z")`; spellings without one pass through.
fn bare(value: &Value) -> String {
    let text = value.to_string();
    let Some(open) = text.find("(\"") else {
        return text;
    };
    if !text.ends_with("\")") {
        return text;
    }
    text[open + 2..text.len() - 2].to_owned()
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn clip(text: &str) -> String {
    let count = text.chars().count();
    if count <= CELL_CHARS {
        return text.to_owned();
    }
    let kept: String = text.chars().take(CELL_CHARS).collect();
    format!("{kept}…(+{})", count - CELL_CHARS)
}

/// The tree (`docs/MCP.md` §4); empty sections are omitted.
pub fn schema_tree(summary: &SchemaSummary, counts: Option<&Counts>) -> String {
    let mut out = String::new();
    if !summary.node_tables.is_empty() {
        out.push_str("nodes\n");
        for table in &summary.node_tables {
            let _ = writeln!(
                out,
                "  {} ({}){}",
                table.name,
                columns(&table.columns),
                count_suffix(counts, &table.name)
            );
        }
    }
    if !summary.rel_tables.is_empty() {
        out.push_str("rels\n");
        for rel in &summary.rel_tables {
            let props = if rel.columns.is_empty() {
                String::new()
            } else {
                format!(" ({})", columns(&rel.columns))
            };
            let _ = writeln!(
                out,
                "  {}: {} -> {}{props}{}",
                rel.name,
                rel.from,
                rel.to,
                count_suffix(counts, &rel.name)
            );
        }
    }
    if let Some(classes) = &summary.classes {
        interfaces_section(&mut out, summary, classes);
        classes_section(&mut out, classes);
    }
    pins_section(&mut out, &summary.pins);
    if out.is_empty() {
        out.push_str("(empty database: no tables)\n");
    }
    out
}

fn columns(columns: &[ColumnSummary]) -> String {
    columns
        .iter()
        .map(|column| {
            if column.primary_key {
                format!("{}:{} pk", column.name, column.ty)
            } else {
                format!("{}:{}", column.name, column.ty)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn count_suffix(counts: Option<&Counts>, table: &str) -> String {
    match counts.and_then(|counts| counts.get(table)) {
        Some(Ok(count)) => format!("  {count} rows"),
        Some(Err(message)) => format!("  rows: {message}"),
        None => String::new(),
    }
}

fn interfaces_section(
    out: &mut String,
    summary: &SchemaSummary,
    classes: &devondb::introspect::ClassesSummary,
) {
    if classes.interfaces.is_empty() {
        return;
    }
    out.push_str("interfaces\n");
    for interface in &classes.interfaces {
        let required = interface
            .columns
            .iter()
            .map(|column| format!("{}:{}", column.name, column.ty))
            .collect::<Vec<_>>()
            .join(", ");
        let implementors = classes
            .node_classes
            .iter()
            .filter(|class| class.implements.contains(&interface.name))
            .map(|class| class.table.as_str())
            .collect::<Vec<_>>();
        let _ = write!(out, "  {} ({required})", interface.name);
        if !implementors.is_empty() {
            let _ = write!(out, " <- {}", implementors.join(", "));
        }
        out.push('\n');
    }
    let _ = summary;
}

fn classes_section(out: &mut String, classes: &devondb::introspect::ClassesSummary) {
    let node_lines: Vec<String> = classes
        .node_classes
        .iter()
        .filter_map(node_class_line)
        .collect();
    let rel_lines: Vec<String> = classes
        .rel_classes
        .iter()
        .filter_map(rel_class_line)
        .collect();
    if node_lines.is_empty() && rel_lines.is_empty() {
        return;
    }
    out.push_str("classes\n");
    for line in node_lines.iter().chain(&rel_lines) {
        let _ = writeln!(out, "  {line}");
    }
}

/// Only what was declared or derived beyond the table name earns a line.
fn node_class_line(class: &NodeClassSummary) -> Option<String> {
    let mut line = format!("{} \"{}\"", class.table, class.display);
    let mut informative = false;
    if let Some(plural) = &class.plural {
        let _ = write!(line, "/\"{plural}\"");
        informative = true;
    }
    if let Some(label) = &class.label {
        let _ = write!(line, " label={label}");
        informative = true;
    }
    if !class.summary.is_empty() {
        let _ = write!(line, " summary=({})", class.summary.join(", "));
        informative = true;
    }
    if !class.implements.is_empty() {
        let _ = write!(line, " implements {}", class.implements.join(", "));
        informative = true;
    }
    if let Some(description) = &class.description {
        let _ = write!(line, " — {description}");
        informative = true;
    }
    informative.then_some(line)
}

fn rel_class_line(class: &RelClassSummary) -> Option<String> {
    let mut line = class.table.clone();
    let mut informative = false;
    if let Some(verb) = &class.verb {
        let _ = write!(line, " verb \"{verb}\"");
        informative = true;
    }
    if let Some(inverse) = &class.inverse {
        let _ = write!(line, " inverse \"{inverse}\"");
        informative = true;
    }
    informative.then_some(line)
}

fn pins_section(out: &mut String, pins: &[PinSummary]) {
    if pins.is_empty() {
        return;
    }
    out.push_str("pins\n");
    for pin in pins {
        let _ = writeln!(out, "  {}: {}", pin.name, pin.canonical);
    }
}

/// The compact refusal (law 7): what did not ground, what did, what to try.
pub fn refusal(report: &NoParse) -> String {
    let mut out = String::from("no parse\n");
    if !report.unrecognized.is_empty() {
        let items = report
            .unrecognized
            .iter()
            .map(|item| match &item.suggestion {
                Some(suggestion) => format!("{} (did you mean: {suggestion})", item.token),
                None => item.token.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "unrecognized: {items}");
    }
    if !report.recognized.is_empty() {
        let items = report
            .recognized
            .iter()
            .map(|item| format!("{} -> {}", item.token, item.target))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "recognized: {items}");
    }
    if !report.nearest.is_empty() {
        out.push_str("try:\n");
        for hint in &report.nearest {
            let _ = writeln!(out, "  {}", hint.example);
        }
    }
    out
}
