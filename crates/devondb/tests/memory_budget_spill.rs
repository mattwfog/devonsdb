use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb_storage::{budget::MemoryBudget, pager::Pager};

const NODE_COUNT: usize = 2_048;
const EDGE_COUNT: usize = NODE_COUNT * 2;
const BATCH_SIZE: usize = 128;
const PAYLOAD_BYTES: usize = 64;
const MEMORY_LIMIT: usize = 1024 * 1024;
const RLIMIT_AS_BYTES: u64 = 256 * 1024 * 1024;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-memory-budget-spill-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("social.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn sort_and_aggregate_spill_inside_memory_budget_and_rlimit() {
    run_spill_workload(true);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn sort_and_aggregate_spill_inside_memory_budget() {
    run_spill_workload(false);
}

fn run_spill_workload(enforce_rlimit: bool) {
    let directory = TestDirectory::new();
    let database_path = directory.database();
    let spill_dir = spill_tmp_path(&database_path);
    fs::create_dir(&spill_dir).unwrap();
    let stale_create_run = spill_dir.join("stale-create.run");
    fs::write(&stale_create_run, b"stale").unwrap();
    let seed = seed_script();
    let seeded = run_session(&database_path, &seed, enforce_rlimit);
    assert_success(&seeded);
    assert!(
        seeded.stderr.is_empty(),
        "seed stderr: {}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    assert!(!stale_create_run.exists());
    let expected_ok = seed.lines().filter(|line| *line != ".exit").count();
    assert_eq!(seeded.stdout, b"ok\n".repeat(expected_ok));

    fs::create_dir_all(&spill_dir).unwrap();
    fs::write(spill_dir.join("stale-open.run"), b"stale").unwrap();
    let queried = run_session(&database_path, &query_script(), enforce_rlimit);
    assert_success(&queried);
    assert!(
        queried.stderr.is_empty(),
        "query stderr: {}",
        String::from_utf8_lossy(&queried.stderr)
    );
    assert_eq!(queried.stdout, expected_query_output().as_bytes());

    assert!(
        spill_dir.is_dir(),
        "calibrated workload did not create spill runs"
    );
    assert_eq!(fs::read_dir(&spill_dir).unwrap().count(), 0);

    let peak = measure_page_cache_peak(&database_path);
    eprintln!(
        "peak MemoryBudget::charged()={peak} bytes; nodes={NODE_COUNT}; edges={EDGE_COUNT}; payload={PAYLOAD_BYTES}; memory_limit={MEMORY_LIMIT}; RLIMIT_AS={RLIMIT_AS_BYTES}"
    );
    assert!(peak > 0);
    assert!(peak <= MEMORY_LIMIT);
}

fn seed_script() -> String {
    let mut script = String::from(
        "create node table Person (id Int64 primary key, name String, age Int64)\n\
         create rel table Knows from Person to Person\n",
    );
    append_node_batches(&mut script);
    append_edge_batches(&mut script);
    script.push_str(".checkpoint\n.exit\n");
    script
}

fn append_node_batches(script: &mut String) {
    for (batch_index, start) in (0..NODE_COUNT).step_by(BATCH_SIZE).enumerate() {
        script.push_str("insert into Person values ");
        let end = (start + BATCH_SIZE).min(NODE_COUNT);
        for id in start..end {
            if id > start {
                script.push_str(", ");
            }
            script.push_str(&format!(
                "({id}, \"{}\", {})",
                person_name(id),
                20 + id % 50
            ));
        }
        script.push('\n');
        append_periodic_checkpoint(script, batch_index + 1);
    }
}

fn append_edge_batches(script: &mut String) {
    for (batch_index, start) in (0..NODE_COUNT).step_by(BATCH_SIZE).enumerate() {
        script.push_str("insert rel into Knows values ");
        let end = (start + BATCH_SIZE).min(NODE_COUNT);
        for id in start..end {
            if id > start {
                script.push_str(", ");
            }
            let next = (id + 1) % NODE_COUNT;
            let seventh = (id + 7) % NODE_COUNT;
            script.push_str(&format!("({id} -> {next}), ({id} -> {seventh})"));
        }
        script.push('\n');
        append_periodic_checkpoint(script, batch_index + 1);
    }
}

fn append_periodic_checkpoint(script: &mut String, completed_batches: usize) {
    if completed_batches.is_multiple_of(8) {
        script.push_str(".checkpoint\n");
    }
}

fn query_script() -> String {
    String::from(
        "nodes(Person) as p | sort p.name desc, p.name desc, p.name desc, p.name desc | limit 3 | project p.id, p.name\n\
         nodes(Person) as p | aggregate count(p.id) as people, min(p.name) as first1, max(p.name) as last1, min(p.name) as first2, max(p.name) as last2, min(p.name) as first3, max(p.name) as last3, avg(p.age) as mean_age by p.age | sort p.age\n\
         .exit\n",
    )
}

fn expected_query_output() -> String {
    let mut output = String::new();
    let sort_header = "p.id | p.name";
    output.push_str(sort_header);
    output.push('\n');
    output.push_str(&"-".repeat(sort_header.len()));
    output.push('\n');
    for id in (NODE_COUNT - 3..NODE_COUNT).rev() {
        output.push_str(&format!("{id} | \"{}\"\n", person_name(id)));
    }
    output.push_str("(3 rows)\n");

    let aggregate_header =
        "p.age | people | first1 | last1 | first2 | last2 | first3 | last3 | mean_age";
    output.push_str(aggregate_header);
    output.push('\n');
    output.push_str(&"-".repeat(aggregate_header.len()));
    output.push('\n');
    for residue in 0..50 {
        let count = (NODE_COUNT - 1 - residue) / 50 + 1;
        let last = residue + 50 * ((NODE_COUNT - 1 - residue) / 50);
        let age = residue + 20;
        output.push_str(&format!(
            "{age} | {count} | \"{}\" | \"{}\" | \"{}\" | \"{}\" | \"{}\" | \"{}\" | {age}\n",
            person_name(residue),
            person_name(last),
            person_name(residue),
            person_name(last),
            person_name(residue),
            person_name(last)
        ));
    }
    output.push_str("(50 rows)\n");
    output
}

fn person_name(id: usize) -> String {
    format!("person-{id:06}-{}", "x".repeat(PAYLOAD_BYTES))
}

fn run_session(path: &Path, input: &str, enforce_rlimit: bool) -> Output {
    let mut command = Command::new(cli_binary());
    command
        .arg(path)
        .arg("--memory-limit")
        .arg(MEMORY_LIMIT.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_rlimit(&mut command, enforce_rlimit);
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn cli_binary() -> &'static Path {
    static CLI_BINARY: OnceLock<PathBuf> = OnceLock::new();
    CLI_BINARY
        .get_or_init(|| {
            let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let target = workspace.join("target/memory-budget-spill-cli");
            let status = Command::new(env!("CARGO"))
                .current_dir(&workspace)
                .args(["build", "-p", "devondb-cli", "--bin", "devondb"])
                .arg("--target-dir")
                .arg(&target)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "failed to build the real devondb CLI binary"
            );
            target.join("debug").join(binary_name())
        })
        .as_path()
}

fn binary_name() -> OsString {
    let mut name = OsString::from("devondb");
    name.push(std::env::consts::EXE_SUFFIX);
    name
}

fn spill_tmp_path(path: &Path) -> PathBuf {
    let mut path = OsString::from(path.as_os_str());
    path.push(".tmp");
    PathBuf::from(path)
}

fn measure_page_cache_peak(path: &Path) -> usize {
    let budget = Arc::new(MemoryBudget::new(MEMORY_LIMIT));
    let pager = Pager::open(path).unwrap().with_budget(Arc::clone(&budget));
    let page_size = u64::from(pager.superblock().page_size);
    let page_count = fs::metadata(path).unwrap().len() / page_size;
    let mut peak = 0;
    for page_id in 2..page_count {
        drop(pager.read_page_ref(page_id).unwrap());
        peak = peak.max(budget.charged());
    }
    peak
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "child failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(not(target_os = "linux"))]
fn configure_rlimit(_command: &mut Command, enforce: bool) {
    assert!(!enforce, "RLIMIT_AS is enforced only on Linux");
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn configure_rlimit(command: &mut Command, enforce: bool) {
    use std::os::unix::process::CommandExt;

    if !enforce {
        return;
    }
    // SAFETY: the closure calls only Linux setrlimit before exec, does not
    // allocate on success, and captures the numeric limit by value.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::RLimit {
                current: RLIMIT_AS_BYTES,
                maximum: RLIMIT_AS_BYTES,
            };
            // SAFETY: `limit` points to a valid C-layout rlimit for the
            // duration of the call and RLIMIT_AS is a valid Linux resource.
            if libc::setrlimit(libc::RLIMIT_AS, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod libc {
    /// Linux's `struct rlimit` on supported 64-bit CI targets.
    #[repr(C)]
    pub struct RLimit {
        pub current: u64,
        pub maximum: u64,
    }

    pub const RLIMIT_AS: i32 = 9;

    unsafe extern "C" {
        pub fn setrlimit(resource: i32, limits: *const RLimit) -> i32;
    }
}
