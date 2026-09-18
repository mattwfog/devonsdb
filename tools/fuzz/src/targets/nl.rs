use devondb::introspect::{ClassesSummary, NodeClassSummary, RelClassSummary};
use devondb::text::{
    parser::{Parsed, parse},
    printer::print_plan,
};
use devondb::{ColumnSummary, NodeTableSummary, RelTableSummary, SchemaSummary, StatementEnvelope};
use devondb_nl::{Compiled, CompiledStatement, DeterministicCompiler, IntentCompiler, NoParse};

use crate::{mutate::mutate_bytes, rng::Rng};

const HEADER_BYTES: usize = 16;
const MAX_NL_INPUT_BYTES: usize = 64 * 1024;
const PLAN_VERSION: u32 = 0;

struct GeneratedSchema {
    summary: SchemaSummary,
    table: String,
    subject: String,
    label: String,
    measure: String,
}

/// Generates a schema-tagged question with grammar-aware and raw-byte mutations.
pub(crate) fn generate(execution: u64, rng: &mut Rng, intensity: usize) -> Vec<u8> {
    let mut header = [0_u8; HEADER_BYTES];
    for byte in &mut header {
        *byte = rng.byte();
    }
    let schema = generate_schema(&header);
    let mut question = seed_question(execution, &schema).into_bytes();
    if execution != 0 && !execution.is_multiple_of(7) {
        let rounds = rng.range(1, intensity.clamp(1, 8));
        for _ in 0..rounds {
            mutate_grammar(&mut question, rng);
        }
    }

    let mut input = header.to_vec();
    input.extend_from_slice(&question);
    if execution != 0 && !execution.is_multiple_of(5) {
        let rounds = rng.range(1, intensity.max(1));
        mutate_bytes(&mut input, rng, rounds, MAX_NL_INPUT_BYTES);
    }
    input.truncate(MAX_NL_INPUT_BYTES);
    input
}

/// Compiles one question through both deterministic NL entry points.
pub(crate) fn exercise(input: &[u8]) -> Result<(), String> {
    if input.len() > MAX_NL_INPUT_BYTES {
        return Err(format!(
            "NL input is {} bytes, over the {MAX_NL_INPUT_BYTES}-byte cap",
            input.len()
        ));
    }
    let header_end = input.len().min(HEADER_BYTES);
    let schema = generate_schema(&input[..header_end]);
    let question_bytes = input.get(HEADER_BYTES..).unwrap_or_default();
    let question = String::from_utf8_lossy(question_bytes);
    let compiler = DeterministicCompiler;
    exercise_query(&compiler, &question, &schema.summary)?;
    exercise_statement(&compiler, &question, &schema.summary)
}

fn exercise_query(
    compiler: &DeterministicCompiler,
    question: &str,
    schema: &SchemaSummary,
) -> Result<(), String> {
    let first = compiler.compile(question, schema);
    let second = compiler.compile(question, schema);
    if first != second {
        return Err("NL query compilation returned different outcomes".to_owned());
    }
    let first_bytes = query_bytes(&first)?;
    let second_bytes = query_bytes(&second)?;
    if first_bytes != second_bytes {
        return Err("NL query compilation was not byte-identical".to_owned());
    }
    match first {
        Compiled::Plan(plan) => round_trip_plan(&plan),
        Compiled::NoParse(report) => check_refusal("query", &report),
    }
}

fn exercise_statement(
    compiler: &DeterministicCompiler,
    question: &str,
    schema: &SchemaSummary,
) -> Result<(), String> {
    let first = compiler.compile_statement(question, schema);
    let second = compiler.compile_statement(question, schema);
    if first != second {
        return Err("NL statement compilation returned different outcomes".to_owned());
    }
    let first_bytes = statement_bytes(&first)?;
    let second_bytes = statement_bytes(&second)?;
    if first_bytes != second_bytes {
        return Err("NL statement compilation was not byte-identical".to_owned());
    }
    match first {
        CompiledStatement::Statement(_) => Ok(()),
        CompiledStatement::NoParse(report) => check_refusal("statement", &report),
    }
}

fn query_bytes(outcome: &Compiled) -> Result<Vec<u8>, String> {
    match outcome {
        Compiled::Plan(plan) => plan
            .to_json()
            .map(String::into_bytes)
            .map_err(|error| format!("NL plan would not serialize: {error}")),
        Compiled::NoParse(report) => Ok(refusal_bytes(report)),
    }
}

