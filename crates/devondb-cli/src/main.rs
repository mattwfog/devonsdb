//! devondb command-line interface.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, BufRead, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{
    Database, Options, Plan, QueryResult, SchemaSummary, Statement, StatementEnvelope, Value,
    text::{
        parser::{Parsed, parse},
        printer::{print_plan, print_statement},
    },
};
use devondb_nl::{
    Compiled, CompiledStatement, DeterministicCompiler, IntentCompiler, NoParse, TemplateHint,
};

mod activate;
mod pack;
#[path = "../../devondb-storage/src/stats.rs"]
mod storage_stats;

const PAGE_SIZE: u32 = 4096;
const DAY_SECONDS: i64 = 86_400;
const DEFAULT_UI_PORT: u16 = 8484;

enum Command {
    Version,
    Ask {
        path: OsString,
        options: Options,
        question: String,
        yes: bool,
        read_only: bool,
    },
    Repl {
        path: OsString,
        options: Options,
        read_only: bool,
    },
    /// `devondb activate <path>`: enable offline multiprocess coordination.
    Activate {
        path: OsString,
    },
    Ui {
        path: OsString,
        options: Options,
        port: u16,
        open: bool,
    },
    /// `devondb mcp <path>`: the read-only MCP tool server (docs/MCP.md).
    Mcp {
        path: OsString,
        options: Options,
    },
    /// `devondb pack <in> <out>`: write the DEVONPACK container
    /// (docs/SCALE.md §7).
    Pack {
        input: OsString,
        output: OsString,
        frame_pages: u32,
        options: Options,
    },
    /// `devondb dump <db|pack>`: emit canonical restore statements.
    Dump {
        path: OsString,
        options: Options,
    },
    /// `devondb compact <db>`: offline live-page rewrite and atomic swap.
    Compact {
        path: OsString,
        options: Options,
    },
    /// `devondb stats <db|pack>`: read-only physical storage accounting.
    Stats {
        path: OsString,
        options: Options,
    },
}

