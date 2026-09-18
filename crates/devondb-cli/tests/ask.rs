use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

const DAY_SECONDS: i64 = 86_400;
const CANONICAL: &str = "nodes(Person) as person | filter person.name = \"ada\" | expand Knows out as other | project other.name";
const RESULT_TABLE: &str = "other.name\n----------\n\"Grace\"\n\"Linus\"\n(2 rows)\n";

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-cli-ask-{label}-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("test.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn ask_m5_real_binary_prints_canonical_plan_and_exact_rows() {
    let directory = TestDirectory::new("m5");
    let path = directory.database();
    seed_graph(&path);

    let output = run_ask(&path, "who does ada know", &["--yes"], None);

    assert!(output.status.success(), "ask failed: {output:?}");
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().next(), Some(CANONICAL));
    assert!(
        stdout
            .lines()
            .nth(1)
            .is_some_and(|line| { line.starts_with("plan tree: {\"v\":0,\"plan\":") }),
        "missing plan tree: {stdout}"
    );
    assert!(stdout.ends_with(RESULT_TABLE), "unexpected rows: {stdout}");
}

#[test]
fn ask_noparse_reports_refusal_and_exits_three() {
    let directory = TestDirectory::new("noparse");
    let path = directory.database();
    seed_graph(&path);

    let output = run_ask(&path, "people older than 30", &["--yes"], None);

    assert_eq!(
        output.status.code(),
        Some(3),
        "unexpected status: {output:?}"
    );
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "unrecognized tokens:\n\
- `older` (did you mean `over`?)\n\
- `than`\n\
closest working phrasings:\n\
- set people 30 <column> to <value>\n\
- change people 30 <column> to <value>\n\
- top 30 people with <column> over 30 by <sort-column>\n"
    );
}

#[test]
fn repl_ask_confirms_and_prints_the_query_table() {
    let directory = TestDirectory::new("repl");
    let input = format!("{}ask who does ada know\ny\n.exit\n", seed_input());

    let output = run_repl(&directory.database(), &input);

    assert!(output.status.success(), "REPL failed: {output:?}");
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&format!("ok\nok\nok\nok\n{CANONICAL}\n")));
    assert!(stdout.contains("\nrun? [y/N]\n"));
    assert!(stdout.ends_with(RESULT_TABLE), "unexpected rows: {stdout}");
}

#[test]
fn ask_decline_is_success_without_query_rows() {
    let directory = TestDirectory::new("decline");
    let path = directory.database();
    seed_graph(&path);

    let output = run_ask(&path, "who does ada know", &[], Some("n\n"));

    assert!(output.status.success(), "decline failed: {output:?}");
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with(&format!("{CANONICAL}\nplan tree: ")));
    assert!(stdout.ends_with("run? [y/N]\n"));
    assert!(!stdout.contains("other.name\n----------"));
}

/// The value the user TYPES differs in case from the value STORED.
///
/// Every other fixture in this file seeds a lowercase `ada` and asks for
/// `ada`, so it passes whether or not row-backed grounding runs — which is
/// exactly how the real binary shipped compiling `person.name = "ada"`
/// against a stored `Ada` and printing `(0 rows)` while every compiler test
/// stayed green. This case is the discriminator: it fails if `run_ask` ever
/// goes back to the schema-only `compile`, because grounding is the only
/// thing that can turn the typed `ada` into the stored `Ada`.
#[test]
fn ask_grounds_a_typed_value_to_its_stored_spelling_in_the_real_binary() {
    let directory = TestDirectory::new("grounding");
    let path = directory.database();
    let seed = "create node table Person (id Int64 primary key, name String)\n\
create rel table Knows from Person to Person\n\
insert into Person values (1, \"Ada\"), (2, \"Grace\"), (3, \"Linus\")\n\
insert rel into Knows values (1 -> 2), (1 -> 3)\n";
    let output = run_repl(&path, &format!("{seed}.exit\n"));
    assert!(output.status.success(), "seed failed: {output:?}");

    let output = run_ask(&path, "who does ada know", &["--yes"], None);

    assert!(output.status.success(), "ask failed: {output:?}");
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.lines().next(),
        Some(
            "nodes(Person) as person | filter person.name = \"Ada\" | expand Knows out as other | project other.name"
        ),
        "the plan must carry the STORED spelling, not the typed one: {stdout}"
    );
    assert!(
        stdout.ends_with("other.name\n----------\n\"Grace\"\n\"Linus\"\n(2 rows)\n"),
        "grounded plan must return the rows, not zero: {stdout}"
    );
}