fn statement_bytes(outcome: &CompiledStatement) -> Result<Vec<u8>, String> {
    match outcome {
        CompiledStatement::Statement(statement) => StatementEnvelope {
            v: PLAN_VERSION,
            stmt: statement.clone(),
        }
        .to_json()
        .map(String::into_bytes)
        .map_err(|error| format!("NL statement would not serialize: {error}")),
        CompiledStatement::NoParse(report) => Ok(refusal_bytes(report)),
    }
}

fn refusal_bytes(report: &NoParse) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_len(&mut bytes, report.recognized.len());
    for item in &report.recognized {
        push_text(&mut bytes, &item.token);
        push_text(&mut bytes, &item.target);
    }
    push_len(&mut bytes, report.unrecognized.len());
    for item in &report.unrecognized {
        push_text(&mut bytes, &item.token);
        push_optional_text(&mut bytes, item.suggestion.as_deref());
    }
    push_len(&mut bytes, report.nearest.len());
    for item in &report.nearest {
        push_text(&mut bytes, &item.example);
    }
    bytes
}

fn push_len(bytes: &mut Vec<u8>, length: usize) {
    bytes.extend_from_slice(&length.to_le_bytes());
}

fn push_text(bytes: &mut Vec<u8>, text: &str) {
    push_len(bytes, text.len());
    bytes.extend_from_slice(text.as_bytes());
}

fn push_optional_text(bytes: &mut Vec<u8>, text: Option<&str>) {
    match text {
        Some(text) => {
            bytes.push(1);
            push_text(bytes, text);
        }
        None => bytes.push(0),
    }
}

fn round_trip_plan(plan: &devondb::Plan) -> Result<(), String> {
    let text = print_plan(plan).map_err(|error| format!("NL plan would not print: {error}"))?;
    match parse(&text).map_err(|error| format!("printed NL plan would not parse: {error}"))? {
        Parsed::Query(reparsed) if reparsed == *plan => Ok(()),
        Parsed::Query(_) => Err("printed NL plan reparsed to a different plan".to_owned()),
        Parsed::Statement(_) => Err("printed NL plan reparsed as a statement".to_owned()),
    }
}

fn check_refusal(surface: &str, report: &NoParse) -> Result<(), String> {
    if report.nearest.len() <= 3 {
        Ok(())
    } else {
        Err(format!(
            "NL {surface} refusal returned {} nearest hints",
            report.nearest.len()
        ))
    }
}

fn generate_schema(header: &[u8]) -> GeneratedSchema {
    const TABLES: [(&str, &str); 4] = [
        ("Person", "people"),
        ("Contact", "contacts"),
        ("Profile", "profiles"),
        ("Count", "counts"),
    ];
    const LABELS: [&str; 4] = ["name", "title", "label", "Count"];
    const MEASURES: [&str; 4] = ["score", "amount", "rating", "Total"];
    let (table, default_plural) = TABLES[usize::from(header_byte(header, 0)) % TABLES.len()];
    let label = LABELS[usize::from(header_byte(header, 1)) % LABELS.len()];
    let measure = MEASURES[usize::from(header_byte(header, 2)) % MEASURES.len()];
    let vector_dim = u32::from(header_byte(header, 3) % 8) + 1;
    let has_classes = header_byte(header, 4).is_multiple_of(2);
    let class_plural = if header_byte(header, 5).is_multiple_of(2) {
        "humans"
    } else {
        "records"
    };
    let subject = if has_classes {
        class_plural
    } else {
        default_plural
    };
    GeneratedSchema {
        summary: schema_summary(table, label, measure, vector_dim, has_classes, class_plural),
        table: table.to_owned(),
        subject: subject.to_owned(),
        label: label.to_owned(),
        measure: measure.to_owned(),
    }
}

fn schema_summary(
    table: &str,
    label: &str,
    measure: &str,
    vector_dim: u32,
    has_classes: bool,
    class_plural: &str,
) -> SchemaSummary {
    SchemaSummary {
        node_tables: vec![
            main_table(table, label, measure, vector_dim),
            company_table(),
        ],
        rel_tables: vec![knows_table(table), works_at_table(table)],
        classes: has_classes.then(|| classes(table, label, class_plural)),
        pins: Vec::new(),
    }
}