fn main() -> ExitCode {
    let Some(command) = parse_arguments(env::args_os().skip(1)) else {
        return usage();
    };

    match command {
        Command::Version => {
            println!("devondb {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Ask {
            path,
            options,
            question,
            yes,
            read_only,
        } => run_ask(Path::new(&path), options, &question, yes, read_only),
        Command::Repl {
            path,
            options,
            read_only,
        } => run_repl(Path::new(&path), options, read_only),
        Command::Activate { path } => activate::run(Path::new(&path)),
        Command::Ui {
            path,
            options,
            port,
            open,
        } => run_ui(Path::new(&path), options, port, open),
        Command::Mcp { path, options } => run_mcp(Path::new(&path), options),
        Command::Pack {
            input,
            output,
            frame_pages,
            options,
        } => pack::run(Path::new(&input), Path::new(&output), frame_pages, options),
        Command::Dump { path, options } => run_dump(Path::new(&path), options),
        Command::Compact { path, options } => run_compact(Path::new(&path), options),
        Command::Stats { path, options } => run_stats(Path::new(&path), options.memory_limit),
    }
}

fn run_repl(path: &Path, options: Options, read_only: bool) -> ExitCode {
    let memory_limit = options.memory_limit;
    let mut database = match open_database(path, options, read_only) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = repl(&mut database, path, memory_limit) {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[derive(Clone, Copy)]
enum AskStatus {
    Ran,
    Declined,
    NoParse,
    Error,
}

fn run_ask(path: &Path, options: Options, question: &str, yes: bool, read_only: bool) -> ExitCode {
    // Orchestration boundary — the compiler stays deterministic; NL.md §16.
    let reference_date = utc_reference_date();
    let mut database = match open_existing_database(path, options, read_only) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    match ask_question(
        &mut database,
        question,
        reference_date,
        yes,
        &mut stdin,
        &mut stdout,
        &mut stderr,
    ) {
        Ok(AskStatus::Ran | AskStatus::Declined) => ExitCode::SUCCESS,
        Ok(AskStatus::NoParse) => ExitCode::from(NO_PARSE_EXIT_CODE),
        Ok(AskStatus::Error) => ExitCode::FAILURE,
        Err(error) => {
            let _ = writeln!(stderr, "error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: devondb <path> [--read-only] [--memory-limit <bytes>] | devondb ask <path> \"question\" [--yes] [--read-only] [--memory-limit <bytes>] | devondb activate <path> | devondb ui <path> [--port <port>] [--memory-limit <bytes>] [--open] | devondb mcp <path> [--memory-limit <bytes>] | devondb pack <in> <out> [--frame-pages <pages>] [--memory-limit <bytes>] | devondb dump <db|pack> [--memory-limit <bytes>] | devondb compact <db> [--memory-limit <bytes>] | devondb stats <db|pack> [--memory-limit <bytes>]\n       DEVONDB_UI_OPENER overrides the command used by devondb ui --open\n       devondb activate enables multiprocess coordination on an offline database\n       --read-only opens an activated database as a follower shell\n       devondb mcp speaks MCP over stdio, read-only (docs/MCP.md)\n       devondb pack writes a read-only DEVONPACK container (docs/SCALE.md §7)\n       devondb dump emits canonical statements without modifying its input\n       devondb compact rewrites only live content and atomically swaps the file\n       devondb stats reports exact read-only physical storage accounting\n       exits: 2 usage, 3 structured ask refusal"
    );
    ExitCode::from(2)
}

const NO_PARSE_EXIT_CODE: u8 = 3;

/// Ends flag parsing; the next argument is always positional.
struct PositionalArguments<I: Iterator<Item = OsString>> {
    arguments: std::iter::Peekable<I>,
    after_separator: bool,
}

impl<I: Iterator<Item = OsString>> Iterator for PositionalArguments<I> {
    type Item = OsString;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.after_separator
            && self
                .arguments
                .next_if(|argument| argument.to_str() == Some("--"))
                .is_some()
        {
            self.after_separator = true;
            return self.arguments.next();
        }
        self.arguments.next()
    }
}

impl<I: Iterator<Item = OsString>> PositionalArguments<I> {
    fn peek_str(&mut self) -> Option<&str> {
        self.arguments.peek().and_then(|argument| argument.to_str())
    }
}

fn parse_arguments(arguments: impl Iterator<Item = OsString>) -> Option<Command> {
    let mut arguments = PositionalArguments {
        arguments: arguments.peekable(),
        after_separator: false,
    };
    if arguments.peek_str() == Some("--version") {
        arguments.next();
        return arguments.next().is_none().then_some(Command::Version);
    }
    let mut path = None;
    let mut options = Options {
        page_size: PAGE_SIZE,
        ..Options::default()
    };
    let mut memory_limit_seen = false;
    let ui = arguments.peek_str() == Some("ui");
    let ask = arguments.peek_str() == Some("ask");
    let mcp = arguments.peek_str() == Some("mcp");
    let pack = arguments.peek_str() == Some("pack");
    let dump = arguments.peek_str() == Some("dump");
    let compact = arguments.peek_str() == Some("compact");
    let stats = arguments.peek_str() == Some("stats");
    let activate = arguments.peek_str() == Some("activate");
    if ui || ask || mcp || pack || dump || compact || stats || activate {
        arguments.next();
    }
    let mut port = DEFAULT_UI_PORT;
    let mut port_seen = false;
    let mut open = false;
    let mut question = None;
    let mut yes = false;
    let mut output = None;
    let mut frame_pages = pack::DEFAULT_FRAME_PAGES;
    let mut frame_pages_seen = false;
    let mut read_only = false;
    while let Some(argument) = arguments.next() {
        if argument.to_str() == Some("--memory-limit") {
            if memory_limit_seen {
                return None;
            }
            let bytes = arguments.next()?.to_str()?.parse().ok()?;
            options.memory_limit = bytes;
            memory_limit_seen = true;
        } else if pack && argument.to_str() == Some("--frame-pages") {
            if frame_pages_seen {
                return None;
            }
            frame_pages = arguments.next()?.to_str()?.parse().ok()?;
            frame_pages_seen = true;
        } else if ui && argument.to_str() == Some("--port") {
            if port_seen {
                return None;
            }
            port = arguments.next()?.to_str()?.parse().ok()?;
            port_seen = true;
        } else if ui && argument.to_str() == Some("--open") {
            if open {
                return None;
            }
            open = true;
        } else if ask && argument.to_str() == Some("--yes") {
            if yes {
                return None;
            }
            yes = true;
        } else if !arguments.after_separator
            && (ask || !(ui || mcp || pack || dump || compact || stats || activate))
            && argument.to_str() == Some("--read-only")
        {
            if read_only {
                return None;
            }
            read_only = true;
        } else if path.is_none()
            && (arguments.after_separator || !argument.to_string_lossy().starts_with("--"))
        {
            path = Some(argument);
        } else if arguments.after_separator || !argument.to_string_lossy().starts_with("--") {
            // A positional after the path: the question (ask) or the
            // output path (pack); after `--` even a dash-led token counts.
            if ask && question.is_none() {
                question = Some(argument.into_string().ok()?);
            } else if pack && output.is_none() {
                output = Some(argument);
            } else {
                return None;
            }
        } else {
            return None;
        }
    }
    let path = path?;
    if activate {
        Some(Command::Activate { path })
    } else if pack {
        Some(Command::Pack {
            input: path,
            output: output?,
            frame_pages,
            options,
        })
    } else if dump {
        Some(Command::Dump { path, options })
    } else if compact {
        Some(Command::Compact { path, options })
    } else if stats {
        Some(Command::Stats { path, options })
    } else if ui {
        Some(Command::Ui {
            path,
            options,
            port,
            open,
        })
    } else if ask {
        Some(Command::Ask {
            path,
            options,
            question: question?,
            yes,
            read_only,
        })
    } else if mcp {
        Some(Command::Mcp { path, options })
    } else {
        Some(Command::Repl {
            path,
            options,
            read_only,
        })
    }
}

fn run_stats(path: &Path, memory_limit: usize) -> ExitCode {
    let stats = match storage_stats::collect(path, memory_limit) {
        Ok(stats) => stats,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = io::stdout().lock();
    if let Err(error) = storage_stats::write_text(&stats, &mut stdout) {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    if let Err(error) = stdout.flush() {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run_dump(path: &Path, options: Options) -> ExitCode {
    let database = match Database::open_inspect_with(path, options) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = io::stdout().lock();
    if let Err(error) = database.dump(&mut stdout) {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    if let Err(error) = stdout.flush() {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run_compact(path: &Path, options: Options) -> ExitCode {
    match compact_database(path, options) {
        Ok(stats) => {
            println!(
                "compacted {}: {} -> {} bytes",
                path.display(),
                stats.before_bytes,
                stats.after_bytes
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Clone, Copy)]
struct CliCompactStats {
    before_bytes: u64,
    after_bytes: u64,
}

fn compact_database(path: &Path, options: Options) -> devondb::DevonResult<CliCompactStats> {
    let main = fs::canonicalize(path)?;
    refuse_cli_wal(&main)?;
    let before_bytes = fs::metadata(&main)?.len();
    let permissions = fs::metadata(&main)?.permissions();
    let page_size = main_page_size(&main)?;
    let coordinated = main_has_feature(&main, 1 << 8)?;
    let mut source = Database::open_with(&main, options)?;
    refuse_cli_wal(&main)?;
    let publication = acquire_cli_publication_gate(&main)?;

    let temporary = cli_temporary_path(&main);
    let mut cleanup = CliTemporary::new(temporary.clone());
    let mut destination = Database::create_with(
        &temporary,
        Options {
            page_size,
            ..options
        },
    )?;
    restore_database(&mut source, &mut destination)?;
    destination.checkpoint()?;
    drop(destination);
    if coordinated {
        Database::activate_multiprocess(&temporary)?;
    }
    cleanup_temp_sidecars(&temporary);
    fs::set_permissions(&temporary, permissions)?;
    File::options()
        .read(true)
        .write(true)
        .open(&temporary)?
        .sync_all()?;
    let after_bytes = fs::metadata(&temporary)?.len();

    fs::rename(&temporary, &main)?;
    cleanup.disarm();
    sync_cli_parent(&main)?;
    drop(publication);
    drop(source);
    Ok(CliCompactStats {
        before_bytes,
        after_bytes,
    })
}

fn restore_database(source: &mut Database, destination: &mut Database) -> devondb::DevonResult<()> {
    let summary = source.schema_summary();
    restore_ddl(source, destination)?;
    restore_nodes(source, destination, &summary)?;
    restore_relationships(source, destination, &summary)?;
    restore_pins(destination, &summary)
}

fn restore_ddl(source: &Database, destination: &mut Database) -> devondb::DevonResult<()> {
    let mut dump = Vec::new();
    source.dump(&mut dump)?;
    let dump = String::from_utf8(dump)
        .map_err(|_| invalid_cli_compact("canonical dump is not valid UTF-8"))?;
    for line in dump.lines() {
        let Parsed::Statement(envelope) = parse(line)? else {
            return Err(invalid_cli_compact("canonical dump emitted a query"));
        };
        if matches!(
            &envelope.stmt,
            Statement::CreateNodeTable { .. }
                | Statement::CreateRelTable { .. }
                | Statement::CreateInterface { .. }
                | Statement::CreateClass { .. }
        ) {
            destination.execute(&envelope.stmt)?;
        }
    }
    Ok(())
}

fn restore_nodes(
    source: &mut Database,
    destination: &mut Database,
    summary: &SchemaSummary,
) -> devondb::DevonResult<()> {
    for table in &summary.node_tables {
        let plan = parse_query(&format!(
            "nodes({}) as compact_row",
            quote_identifier(&table.name)
        ))?;
        let result = source.run(&plan)?;
        for rows in result.rows.chunks(512) {
            destination.execute(&Statement::InsertNode {
                table: table.name.clone(),
                rows: rows.to_vec(),
            })?;
        }
    }
    Ok(())
}

fn restore_relationships(
    source: &Database,
    destination: &mut Database,
    summary: &SchemaSummary,
) -> devondb::DevonResult<()> {
    let keys = endpoint_keys(source, summary)?;
    for relationship in &summary.rel_tables {
        let from_keys = keys.get(&relationship.from).ok_or_else(|| {
            invalid_cli_compact("relationship source table is missing from endpoint keys")
        })?;
        let to_keys = keys.get(&relationship.to).ok_or_else(|| {
            invalid_cli_compact("relationship destination table is missing from endpoint keys")
        })?;
        let cursor = source.visible_relationships(&relationship.name)?;
        let mut rows = Vec::new();
        for edge in cursor {
            let edge = edge?;
            let from = endpoint_key(from_keys, edge.from_offset)?;
            let to = endpoint_key(to_keys, edge.to_offset)?;
            rows.push(rel_row_text(from, to, &edge.values));
            if rows.len() == 256 {
                flush_rel_rows(destination, &relationship.name, &mut rows)?;
            }
        }
        flush_rel_rows(destination, &relationship.name, &mut rows)?;
    }
    Ok(())
}

fn endpoint_keys(
    source: &Database,
    summary: &SchemaSummary,
) -> devondb::DevonResult<BTreeMap<String, BTreeMap<u64, Value>>> {
    let mut tables = BTreeMap::new();
    for table in &summary.node_tables {
        let primary = table
            .columns
            .iter()
            .position(|column| column.primary_key)
            .ok_or_else(|| invalid_cli_compact("node table has no primary key"))?;
        let mut keys = BTreeMap::new();
        for row in source.visible_nodes(&table.name)? {
            let row = row?;
            let key = row
                .values
                .get(primary)
                .ok_or_else(|| invalid_cli_compact("visible node row is missing its key"))?;
            keys.insert(row.offset, key.clone());
        }
        tables.insert(table.name.clone(), keys);
    }
    Ok(tables)
}

fn endpoint_key(keys: &BTreeMap<u64, Value>, offset: u64) -> devondb::DevonResult<&Value> {
    keys.get(&offset)
        .ok_or_else(|| invalid_cli_compact("relationship endpoint offset is missing"))
}

fn rel_row_text(from: &Value, to: &Value, values: &[Value]) -> String {
    let properties = if values.is_empty() {
        String::new()
    } else {
        format!(
            ", {}",
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!("({from} -> {to}{properties})")
}

fn flush_rel_rows(
    destination: &mut Database,
    table: &str,
    rows: &mut Vec<String>,
) -> devondb::DevonResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let text = format!(
        "insert rel into {} values {}",
        quote_identifier(table),
        rows.join(", ")
    );
    rows.clear();
    execute_statement_text(destination, &text)
}

fn restore_pins(destination: &mut Database, summary: &SchemaSummary) -> devondb::DevonResult<()> {
    for pin in &summary.pins {
        let plan = parse_query(&pin.canonical)?;
        destination.pin(&pin.name, &pin.text, &plan)?;
    }
    Ok(())
}

fn parse_query(text: &str) -> devondb::DevonResult<Plan> {
    match parse(text)? {
        Parsed::Query(plan) => Ok(plan),
        Parsed::Statement(_) => Err(invalid_cli_compact("compact expected a query")),
    }
}

fn execute_statement_text(database: &mut Database, text: &str) -> devondb::DevonResult<()> {
    match parse(text)? {
        Parsed::Statement(envelope) => database.execute(&envelope.stmt),
        Parsed::Query(_) => Err(invalid_cli_compact("compact expected a statement")),
    }
}

fn quote_identifier(identifier: &str) -> String {
    let mut quoted = String::from("`");
    for character in identifier.chars() {
        push_escaped(&mut quoted, character, '`');
    }
    quoted.push('`');
    quoted
}

fn push_escaped(output: &mut String, character: char, delimiter: char) {
    match character {
        '\\' => output.push_str("\\\\"),
        '\n' => output.push_str("\\n"),
        '\r' => output.push_str("\\r"),
        '\t' => output.push_str("\\t"),
        value if value == delimiter => {
            output.push('\\');
            output.push(value);
        }
        value if value.is_control() => output.push_str(&format!("\\u{{{:x}}}", value as u32)),
        value => output.push(value),
    }
}

fn refuse_cli_wal(main: &Path) -> devondb::DevonResult<()> {
    let wal = cli_append_suffix(main, "-wal");
    match fs::metadata(&wal) {
        Ok(metadata) if metadata.len() != 0 => Err(devondb::DevonError::Busy {
            context: format!(
                "compact refused: WAL {} is non-empty; checkpoint the database first",
                wal.display()
            ),
        }),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn acquire_cli_publication_gate(main: &Path) -> devondb::DevonResult<File> {
    let path = cli_append_suffix(main, ".lock-publish");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(devondb::DevonError::Busy {
            context: format!("publication gate is held for {}", path.display()),
        }),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn main_page_size(main: &Path) -> devondb::DevonResult<u32> {
    let mut file = File::open(main)?;
    let mut header = [0_u8; 64];
    file.read_exact(&mut header)?;
    Ok(u32::from_le_bytes([
        header[24], header[25], header[26], header[27],
    ]))
}

fn main_has_feature(main: &Path, feature: u64) -> devondb::DevonResult<bool> {
    let page_size = main_page_size(main)?;
    let mut file = File::open(main)?;
    let mut first = [0_u8; 64];
    let mut second = [0_u8; 64];
    file.read_exact(&mut first)?;
    file.seek(SeekFrom::Start(u64::from(page_size)))?;
    file.read_exact(&mut second)?;
    Ok((read_header_flags(&first) | read_header_flags(&second)) & feature != 0)
}

fn read_header_flags(header: &[u8; 64]) -> u64 {
    u64::from_le_bytes([
        header[16], header[17], header[18], header[19], header[20], header[21], header[22],
        header[23],
    ])
}

static CLI_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn cli_temporary_path(main: &Path) -> PathBuf {
    let sequence = CLI_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    cli_append_suffix(
        main,
        &format!(".compact-{}-{nanos}-{sequence}.tmp", std::process::id()),
    )
}

fn cli_append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    value.into()
}

fn cleanup_temp_sidecars(path: &Path) {
    for suffix in ["-wal", ".lock-writer", ".lock-publish"] {
        let _ = fs::remove_file(cli_append_suffix(path, suffix));
    }
    let _ = fs::remove_dir(cli_append_suffix(path, ".tmp"));
}

fn sync_cli_parent(path: &Path) -> devondb::DevonResult<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Ok(directory) = File::open(parent) else {
        return Ok(());
    };
    directory.sync_all()?;
    Ok(())
}

struct CliTemporary {
    path: PathBuf,
    armed: bool,
}

impl CliTemporary {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CliTemporary {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
            cleanup_temp_sidecars(&self.path);
        }
    }
}

fn invalid_cli_compact(context: &str) -> devondb::DevonError {
    devondb::DevonError::InvalidArgument {
        context: context.to_owned(),
    }
}

#[cfg(feature = "mcp")]
fn run_mcp(path: &Path, options: Options) -> ExitCode {
    // Read-only law (docs/MCP.md §2): follower open when the file is
    // multiprocess-activated, exclusive open otherwise; no tool mutates.
    let database = match devondb_server::mcp::open(path, options) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let stdin = io::stdin().lock();
    let stdout = io::stdout().lock();
    if let Err(error) = devondb_server::mcp::serve(database, stdin, stdout) {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(not(feature = "mcp"))]
fn run_mcp(_path: &Path, _options: Options) -> ExitCode {
    eprintln!("error: this binary was built without the \"mcp\" feature");
    ExitCode::FAILURE
}

#[cfg(feature = "ui")]
fn run_ui(path: &Path, options: Options, port: u16, open: bool) -> ExitCode {
    let database = match open_existing_database(path, options, false) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let url = match announce_ui(&listener) {
        Ok(url) => url,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    if open {
        open_ui(&url);
    }
    if let Err(error) = devondb_server::api::serve(database, listener) {
        eprintln!("error: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(feature = "ui")]
fn announce_ui(listener: &std::net::TcpListener) -> io::Result<String> {
    let port = listener.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/");
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "devondb ui: {url}")?;
    stdout.flush()?;
    Ok(url)
}

#[cfg(feature = "ui")]
fn open_ui(url: &str) {
    let opener = env::var_os("DEVONDB_UI_OPENER").or_else(default_ui_opener);
    let Some(opener) = opener else {
        eprintln!("devondb ui: open your browser at {url}");
        return;
    };
    if let Err(error) = std::process::Command::new(opener).arg(url).spawn() {
        eprintln!("warning: could not open browser at {url}: {error}");
    }
}

#[cfg(feature = "ui")]
fn default_ui_opener() -> Option<OsString> {
    if cfg!(target_os = "macos") {
        Some(OsString::from("open"))
    } else if cfg!(target_os = "linux") {
        Some(OsString::from("xdg-open"))
    } else {
        None
    }
}

#[cfg(not(feature = "ui"))]
fn run_ui(_path: &Path, _options: Options, _port: u16, _open: bool) -> ExitCode {
    eprintln!("error: this binary was built without the \"ui\" feature");
    ExitCode::FAILURE
}

fn open_database(path: &Path, options: Options, read_only: bool) -> devondb::DevonResult<Database> {
    if read_only {
        open_existing_database(path, options, true)
    } else if path.exists() {
        Database::open_with(path, options)
    } else {
        Database::create_with(path, options)
    }
}

fn open_existing_database(
    path: &Path,
    options: Options,
    read_only: bool,
) -> Result<Database, devondb::DevonError> {
    let opened = if read_only {
        Database::open_read_only_with(path, options)
    } else {
        Database::open_with(path, options)
    };
    opened.map_err(|error| match error {
        devondb::DevonError::Io(io_error) if io_error.kind() == io::ErrorKind::NotFound => {
            devondb::DevonError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("database file not found: {}", path.display()),
            ))
        }
        error => error,
    })
}

fn ask_question(
    database: &mut Database,
    question: &str,
    reference_date: i64,
    yes: bool,
    input: &mut impl BufRead,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<AskStatus> {
    // `compile_with_database`, never the schema-only `compile`: row-backed
    // grounding is what resolves a value to its STORED spelling (`ada` ->
    // `"Ada"`, `docs/NL.md` § entity value slots) and a place name to a geo
    // literal (§ 9). Calling `compile` here silently degrades both to
    // schema-only, which is how `devondb ask "who does ada know"` returned
    // zero rows while the compiler tests were green.
    match DeterministicCompiler.compile_at_with_database(question, database, reference_date) {
        Compiled::Plan(plan) => ask_plan(database, &plan, yes, input, stdout, stderr),
        Compiled::NoParse(report) => {
            ask_statement(database, question, report, yes, input, stdout, stderr)
        }
    }
}

fn utc_reference_date() -> i64 {
    let epoch_secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(error) => -(error.duration().as_secs() as i64),
    };
    epoch_secs - epoch_secs.rem_euclid(DAY_SECONDS)
}

fn ask_statement(
    database: &mut Database,
    input_text: &str,
    mut question_report: NoParse,
    yes: bool,
    input: &mut impl BufRead,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<AskStatus> {
    match DeterministicCompiler.compile_statement(input_text, &database.schema_summary()) {
        CompiledStatement::Statement(statement) => {
            let envelope = StatementEnvelope {
                v: 0,
                stmt: statement,
            };
            ask_compiled_statement(database, &envelope, yes, input, stdout, stderr)
        }
        CompiledStatement::NoParse(statement_report) => {
            append_statement_hints(&mut question_report, &statement_report);
            write_noparse(stderr, &question_report)?;
            Ok(AskStatus::NoParse)
        }
    }
}

fn append_statement_hints(question: &mut NoParse, statement: &NoParse) {
    let Some(statement_hint) = rank_hints(statement.nearest.clone()).into_iter().next() else {
        return;
    };
    if has_statement_head(question, statement) {
        let mut merged = vec![statement_hint];
        merged.append(&mut question.nearest);
        merged.dedup();
        merged.truncate(3);
        question.nearest = merged;
    } else {
        question.nearest.extend(statement.nearest.iter().cloned());
        question.nearest = rank_hints(std::mem::take(&mut question.nearest));
    }
}

fn has_statement_head(question: &NoParse, statement: &NoParse) -> bool {
    let Some(head) = question.recognized.first().map(|item| item.token.as_str()) else {
        return false;
    };
    statement.nearest.iter().any(|hint| {
        hint.example
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case(head))
    })
}

fn grounded_slot_count(hint: &TemplateHint) -> usize {
    const PLACEHOLDERS: [&str; 8] = [
        "<table>",
        "<column>",
        "<pk-value>",
        "<value>",
        "<n>",
        "<entity>",
        "<rel>",
        "<place>",
    ];
    PLACEHOLDERS
        .iter()
        .filter(|placeholder| hint.example.contains(*placeholder))
        .count()
}

fn rank_hints(mut hints: Vec<TemplateHint>) -> Vec<TemplateHint> {
    // Stable sort keeps each compiler's template order within one grounded-
    // slot count; sorting first also makes identical hints adjacent so the
    // dedup catches cross-report duplicates (`docs/NL.md` § 7).
    hints.sort_by_key(|hint| std::cmp::Reverse(grounded_slot_count(hint)));
    hints.dedup();
    hints.truncate(3);
    hints
}

fn ask_compiled_statement(
    database: &mut Database,
    statement: &StatementEnvelope,
    yes: bool,
    input: &mut impl BufRead,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<AskStatus> {
    // Trust-loop law: only the engine printer supplies mutation text, and
    // execution follows confirmation (`docs/UI.md` §2 rule 2 and §12.6).
    let canonical = match print_statement(statement) {
        Ok(canonical) => canonical,
        Err(error) => {
            write_error(stderr, &error)?;
            return Ok(AskStatus::Error);
        }
    };
    writeln!(stdout, "{canonical}")?;
    stdout.flush()?;
    if !yes && !confirm_run(input, stdout)? {
        return Ok(AskStatus::Declined);
    }
    match database.execute(&statement.stmt) {
        Ok(()) => {
            writeln!(stdout, "ok")?;
            stdout.flush()?;
            Ok(AskStatus::Ran)
        }
        Err(error) => {
            write_error(stderr, &error)?;
            Ok(AskStatus::Error)
        }
    }
}

fn ask_plan(
    database: &mut Database,
    plan: &Plan,
    yes: bool,
    input: &mut impl BufRead,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<AskStatus> {
    if !write_plan(plan, stdout, stderr)? {
        return Ok(AskStatus::Error);
    }
    if !yes && !confirm_run(input, stdout)? {
        return Ok(AskStatus::Declined);
    }
    match database.run(plan) {
        Ok(result) => {
            write_table(stdout, &result)?;
            stdout.flush()?;
            Ok(AskStatus::Ran)
        }
        Err(error) => {
            write_error(stderr, &error)?;
            Ok(AskStatus::Error)
        }
    }
}

fn write_plan(plan: &Plan, stdout: &mut impl Write, stderr: &mut impl Write) -> io::Result<bool> {
    let canonical = match print_plan(plan) {
        Ok(canonical) => canonical,
        Err(error) => {
            write_error(stderr, &error)?;
            return Ok(false);
        }
    };
    let tree = match plan.to_json() {
        Ok(tree) => tree,
        Err(error) => {
            write_error(stderr, &error)?;
            return Ok(false);
        }
    };
    writeln!(stdout, "{canonical}")?;
    writeln!(stdout, "plan tree: {tree}")?;
    stdout.flush()?;
    Ok(true)
}

fn confirm_run(input: &mut impl BufRead, stdout: &mut impl Write) -> io::Result<bool> {
    writeln!(stdout, "run? [y/N]")?;
    stdout.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    let answer = answer.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

fn write_noparse(output: &mut impl Write, report: &NoParse) -> io::Result<()> {
    writeln!(output, "unrecognized tokens:")?;
    for token in &report.unrecognized {
        match &token.suggestion {
            Some(suggestion) => {
                writeln!(output, "- `{}` (did you mean `{suggestion}`?)", token.token)?;
            }
            None => writeln!(output, "- `{}`", token.token)?,
        }
    }
    writeln!(output, "closest working phrasings:")?;
    for hint in &report.nearest {
        writeln!(output, "- {}", hint.example)?;
    }
    output.flush()
}

fn repl(database: &mut Database, path: &Path, memory_limit: usize) -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == ".exit" {
            return Ok(());
        }
        if input == ".checkpoint" {
            write_operation_result(&mut stdout, &mut stderr, database.checkpoint())?;
            continue;
        }
        if input == ".stats" {
            match storage_stats::collect(path, memory_limit) {
                Ok(stats) => {
                    storage_stats::write_text(&stats, &mut stdout)?;
                    stdout.flush()?;
                }
                Err(error) => {
                    writeln!(stderr, "error: {error}")?;
                    stderr.flush()?;
                }
            }
            continue;
        }
        if input.starts_with('.') {
            writeln!(stderr, "error: unknown command `{input}`")?;
            stderr.flush()?;
            continue;
        }
        if let Some(question) = ask_form(input) {
            // Orchestration boundary — the compiler stays deterministic; NL.md §16.
            let reference_date = utc_reference_date();
            ask_question(
                database,
                question,
                reference_date,
                false,
                &mut reader,
                &mut stdout,
                &mut stderr,
            )?;
            continue;
        }
        if let Some(explained) = input.strip_prefix("explain ") {
            explain_text(explained, &mut stdout, &mut stderr)?;
            continue;
        }
        if input.starts_with('{') {
            execute_json(database, input, &mut stdout, &mut stderr)?;
            continue;
        }

        execute_text(database, input, &mut stdout, &mut stderr)?;
    }
}

fn ask_form(input: &str) -> Option<&str> {
    let rest = input.strip_prefix("ask")?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim_start())
    } else {
        None
    }
}

fn explain_text(input: &str, stdout: &mut impl Write, stderr: &mut impl Write) -> io::Result<()> {
    let canonical = match parse(input) {
        Ok(Parsed::Query(plan)) => print_plan(&plan),
        Ok(Parsed::Statement(statement)) => print_statement(&statement),
        Err(error) => return write_error(stderr, &error),
    };
    match canonical {
        Ok(text) => {
            writeln!(stdout, "{text}")?;
            stdout.flush()
        }
        Err(error) => write_error(stderr, &error),
    }
}

fn execute_text(
    database: &mut Database,
    input: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<()> {
    match parse(input) {
        Ok(Parsed::Statement(envelope)) => {
            write_operation_result(stdout, stderr, database.execute(&envelope.stmt))
        }
        Ok(Parsed::Query(plan)) => execute_plan(database, &plan, stdout, stderr),
        Err(error) => write_error(stderr, &error),
    }
}

fn execute_json(
    database: &mut Database,
    input: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<()> {
    if let Ok(envelope) = StatementEnvelope::from_json(input) {
        return write_operation_result(stdout, stderr, database.execute(&envelope.stmt));
    }

    match Plan::from_json(input) {
        Ok(plan) => execute_plan(database, &plan, stdout, stderr),
        Err(error) => write_error(stderr, &error),
    }
}

fn execute_plan(
    database: &mut Database,
    plan: &Plan,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<()> {
    match database.run(plan) {
        Ok(result) => {
            write_table(stdout, &result)?;
            stdout.flush()
        }
        Err(error) => write_error(stderr, &error),
    }
}

fn write_operation_result(
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    result: devondb::DevonResult<()>,
) -> io::Result<()> {
    match result {
        Ok(()) => {
            writeln!(stdout, "ok")?;
            stdout.flush()
        }
        Err(error) => write_error(stderr, &error),
    }
}

fn write_error(stderr: &mut impl Write, error: &devondb::DevonError) -> io::Result<()> {
    writeln!(stderr, "error: {error}")?;
    stderr.flush()
}

fn write_table(output: &mut impl Write, result: &QueryResult) -> io::Result<()> {
    let header = result.columns.join(" | ");
    writeln!(output, "{header}")?;
    writeln!(output, "{}", "-".repeat(header.chars().count()))?;
    for row in &result.rows {
        let values = row
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
        writeln!(output, "{values}")?;
    }
    writeln!(output, "({} rows)", result.rows.len())
}

#[cfg(test)]
mod stats_tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{Database, Options, Parsed, parse, storage_stats};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "devondb-cli-stats-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn execute(database: &mut Database, text: &str) {
        let Parsed::Statement(statement) = parse(text).unwrap() else {
            panic!("fixture text parsed as a query");
        };
        database.execute(&statement.stmt).unwrap();
    }

    #[test]
    fn stats_accounts_real_insert_detach_and_checkpoint_cycles_read_only() {
        let directory = TestDirectory::new();
        let path = directory.0.join("detach.devondb");
        let mut database = Database::create_with(&path, Options::default()).unwrap();
        for statement in [
            "create node table Person (id Int64 primary key, score Int64)",
            "create rel table Knows from Person to Person (since Int64)",
            "insert into Person values (1, 10), (2, 20), (3, 30)",
            "insert rel into Knows values (1 -> 2, 2001), (3 -> 2, 2002)",
        ] {
            execute(&mut database, statement);
        }
        database.checkpoint().unwrap();
        execute(&mut database, "detach delete from Person where id = 3");
        database.checkpoint().unwrap();
        drop(database);

        let before = fs::read(&path).unwrap();
        let observed = storage_stats::collect(&path, 1024 * 1024).unwrap();
        let pages = observed.pages.unwrap();
        assert!(pages.live > 0);
        assert!(pages.free > 0);
        assert!(pages.metadata > 0);
        assert_eq!(
            pages.live + pages.free + pages.metadata + pages.unaccounted,
            pages.total
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(observed.wal_bytes, Some(0));
    }
}
