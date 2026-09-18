use std::{
    env,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use devondb::{Database, DevonError, DevonResult, Plan, Statement};
use devondb_plan::ops::Operator;
use devondb_storage::pager::Pager;
use devondb_types::value::Value;

pub(super) const ROLE_ENV: &str = "DEVONDB_MP_ROLE";
pub(super) const PATH_ENV: &str = "DEVONDB_MP_PATH";
pub(super) const CONTENDER_BATCH_ENV: &str = "DEVONDB_MP_CONTENDER_BATCH";
pub(super) const REPORT_PREFIX: &str = "DEVONDB_MP\t";
pub(super) const BATCH_ROWS: u32 = 3;
pub(super) const POLL_BOUND: Duration = Duration::from_millis(100);

const LOOP_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DataRow {
    pub(super) id: i64,
    pub(super) batch: i64,
    pub(super) ordinal: i64,
    pub(super) name: String,
}

/// Runs the configured worker role inside a self-executed integration-test process.
pub(super) fn run_if_configured() -> bool {
    let Some(role) = env::var_os(ROLE_ENV) else {
        return false;
    };
    let role = role.to_string_lossy().into_owned();
    let path = env::var_os(PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{PATH_ENV} is required for multiprocess worker `{role}`"));
    if let Err(error) = run_role(&role, &path) {
        panic!("multiprocess worker `{role}` failed: {error}");
    }
    true
}

pub(super) fn decode_rows(encoded: &str) -> Result<Vec<DataRow>, String> {
    if encoded.is_empty() {
        return Ok(Vec::new());
    }
    encoded.split(',').map(decode_row).collect()
}

fn run_role(role: &str, path: &Path) -> DevonResult<()> {
    match role {
        "writer" => run_writer(path),
        "reader" => run_reader(path),
        "contender" => run_contender(path),
        other => Err(invalid(format!("unknown multiprocess role `{other}`"))),
    }
}

fn run_writer(path: &Path) -> DevonResult<()> {
    let mut database = Database::open(path)?;
    report(path, "writer", &database, "ready", &scan(&database)?, "")?;
    for command in io::stdin().lock().lines() {
        let command = command?;
        if handle_writer_command(path, &mut database, &command)? {
            return Ok(());
        }
    }
    Err(invalid("writer command pipe closed before EXIT"))
}

fn handle_writer_command(path: &Path, database: &mut Database, command: &str) -> DevonResult<bool> {
    if let Some(batch) = command.strip_prefix("COMMIT ") {
        let batch = parse_batch(batch)?;
        database.execute(&insert_batch(batch))?;
        return report_command(path, database, "committed", format!("batch={batch}"));
    }
    if let Some(batches) = command.strip_prefix("MULTI_COMMIT ") {
        return run_multi_commit(path, database, batches);
    }
    if let Some(payload) = command.strip_prefix("LONG_COMMIT ") {
        return run_long_commit(path, database, payload);
    }
    if let Some(payload) = command.strip_prefix("PAYLOAD_COMMIT ") {
        return run_payload_commit(path, database, payload);
    }
    match command {
        "CHECKPOINT" => {
            database.checkpoint()?;
            report_command(path, database, "checkpointed", String::new())
        }
        "CHECKPOINT_IN_FLIGHT" => run_checkpoint_in_flight(path, database),
        "VERIFY" => verify_database(path, database),
        "SCAN" => report_command(path, database, "scanned", String::new()),
        "EXIT" => report_command(path, database, "exiting", String::new()),
        other => Err(invalid(format!("unknown writer command `{other}`"))),
    }
}

fn run_multi_commit(path: &Path, database: &mut Database, batches: &str) -> DevonResult<bool> {
    let (first, second) = parse_pair(batches, "batch")?;
    database.execute(&insert_batch(first))?;
    report(
        path,
        "writer",
        database,
        "mid-batch",
        &scan(database)?,
        &format!("batch={first}"),
    )?;
    database.execute(&insert_batch(second))?;
    report_command(path, database, "batch-complete", format!("batch={second}"))
}

fn run_long_commit(path: &Path, database: &mut Database, payload: &str) -> DevonResult<bool> {
    let (id, byte_len) = parse_pair(payload, "payload")?;
    let statement = insert_payload(id, byte_len)?;
    report(
        path,
        "writer",
        database,
        "long-commit-started",
        &scan(database)?,
        &format!("payload={id},bytes={byte_len}"),
    )?;
    database.execute(&statement)?;
    report_command(
        path,
        database,
        "long-committed",
        format!("payload={id},bytes={byte_len}"),
    )
}

fn run_payload_commit(path: &Path, database: &mut Database, payload: &str) -> DevonResult<bool> {
    let (id, byte_len) = parse_pair(payload, "payload")?;
    database.execute(&insert_payload(id, byte_len)?)?;
    report_command(
        path,
        database,
        "payload-committed",
        format!("payload={id},bytes={byte_len}"),
    )
}

fn run_checkpoint_in_flight(path: &Path, database: &mut Database) -> DevonResult<bool> {
    report(
        path,
        "writer",
        database,
        "checkpoint-started",
        &scan(database)?,
        "",
    )?;
    database.checkpoint()?;
    report_command(path, database, "checkpointed", String::new())
}

fn verify_database(path: &Path, database: &Database) -> DevonResult<bool> {
    let rows = scan(database)?;
    let payloads = scan_payloads(database)?;
    let ids = payloads
        .iter()
        .map(|payload| payload.id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let bytes = payloads
        .iter()
        .try_fold(0_usize, |total, payload| {
            total.checked_add(payload.byte_len)
        })
        .ok_or_else(|| invalid("verified payload byte count overflowed usize"))?;
    let note = format!(
        "tables=Batch,Payload;payload_count={};payload_ids={ids};payload_bytes={bytes}",
        payloads.len()
    );
    report(path, "writer", database, "verified", &rows, &note)?;
    Ok(false)
}

fn report_command(
    path: &Path,
    database: &Database,
    phase: &str,
    note: String,
) -> DevonResult<bool> {
    report(path, "writer", database, phase, &scan(database)?, &note)?;
    Ok(phase == "exiting")
}

fn run_reader(path: &Path) -> DevonResult<()> {
    let database = Database::open_read_only(path)?;
    let mut pinned = None;
    report(path, "reader", &database, "ready", &scan(&database)?, "")?;
    for command in io::stdin().lock().lines() {
        let command = command?;
        if handle_reader_command(path, &database, &mut pinned, &command)? {
            return Ok(());
        }
    }
    Err(invalid("reader command pipe closed before EXIT"))
}

fn handle_reader_command(
    path: &Path,
    database: &Database,
    pinned: &mut Option<(devondb::txn::Snapshot, Vec<DataRow>)>,
    command: &str,
) -> DevonResult<bool> {
    if let Some(batch) = command.strip_prefix("POLL_BATCH ") {
        return poll_for_batch(path, database, parse_batch(batch)?);
    }
    if let Some(batch) = command.strip_prefix("REFRESH_LOOP ") {
        return refresh_loop(path, database, parse_batch(batch)?);
    }
    match command {
        "REFRESH" => refresh_reader(path, database),
        "SCAN" => {
            report(path, "reader", database, "scanned", &scan(database)?, "")?;
            Ok(false)
        }
        "PIN" => pin_reader(path, database, pinned),
        "PIN_HOLD" => pin_holding_reader(path, database, pinned),
        "CHECK_PIN" => check_pinned_reader(path, database, pinned),
        "HOLD" => {
            report(path, "reader", database, "holding", &scan(database)?, "")?;
            Ok(false)
        }
        "EXIT" => {
            report(path, "reader", database, "exiting", &scan(database)?, "")?;
            Ok(true)
        }
        other => Err(invalid(format!("unknown reader command `{other}`"))),
    }
}

fn poll_for_batch(path: &Path, database: &Database, batch: u32) -> DevonResult<bool> {
    let started = Instant::now();
    let initial = scan(database)?;
    if contains_batch(&initial, batch) {
        return Err(invalid(format!(
            "POLL_BATCH {batch} began after the batch was already visible"
        )));
    }
    report(path, "reader", database, "polling", &initial, "poll-only")?;
    loop {
        if started.elapsed() >= LOOP_DEADLINE {
            return Err(invalid(format!(
                "30s deadline expired polling for batch {batch}"
            )));
        }
        thread::sleep(POLL_BOUND);
        database.refresh()?;
        let rows = scan(database)?;
        if contains_batch(&rows, batch) {
            let elapsed_ms = started.elapsed().as_millis();
            report(
                path,
                "reader",
                database,
                "observed",
                &rows,
                &format!("poll-only,elapsed_ms={elapsed_ms}"),
            )?;
            return Ok(false);
        }
    }
}

fn refresh_loop(path: &Path, database: &Database, batch: u32) -> DevonResult<bool> {
    let started = Instant::now();
    database.refresh()?;
    let initial = scan(database)?;
    if contains_batch(&initial, batch) {
        return Err(invalid(format!(
            "REFRESH_LOOP {batch} began after the batch was already visible"
        )));
    }
    report(
        path,
        "reader",
        database,
        "refresh-loop",
        &initial,
        &format!("awaiting_batch={batch}"),
    )?;
    loop {
        if started.elapsed() >= LOOP_DEADLINE {
            return Err(invalid(format!(
                "30s deadline expired in refresh loop for batch {batch}"
            )));
        }
        thread::sleep(Duration::from_millis(10));
        database.refresh()?;
        let rows = scan(database)?;
        if contains_batch(&rows, batch) {
            report(
                path,
                "reader",
                database,
                "refresh-loop-complete",
                &rows,
                &format!("batch={batch}"),
            )?;
            return Ok(false);
        }
    }
}

fn contains_batch(rows: &[DataRow], batch: u32) -> bool {
    rows.iter()
        .filter(|row| row.batch == i64::from(batch))
        .count()
        == BATCH_ROWS as usize
}

fn refresh_reader(path: &Path, database: &Database) -> DevonResult<bool> {
    let advanced = database.refresh()?;
    report(
        path,
        "reader",
        database,
        "refreshed",
        &scan(database)?,
        &format!("advanced={advanced}"),
    )?;
    Ok(false)
}

fn pin_reader(
    path: &Path,
    database: &Database,
    pinned: &mut Option<(devondb::txn::Snapshot, Vec<DataRow>)>,
) -> DevonResult<bool> {
    let snapshot = database.snapshot();
    let rows = rows_from_values(snapshot.run(&scan_plan())?.rows)?;
    report(path, "reader", database, "pinned", &rows, "")?;
    *pinned = Some((snapshot, rows));
    Ok(false)
}

fn pin_holding_reader(
    path: &Path,
    database: &Database,
    pinned: &mut Option<(devondb::txn::Snapshot, Vec<DataRow>)>,
) -> DevonResult<bool> {
    let snapshot = database.snapshot();
    let rows = rows_from_values(snapshot.run(&scan_plan())?.rows)?;
    report(path, "reader", database, "pin-holding", &rows, "")?;
    *pinned = Some((snapshot, rows));
    Ok(false)
}

fn check_pinned_reader(
    path: &Path,
    database: &Database,
    pinned: &Option<(devondb::txn::Snapshot, Vec<DataRow>)>,
) -> DevonResult<bool> {
    let (snapshot, original) = pinned
        .as_ref()
        .ok_or_else(|| invalid("CHECK_PIN received before PIN"))?;
    let current = rows_from_values(snapshot.run(&scan_plan())?.rows)?;
    let stable = current == *original;
    report(
        path,
        "reader",
        database,
        "pin-checked",
        &current,
        &format!("stable={stable}"),
    )?;
    if stable {
        Ok(false)
    } else {
        Err(invalid(
            "pinned snapshot changed across external publication",
        ))
    }
}

fn run_contender(path: &Path) -> DevonResult<()> {
    match Database::open(path) {
        Ok(mut database) => contender_acquired(path, &mut database),
        Err(error @ DevonError::Busy { .. }) => report_raw(
            path,
            "contender",
            0,
            "busy",
            &[],
            &format!("kind=Busy,error={error}"),
        ),
        Err(error) => Err(error),
    }
}

fn contender_acquired(path: &Path, database: &mut Database) -> DevonResult<()> {
    let batch = env::var(CONTENDER_BATCH_ENV).ok();
    if let Some(batch) = batch {
        let batch = parse_batch(&batch)?;
        database.execute(&insert_batch(batch))?;
        report(
            path,
            "contender",
            database,
            "committed",
            &scan(database)?,
            &format!("batch={batch}"),
        )
    } else {
        report(
            path,
            "contender",
            database,
            "acquired",
            &scan(database)?,
            "",
        )
    }
}

fn report(
    path: &Path,
    role: &str,
    database: &Database,
    phase: &str,
    rows: &[DataRow],
    note: &str,
) -> DevonResult<()> {
    report_raw(
        path,
        role,
        database.observed_commit_lsn(),
        phase,
        rows,
        note,
    )
}

fn report_raw(
    path: &Path,
    role: &str,
    commit_lsn: u64,
    phase: &str,
    rows: &[DataRow],
    note: &str,
) -> DevonResult<()> {
    let checkpoint_lsn = Pager::open(path)?.superblock().checkpoint_lsn;
    let line = format!(
        "{REPORT_PREFIX}pid={}\trole={role}\tcheckpoint={checkpoint_lsn}\tcommit={commit_lsn}\tphase={phase}\tcount={}\trows={}\tnote={}",
        std::process::id(),
        rows.len(),
        encode_rows(rows),
        sanitize(note)
    );
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}

fn scan(database: &Database) -> DevonResult<Vec<DataRow>> {
    rows_from_values(database.snapshot().run(&scan_plan())?.rows)
}

fn rows_from_values(rows: Vec<Vec<Value>>) -> DevonResult<Vec<DataRow>> {
    rows.into_iter().map(row_from_values).collect()
}

fn row_from_values(row: Vec<Value>) -> DevonResult<DataRow> {
    match row.as_slice() {
        [
            Value::Int64(id),
            Value::Int64(batch),
            Value::Int64(ordinal),
            Value::String(name),
        ] => Ok(DataRow {
            id: *id,
            batch: *batch,
            ordinal: *ordinal,
            name: name.clone(),
        }),
        other => Err(invalid(format!(
            "unexpected Batch row from scan: {other:?}"
        ))),
    }
}

fn insert_batch(batch: u32) -> Statement {
    let rows = (0..BATCH_ROWS)
        .map(|ordinal| {
            vec![
                Value::Int64(i64::from(batch * BATCH_ROWS + ordinal)),
                Value::Int64(i64::from(batch)),
                Value::Int64(i64::from(ordinal)),
                Value::String(format!("batch-{batch}-row-{ordinal}")),
            ]
        })
        .collect();
    Statement::InsertNode {
        table: "Batch".to_owned(),
        rows,
    }
}

fn insert_payload(id: u32, byte_len: u32) -> DevonResult<Statement> {
    let byte_len = usize::try_from(byte_len)
        .map_err(|error| invalid(format!("payload byte length does not fit usize: {error}")))?;
    let marker = payload_marker(id);
    let body = String::from_utf8(vec![marker; byte_len])
        .map_err(|error| invalid(format!("payload marker was not UTF-8: {error}")))?;
    Ok(Statement::InsertNode {
        table: "Payload".to_owned(),
        rows: vec![vec![
            Value::Int64(i64::from(id)),
            Value::Int64(i64::try_from(byte_len).map_err(|error| {
                invalid(format!("payload byte length does not fit i64: {error}"))
            })?),
            Value::String(body),
        ]],
    })
}

struct PayloadSummary {
    id: u32,
    byte_len: usize,
}

fn scan_payloads(database: &Database) -> DevonResult<Vec<PayloadSummary>> {
    database
        .snapshot()
        .run(&table_scan_plan("Payload", "payload"))?
        .rows
        .into_iter()
        .map(payload_from_values)
        .collect()
}

fn payload_from_values(row: Vec<Value>) -> DevonResult<PayloadSummary> {
    let [
        Value::Int64(id),
        Value::Int64(byte_len),
        Value::String(body),
    ] = row.as_slice()
    else {
        return Err(invalid(format!(
            "unexpected Payload row from scan: {row:?}"
        )));
    };
    let id = u32::try_from(*id)
        .map_err(|error| invalid(format!("payload id `{id}` is invalid: {error}")))?;
    let byte_len = usize::try_from(*byte_len).map_err(|error| {
        invalid(format!(
            "payload byte length `{byte_len}` is invalid: {error}"
        ))
    })?;
    if body.len() != byte_len
        || !body
            .as_bytes()
            .iter()
            .all(|byte| *byte == payload_marker(id))
    {
        return Err(invalid(format!(
            "payload {id} failed full-scan integrity: declared={byte_len}, actual={}",
            body.len()
        )));
    }
    Ok(PayloadSummary { id, byte_len })
}

fn payload_marker(id: u32) -> u8 {
    b'a' + u8::try_from(id % 26).unwrap_or(0)
}

fn scan_plan() -> Plan {
    table_scan_plan("Batch", "batch")
}

fn table_scan_plan(table: &str, binding: &str) -> Plan {
    Plan {
        v: 0,
        plan: Operator::ScanNodes {
            table: table.to_owned(),
            binding: binding.to_owned(),
        },
    }
}

fn encode_rows(rows: &[DataRow]) -> String {
    rows.iter()
        .map(|row| format!("{}:{}:{}:{}", row.id, row.batch, row.ordinal, row.name))
        .collect::<Vec<_>>()
        .join(",")
}

fn decode_row(encoded: &str) -> Result<DataRow, String> {
    let mut fields = encoded.splitn(4, ':');
    let id = parse_row_number(fields.next(), "id", encoded)?;
    let batch = parse_row_number(fields.next(), "batch", encoded)?;
    let ordinal = parse_row_number(fields.next(), "ordinal", encoded)?;
    let name = fields
        .next()
        .ok_or_else(|| format!("row `{encoded}` has no name"))?
        .to_owned();
    Ok(DataRow {
        id,
        batch,
        ordinal,
        name,
    })
}

fn parse_row_number(field: Option<&str>, name: &str, row: &str) -> Result<i64, String> {
    field
        .ok_or_else(|| format!("row `{row}` has no {name}"))?
        .parse()
        .map_err(|error| format!("row `{row}` has invalid {name}: {error}"))
}

fn parse_batch(batch: &str) -> DevonResult<u32> {
    batch
        .parse()
        .map_err(|error| invalid(format!("invalid batch `{batch}`: {error}")))
}

fn parse_pair(values: &str, kind: &str) -> DevonResult<(u32, u32)> {
    let mut values = values.split_whitespace();
    let first = values
        .next()
        .ok_or_else(|| invalid(format!("{kind} command omitted its first value")))?;
    let second = values
        .next()
        .ok_or_else(|| invalid(format!("{kind} command omitted its second value")))?;
    if values.next().is_some() {
        return Err(invalid(format!("{kind} command has too many values")));
    }
    Ok((parse_batch(first)?, parse_batch(second)?))
}

fn sanitize(note: &str) -> String {
    note.replace(['\t', '\r', '\n'], " ")
}

fn invalid(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}