fn main_table(table: &str, label: &str, measure: &str, vector_dim: u32) -> NodeTableSummary {
    NodeTableSummary {
        name: table.to_owned(),
        columns: vec![
            column("id", "Int64", true),
            column(label, "String", false),
            column("age", "Int64", false),
            column(measure, "Float64", false),
            column("occurred", "Int64", false),
            column("created_at", "Timestamp", false),
            column("embedding", &format!("Vector({vector_dim})"), false),
            column("location", "GeoPoint", false),
        ],
    }
}

fn company_table() -> NodeTableSummary {
    NodeTableSummary {
        name: "Company".to_owned(),
        columns: vec![
            column("id", "Int64", true),
            column("name", "String", false),
            column("valuation", "Float64", false),
            column("founded_at", "Timestamp", false),
            column("headquarters", "GeoPoint", false),
        ],
    }
}

fn knows_table(table: &str) -> RelTableSummary {
    RelTableSummary {
        name: "Knows".to_owned(),
        from: table.to_owned(),
        to: table.to_owned(),
        columns: vec![column("since", "Timestamp", false)],
    }
}

fn works_at_table(table: &str) -> RelTableSummary {
    RelTableSummary {
        name: "Works_At".to_owned(),
        from: table.to_owned(),
        to: "Company".to_owned(),
        columns: vec![column("since", "Int64", false)],
    }
}

fn classes(table: &str, label: &str, plural: &str) -> ClassesSummary {
    ClassesSummary {
        interfaces: Vec::new(),
        node_classes: vec![NodeClassSummary {
            table: table.to_owned(),
            display: "Human".to_owned(),
            plural: Some(plural.to_owned()),
            label: Some(label.to_owned()),
            summary: vec![label.to_owned(), "age".to_owned()],
            color: Some("#7aa2ff".to_owned()),
            description: Some("a generated fuzz class".to_owned()),
            implements: Vec::new(),
        }],
        rel_classes: vec![RelClassSummary {
            table: "Knows".to_owned(),
            verb: Some("knows".to_owned()),
            inverse: Some("is known by".to_owned()),
        }],
    }
}

fn column(name: &str, ty: &str, primary_key: bool) -> ColumnSummary {
    ColumnSummary {
        name: name.to_owned(),
        ty: ty.to_owned(),
        primary_key,
    }
}

fn header_byte(header: &[u8], index: usize) -> u8 {
    header.get(index).copied().unwrap_or_default()
}

fn seed_question(execution: u64, schema: &GeneratedSchema) -> String {
    let table = &schema.table;
    let subject = &schema.subject;
    let label = &schema.label;
    let measure = &schema.measure;
    match execution % 43 {
        0 => format!("show all {subject}"),
        1 => format!("{subject} with age over 30"),
        2 => "who is Ada".to_owned(),
        3 => "show Ada".to_owned(),
        4 => "who does Ada know".to_owned(),
        5 => format!("how many {subject} have age over 30"),
        6 => format!("top 5 {subject} by age"),
        7 => format!("display {subject} with age at least 21"),
        8 => format!("{subject} where age over 30 and {measure} below 100.5"),
        9 => format!("number of {subject}"),
        10 => format!("count {subject} whose {label} equals Ada"),
        11 => "who knows Ada".to_owned(),
        12 => format!("bottom 2 {subject} with age at least 18 by age"),
        13 => format!("{subject} within 2 miles of geo(45.5152, -122.6784)"),
        14 => format!("set {table} 1 {measure} to 9.5"),
        15 => format!("change {table} 1 {label} to \"Ada Lovelace\""),
        16 => format!("delete {table} 1"),
        17 => format!("remove {table} 1"),
        18 => "knows of knows of Ada".to_owned(),
        19 => format!("who do the {subject} Ada knows know"),
        20 => format!("{subject} with occurred since last week"),
        21 => format!("{subject} with occurred in the last 7 days"),
        22 => format!("{subject} with occurred yesterday"),
        23 => format!("{subject} with occurred today"),
        24 => format!("{subject} with occurred since 2 days ago"),
        25 => format!("total {measure} of {subject}"),
        26 => format!("sum of {measure} for {subject} by {label}"),
        27 => format!("{subject} average {measure}"),
        28 => format!("highest age of {subject}"),
        29 => format!("lowest age for {subject} by {label}"),
        30 => format!("how many {subject} by {label}"),
        31 => format!("{subject} similar to Ada"),
        32 => format!("top 3 {subject} similar to Ada"),
        33 => format!("{subject} with no knows"),
        34 => format!("{subject} without {measure}"),
        35 => format!("{subject} not in {label} Ada"),
        36 => format!("{subject} with age between 18 and 65"),
        37 => format!("{subject} from january to march 2026"),
        38 => format!("{subject} older than 30"),
        39 => format!("top five {subject} by age"),
        40 => format!("{subject} like Ada"),
        41 => format!("delete all {subject} over 40"),
        _ => format!("search {table} {label} for \"Ada graph\""),
    }
}