#[test]
fn ask_statement_prints_canonical_confirms_executes_and_changes_the_row() {
    let directory = TestDirectory::new("statement");
    let path = directory.database();
    seed_mutable_people(&path);

    let output = run_ask(&path, "set person ada age to 39", &[], Some("y\n"));

    assert!(output.status.success(), "ask failed: {output:?}");
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "update Person set age = 39 where name = \"ada\"\nrun? [y/N]\nok\n"
    );

    let requery = run_repl(
        &path,
        "nodes(Person) as p | project p.name, p.age | sort p.name\n.exit\n",
    );
    assert!(requery.status.success(), "requery failed: {requery:?}");
    assert!(requery.stderr.is_empty());
    assert_eq!(
        String::from_utf8(requery.stdout).unwrap(),
        "p.name | p.age\n--------------\n\"ada\" | 39\n\"grace\" | 50\n(2 rows)\n"
    );
}

#[test]
fn ask_bulk_delete_refuses_with_pk_hints_and_does_not_mutate() {
    let directory = TestDirectory::new("statement-refusal");
    let path = directory.database();
    seed_mutable_people(&path);

    let output = run_ask(&path, "delete all people over 40", &["--yes"], None);

    assert_eq!(
        output.status.code(),
        Some(3),
        "unexpected status: {output:?}"
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        "unrecognized tokens:\n\
closest working phrasings:\n\
- set people 40 <column> to <value>\n\
- top 40 people with <column> over 40 by <sort-column>\n\
- how many people have <column> over 40\n"
    );

    let requery = run_repl(
        &path,
        "nodes(Person) as p | project p.name, p.age | sort p.name\n.exit\n",
    );
    assert!(requery.status.success(), "requery failed: {requery:?}");
    assert!(requery.stderr.is_empty());
    assert_eq!(
        String::from_utf8(requery.stdout).unwrap(),
        "p.name | p.age\n--------------\n\"ada\" | 36\n\"grace\" | 50\n(2 rows)\n"
    );
}

/// The §7 cap and ranking apply to the merged question + statement report.
///
/// `people 40` grounds table and value slots across the template union. The
/// six-entry pre-fix list collapses to exactly three, ranked by the merged
/// ordering with each compiler's template order as the tiebreak.
#[test]
fn merged_refusal_hints_are_capped_and_ranked_across_reports() {
    let directory = TestDirectory::new("merged-hints");
    let path = directory.database();
    seed_mutable_people(&path);

    let output = run_ask(&path, "people 40", &["--yes"], None);

    assert_eq!(
        output.status.code(),
        Some(3),
        "unexpected status: {output:?}"
    );
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "unrecognized tokens:\n\
closest working phrasings:\n\
- set people 40 <column> to <value>\n\
- change people 40 <column> to <value>\n\
- top 40 people with <column> over 40 by <sort-column>\n"
    );
}

/// A question-only refusal still obeys the merged cap after statement hints
/// are appended unconditionally (`docs/NL.md` §14).
#[test]
fn merged_refusal_hints_stay_capped_for_question_only_input() {
    let directory = TestDirectory::new("merged-question-hints");
    let path = directory.database();
    seed_mutable_people(&path);

    let output = run_ask(&path, "people older than 30", &["--yes"], None);

    assert_eq!(
        output.status.code(),
        Some(3),
        "unexpected status: {output:?}"
    );
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "unrecognized tokens:\n\
- `older` (did you mean `over`?)\n\
- `than`\n\
closest working phrasings:\n\
- set people 30 <column> to <value>\n\
- change people 30 <column> to <value>\n\
- top 30 people with <column> over 30 by <sort-column>\n"
    );
}

/// `ask` is read-shaped: a typo must not create a misleading empty database.
#[test]
fn ask_missing_database_errors_without_creating_it() {
    let directory = TestDirectory::new("missing-ask");
    let path = directory.database();

    let output = run_ask(&path, "who does ada know", &["--yes"], None);

    assert!(!output.status.success(), "unexpected success: {output:?}");
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains(&format!("database file not found: {}", path.display())),
    );
    assert!(!path.exists());
}

/// A quoted question beginning with dashes must survive after `--`.
#[test]
fn separator_allows_a_question_beginning_with_dashes() {
    let directory = TestDirectory::new("separator");
    let path = directory.database();
    seed_graph(&path);

    let mut command = Command::new(env!("CARGO_BIN_EXE_devondb"));
    command.args(["ask"]).arg(&path).arg("--").arg("--version");
    let output = command.output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(3),
        "unexpected status: {output:?}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("- `--version`"));
}

