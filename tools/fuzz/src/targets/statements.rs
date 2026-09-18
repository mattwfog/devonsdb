use std::path::Path;

use devondb::{Database, Options, text::parser::Parsed, text::parser::parse};

use crate::{mutate::mutate_bytes, rng::Rng};

const RECORD_SEPARATOR: u8 = 0x1e;
const MAX_STATEMENT_INPUT_BYTES: usize = 64 * 1024;

/// Generates a coherent statement stream, then applies grammar and byte mutations.
pub(crate) fn generate(execution: u64, rng: &mut Rng, intensity: usize) -> Vec<u8> {
    let table = ["Person", "Entity", "Node_7"][rng.index(3)];
    let company = ["Company", "Firm", "Org_2"][rng.index(3)];
    let rel = ["WorksAt", "MemberOf", "LinkedTo"][rng.index(3)];
    let base = valid_stream(table, company, rel, rng);
    if execution == 0 {
        return base;
    }

    let mut bytes = base;
    if execution.is_multiple_of(2) {
        grammar_mutation(&mut bytes, rng);
    }
    let rounds = rng.range(1, intensity.max(1));
    mutate_bytes(&mut bytes, rng, rounds, MAX_STATEMENT_INPUT_BYTES);
    bytes
}

fn valid_stream(table: &str, company: &str, rel: &str, rng: &mut Rng) -> Vec<u8> {
    let suffix = rng.next_u64() % 10_000;
    let statements = [
        format!(
            "create node table {table} (id Int64 primary key, label String, score Float64, active Bool, embedding Vector(3))"
        ),
        format!(
            "insert into {table} values (1, \"Ada {suffix}\", 1.5, true, [1, 0, -1]), (2, \"Grace\", -2.25, false, [0.5, 1.5, 2.5]), (3, \"Linus\", 0.0, true, [0, 0, 0])"
        ),
        format!("update {table} set label = \"Ada Lovelace\", score = 2.0 where id = 1"),
        format!("upsert {table} values (2, \"Grace Hopper\", 3.5, true, [2, 1, 0])"),
        format!("create node table {company} (id Int64 primary key, name String)"),
        format!("insert into {company} values (10, \"Devon\"), (11, \"Analytical Engines\")"),
        format!("create rel table {rel} from {table} to {company} (since Int64)"),
        format!("insert rel into {rel} values (1 -> 10, 2024), (2 -> 11, 2025)"),
        format!("delete from {table} where id = 3"),
    ];
    join_records(&statements)
}

fn join_records(records: &[String]) -> Vec<u8> {
    let mut stream = Vec::new();
    for (index, record) in records.iter().enumerate() {
        if index > 0 {
            stream.push(RECORD_SEPARATOR);
        }
        stream.extend_from_slice(record.as_bytes());
    }
    stream
}

fn grammar_mutation(bytes: &mut Vec<u8>, rng: &mut Rng) {
    const REPLACEMENTS: [(&[u8], &[u8]); 9] = [
        (b"insert into", b"upsert"),
        (b"upsert", b"insert into"),
        (b"Int64", b"String"),
        (b"Float64", b"Bool"),
        (b"primary key", b"primary primary key"),
        (b" where ", b" where missing = "),
        (b"values", b"value"),
        (b"true", b"null"),
        (b"Vector(3)", b"Vector(4294967295)"),
    ];
    let (needle, replacement) = REPLACEMENTS[rng.index(REPLACEMENTS.len())];
    replace_one(bytes, needle, replacement, rng);
}

fn replace_one(bytes: &mut Vec<u8>, needle: &[u8], replacement: &[u8], rng: &mut Rng) {
    let positions: Vec<usize> = bytes
        .windows(needle.len())
        .enumerate()
        .filter_map(|(index, window)| (window == needle).then_some(index))
        .collect();
    if positions.is_empty() {
        return;
    }
    let start = positions[rng.index(positions.len())];
    bytes.splice(start..start + needle.len(), replacement.iter().copied());
}

/// Parses and executes the stream, then proves its database can reopen.
pub(crate) fn exercise(input: &[u8], scratch: &Path) -> Result<(), String> {
    if input.len() > MAX_STATEMENT_INPUT_BYTES {
        return Err(format!(
            "statement input is {} bytes, over the {MAX_STATEMENT_INPUT_BYTES}-byte budget",
            input.len()
        ));
    }
    let path = scratch.join("statements.devondb");
    let mut database = Database::create_with(
        &path,
        Options {
            memory_limit: 8 * 1024 * 1024,
            ..Options::default()
        },
    )
    .map_err(|error| format!("scratch database create failed: {error}"))?;
    exercise_records(&mut database, input);
    drop(database);

    Database::open_with(
        &path,
        Options {
            memory_limit: 8 * 1024 * 1024,
            ..Options::default()
        },
    )
    .map(drop)
    .map_err(|error| format!("database did not reopen after statement input: {error}"))
}

fn exercise_records(database: &mut Database, input: &[u8]) {
    for record in input.split(|byte| *byte == RECORD_SEPARATOR) {
        let text = match std::str::from_utf8(record) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("error: statement input is not UTF-8: {error}");
                return;
            }
        };
        if text.trim().is_empty() {
            continue;
        }
        let outcome = match parse(text) {
            Ok(Parsed::Statement(envelope)) => database.execute(&envelope.stmt),
            Ok(Parsed::Query(plan)) => database.run(&plan).map(drop),
            Err(error) => {
                eprintln!("error: {error}");
                return;
            }
        };
        if let Err(error) = outcome {
            eprintln!("error: {error}");
            return;
        }
    }
}