fn mutate_grammar(question: &mut Vec<u8>, rng: &mut Rng) {
    let text = String::from_utf8_lossy(question);
    let mut tokens: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    match rng.index(7) {
        0 => drop_token(&mut tokens, rng),
        1 => duplicate_token(&mut tokens, rng),
        2 => swap_tokens(&mut tokens, rng),
        3 => inject_punctuation(&mut tokens, rng),
        4 => inject_quotes(&mut tokens, rng),
        5 => inject_controls(&mut tokens, rng),
        _ => splice_literal(&mut tokens, rng),
    }
    *question = tokens.join(" ").into_bytes();
}

fn drop_token(tokens: &mut Vec<String>, rng: &mut Rng) {
    if !tokens.is_empty() {
        tokens.remove(rng.index(tokens.len()));
    }
}

fn duplicate_token(tokens: &mut Vec<String>, rng: &mut Rng) {
    if let Some(token) = tokens.get(rng.index(tokens.len())).cloned() {
        let destination = rng.index(tokens.len().saturating_add(1));
        tokens.insert(destination, token);
    }
}

fn swap_tokens(tokens: &mut [String], rng: &mut Rng) {
    if tokens.len() >= 2 {
        let left = rng.index(tokens.len());
        let right = rng.index(tokens.len());
        tokens.swap(left, right);
    }
}

fn inject_punctuation(tokens: &mut Vec<String>, rng: &mut Rng) {
    const PUNCTUATION: [&str; 9] = ["?", ",", ".", "!", ";", ":", "(", ")", "..."];
    let destination = rng.index(tokens.len().saturating_add(1));
    tokens.insert(
        destination,
        PUNCTUATION[rng.index(PUNCTUATION.len())].to_owned(),
    );
}

fn inject_quotes(tokens: &mut Vec<String>, rng: &mut Rng) {
    const QUOTES: [&str; 5] = ["\"Ada, Inc.\"", "\"unterminated", "''", "`name`", "\\\""];
    if !tokens.is_empty() && rng.index(2) == 0 {
        let index = rng.index(tokens.len());
        tokens[index] = format!("\"{}\"", tokens[index]);
    } else {
        let destination = rng.index(tokens.len().saturating_add(1));
        tokens.insert(destination, QUOTES[rng.index(QUOTES.len())].to_owned());
    }
}

fn inject_controls(tokens: &mut Vec<String>, rng: &mut Rng) {
    const CONTROLS: [&str; 7] = ["\0", "\u{1}", "\u{1f}", "\t", "\r", "Ada’s", "O’Neil"];
    let destination = rng.index(tokens.len().saturating_add(1));
    tokens.insert(destination, CONTROLS[rng.index(CONTROLS.len())].to_owned());
}

fn splice_literal(tokens: &mut Vec<String>, rng: &mut Rng) {
    const LITERALS: [&str; 12] = [
        "-0",
        "-3.5e+2",
        ".5",
        "9223372036854775808",
        "2024-02-29",
        "2026-02-29",
        "1970-01-01",
        "geo(45.5152,-122.6784)",
        "geo(90,180)",
        "timestamp(\"2026-01-02T00:00:00Z\")",
        "[1,0,-1]",
        "\"Ada\\nLovelace\"",
    ];
    let literal = LITERALS[rng.index(LITERALS.len())].to_owned();
    if tokens.is_empty() || rng.index(2) == 0 {
        let destination = rng.index(tokens.len().saturating_add(1));
        tokens.insert(destination, literal);
    } else {
        let index = rng.index(tokens.len());
        tokens[index] = literal;
    }
}