/// The CLI must supply the UTC reference date at its orchestration boundary.
///
/// `events occurred today` is the severed-proof discriminator: without
/// `compile_at`, the time phrase refuses; with it, the plan compiles against
/// rows stored for today's system-clock UTC day.
#[test]
fn time_phrase_ask_compiles_with_the_caller_utc_reference_date() {
    let directory = TestDirectory::new("time-phrase-ask");
    let path = directory.database();
    let reference_date = utc_reference_date();
    let event = epoch_seconds(reference_date);

    let seed = format!(
        "create node table Event (id Int64 primary key, title String, occurred Int64)\n\
         insert into Event values (1, \"Yesterday\", {}), (2, \"Today\", {})\n",
        reference_date - DAY_SECONDS,
        event,
    );
    let output = run_repl(&path, &format!("{seed}.exit\n"));
    assert!(output.status.success(), "seed failed: {output:?}");

    let ask = run_ask(&path, "events with occurred today", &["--yes"], None);

    assert!(ask.status.success(), "ask failed: {ask:?}");
    assert!(ask.stderr.is_empty());
    let stdout = String::from_utf8(ask.stdout).unwrap();
    let canonical = format!(
        "nodes(Event) as event | filter event.occurred >= {reference_date} \
         and event.occurred < {}",
        reference_date + DAY_SECONDS
    );
    assert!(
        stdout.starts_with(&format!("{canonical}\n")),
        "unexpected canonical time plan: {stdout}"
    );
    assert!(
        stdout.ends_with(&format!(
            "event.id | event.title | event.occurred\n---------------------------------------\n2 | \"Today\" | {event}\n(1 rows)\n"
        )),
        "unexpected rows: {stdout}"
    );
}

#[test]
fn time_phrase_repl_ask_compiles_with_the_caller_utc_reference_date() {
    let directory = TestDirectory::new("time-phrase-repl");
    let path = directory.database();
    let reference_date = utc_reference_date();
    let event = epoch_seconds(reference_date);
    let input = format!(
        "create node table Event (id Int64 primary key, title String, occurred Int64)\n\
         insert into Event values (1, \"Yesterday\", {}), (2, \"Today\", {})\n\
         ask events with occurred today\ny\n.exit\n",
        reference_date - DAY_SECONDS,
        event,
    );

    let output = run_repl(&path, &input);

    assert!(output.status.success(), "REPL failed: {output:?}");
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let canonical = format!(
        "nodes(Event) as event | filter event.occurred >= {reference_date} \
         and event.occurred < {}",
        reference_date + DAY_SECONDS
    );
    assert!(
        stdout.contains(&format!("ok\nok\n{canonical}\n")),
        "unexpected canonical time plan: {stdout}"
    );
    assert!(stdout.ends_with("(1 rows)\n"), "unexpected rows: {stdout}");
}

fn utc_reference_date() -> i64 {
    let epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock after Unix epoch")
        .as_secs() as i64;
    epoch_secs - epoch_secs.rem_euclid(DAY_SECONDS)
}

fn epoch_seconds(reference_date: i64) -> i64 {
    reference_date + 12 * 60 * 60
}

fn seed_mutable_people(path: &Path) {
    let output = run_repl(
        path,
        "create node table Person (name String primary key, age Int64)\n\
insert into Person values (\"ada\", 36), (\"grace\", 50)\n\
.exit\n",
    );
    assert!(output.status.success(), "seed failed: {output:?}");
    assert_eq!(output.stdout, b"ok\nok\n");
    assert!(output.stderr.is_empty());
}

fn seed_graph(path: &Path) {
    let output = run_repl(path, &format!("{}.exit\n", seed_input()));
    assert!(output.status.success(), "seed failed: {output:?}");
    assert_eq!(output.stdout, b"ok\nok\nok\nok\n");
    assert!(output.stderr.is_empty());
}

fn seed_input() -> &'static str {
    "create node table Person (id Int64 primary key, name String)\n\
create rel table Knows from Person to Person\n\
insert into Person values (1, \"ada\"), (2, \"Grace\"), (3, \"Linus\")\n\
insert rel into Knows values (1 -> 2), (1 -> 3)\n"
}

fn run_repl(path: &Path, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_devondb"))
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn run_ask(path: &Path, question: &str, flags: &[&str], input: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devondb"));
    command.arg("ask").arg(path).arg(question).args(flags);
    if let Some(input) = input {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    } else {
        command.output().unwrap()
    }
}

#[cfg(feature = "fts")]
#[test]
fn ask_fulltext_runs_the_grounded_search_with_default_capability() {
    let directory = TestDirectory::new("fulltext");
    let path = directory.database();
    seed_graph(&path);
    let output = run_ask(&path, "search Person name for \"ada\"", &["--yes"], None);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with("textscan(Person.name, \"ada\", k=10) as person\n"),
        "{stdout}"
    );
    assert!(stdout.contains("\"ada\""), "{stdout}");
    assert!(
        stdout.ends_with("(1 rows)\n") || stdout.ends_with("(1 row)\n"),
        "{stdout}"
    );
}
