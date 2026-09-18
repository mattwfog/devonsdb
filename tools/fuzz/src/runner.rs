use std::{
    collections::HashSet,
    ffi::OsStr,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{RunConfig, Target, rng::Rng, targets};

const WATCHDOG: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Runs generated cases for one target and prints its scoreboard.
pub(crate) fn run(config: &RunConfig) -> Result<bool, String> {
    let temp = TempTree::new("run")?;
    let fixtures = prepare_fixtures(config.target, temp.path())?;
    let fixture_root = fixtures.as_ref().map(targets::container::Fixtures::root);
    let started = Instant::now();
    let mut rng = Rng::new(config.seed ^ config.target.seed_salt());
    let mut executions = 0_u64;
    let mut distinct = HashSet::new();

    while config.should_continue(executions, started.elapsed()) {
        let input = targets::generate(
            config.target,
            executions,
            &mut rng,
            config.intensity,
            fixtures.as_ref(),
        )?;
        let case = run_case(config.target, executions, &input, temp.path(), fixture_root)?;
        if let Some(failure) = case {
            distinct.insert(failure.signature());
            persist_failure(config.target, config.seed, executions, &input, &failure)?;
        }
        executions += 1;
    }

    println!(
        "scoreboard target={} execs={executions} distinct_failures={}",
        config.target,
        distinct.len()
    );
    Ok(distinct.is_empty())
}

/// Replays every committed regression before a generated fuzz tier.
pub(crate) fn regressions() -> Result<bool, String> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/regressions");
    let mut files = regression_files(&directory)?;
    files.sort();
    let temp = TempTree::new("regressions")?;
    let needs_container = files
        .iter()
        .any(|path| regression_identity(path).is_ok_and(|(target, _)| target == Target::Container));
    let fixtures = if needs_container {
        Some(targets::container::build_fixtures(
            &temp.path().join("fixtures"),
        )?)
    } else {
        None
    };
    let mut executions = [0_u64; Target::ALL.len()];
    let mut distinct: [HashSet<u64>; Target::ALL.len()] = std::array::from_fn(|_| HashSet::new());
    let mut all_passed = true;

    for path in files {
        let (target, seed) = regression_identity(&path)?;
        let input = read_regression(&path, target.input_cap())?;
        let index = target.index();
        let fixture_root = fixtures
            .as_ref()
            .filter(|_| target == Target::Container)
            .map(targets::container::Fixtures::root);
        let outcome = run_case(target, executions[index], &input, temp.path(), fixture_root)?;
        executions[index] += 1;
        if let Some(failure) = outcome {
            all_passed = false;
            distinct[index].insert(failure.signature());
            eprintln!(
                "regression failed path={} target={target} seed={seed:#018x}: {}",
                path.display(),
                failure.reason
            );
        }
    }

    for target in Target::ALL {
        println!(
            "scoreboard target={} execs={} distinct_failures={}",
            target,
            executions[target.index()],
            distinct[target.index()].len()
        );
    }
    Ok(all_passed)
}

fn read_regression(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let length = fs::metadata(path)
        .map_err(|error| format!("stat regression {}: {error}", path.display()))?
        .len();
    if length > cap {
        return Err(format!(
            "regression {} is {length} bytes, over its {cap}-byte input cap",
            path.display()
        ));
    }
    fs::read(path).map_err(|error| format!("read regression {}: {error}", path.display()))
}

fn prepare_fixtures(
    target: Target,
    root: &Path,
) -> Result<Option<targets::container::Fixtures>, String> {
    if target != Target::Container {
        return Ok(None);
    }
    targets::container::build_fixtures(&root.join("fixtures")).map(Some)
}

fn run_case(
    target: Target,
    execution: u64,
    input: &[u8],
    root: &Path,
    fixture_root: Option<&Path>,
) -> Result<Option<Failure>, String> {
    let case_root = root.join(format!("case-{}-{execution}", target));
    let scratch = case_root.join("scratch");
    fs::create_dir_all(&scratch)
        .map_err(|error| format!("create case scratch {}: {error}", scratch.display()))?;
    let input_path = case_root.join("input.bin");
    fs::write(&input_path, input)
        .map_err(|error| format!("write case input {}: {error}", input_path.display()))?;
    let stdout_path = case_root.join("stdout.txt");
    let stderr_path = case_root.join("stderr.txt");
    let stdout = File::create(&stdout_path)
        .map_err(|error| format!("create child stdout {}: {error}", stdout_path.display()))?;
    let stderr = File::create(&stderr_path)
        .map_err(|error| format!("create child stderr {}: {error}", stderr_path.display()))?;

    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve fuzz executable for child: {error}"))?;
    let mut command = Command::new(executable);
    command
        .arg("__case")
        .arg(target.as_str())
        .arg(&input_path)
        .arg(&scratch)
        .arg(fixture_root.unwrap_or_else(|| Path::new("-")))
        .env("RUST_BACKTRACE", "1")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn {target} watchdog child: {error}"))?;
    let started = Instant::now();
    let outcome = loop {
        match child
            .try_wait()
            .map_err(|error| format!("poll {target} watchdog child: {error}"))?
        {
            Some(status) => break ChildExit::Status(status),
            None if started.elapsed() >= WATCHDOG => {
                let _ = child.kill();
                let status = child
                    .wait()
                    .map_err(|error| format!("reap timed-out {target} child: {error}"))?;
                break ChildExit::Timeout(status);
            }
            None => thread::sleep(POLL_INTERVAL),
        }
    };
    let stdout = read_lossy(&stdout_path)?;
    let stderr = read_lossy(&stderr_path)?;
    let _ = fs::remove_dir_all(&case_root);
    match outcome {
        ChildExit::Status(status) if status.success() => Ok(None),
        ChildExit::Status(status) => Ok(Some(Failure {
            reason: status_reason(status),
            stdout,
            stderr,
        })),
        ChildExit::Timeout(status) => Ok(Some(Failure {
            reason: format!(
                "watchdog timeout after {} ms; child was killed ({})",
                WATCHDOG.as_millis(),
                status_reason(status)
            ),
            stdout,
            stderr,
        })),
    }
}

enum ChildExit {
    Status(ExitStatus),
    Timeout(ExitStatus),
}

struct Failure {
    reason: String,
    stdout: String,
    stderr: String,
}

impl Failure {
    fn signature(&self) -> u64 {
        let mut bytes = self.reason.as_bytes().to_vec();
        bytes.extend_from_slice(self.stderr.as_bytes());
        fnv1a64(&bytes)
    }
}

fn persist_failure(
    target: Target,
    seed: u64,
    execution: u64,
    input: &[u8],
    failure: &Failure,
) -> Result<(), String> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/crashes");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("create crash corpus {}: {error}", directory.display()))?;
    let hash = fnv1a64(input);
    let stem = format!("{target}-{seed:016x}-{hash:016x}-{execution:08}");
    let binary = directory.join(format!("{stem}.bin"));
    let report = directory.join(format!("{stem}.txt"));
    fs::write(&binary, input)
        .map_err(|error| format!("persist crash input {}: {error}", binary.display()))?;
    let details = format!(
        "target={target}\nseed={seed:#018x}\nexecution={execution}\nwatchdog_ms={}\nreason={}\ninput_path={}\ninput_hex={}\n\nstdout:\n{}\n\nstderr/backtrace:\n{}\n",
        WATCHDOG.as_millis(),
        failure.reason,
        binary.display(),
        hex(input),
        failure.stdout,
        failure.stderr
    );
    fs::write(&report, details)
        .map_err(|error| format!("persist crash report {}: {error}", report.display()))?;
    eprintln!(
        "failure target={target} seed={seed:#018x} input={} report={}",
        binary.display(),
        report.display()
    );
    Ok(())
}

fn regression_files(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("read regression directory {}: {error}", directory.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read regression entry: {error}"))?;
        let path = entry.path();
        if path.extension() == Some(OsStr::new("bin")) {
            files.push(path);
        }
    }
    Ok(files)
}

fn regression_identity(path: &Path) -> Result<(Target, u64), String> {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| format!("regression filename is not UTF-8: {}", path.display()))?;
    let mut parts = name.split('-');
    let target = parts
        .next()
        .and_then(Target::parse)
        .ok_or_else(|| format!("regression filename has unknown target: {name}"))?;
    let seed_text = parts
        .next()
        .ok_or_else(|| format!("regression filename has no seed: {name}"))?;
    let seed = u64::from_str_radix(seed_text, 16)
        .map_err(|error| format!("regression filename seed is invalid in {name}: {error}"))?;
    Ok((target, seed))
}

fn status_reason(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("child terminated by signal {signal}");
        }
    }
    match status.code() {
        Some(code) => format!("child exited with status {code}"),
        None => "child exited without a status code".to_owned(),
    }
}

fn read_lossy(path: &Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|error| format!("read child output {}: {error}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock is before UNIX epoch: {error}"))?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "devondb-fuzz-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&path)
            .map_err(|error| format!("create temporary tree {}: {error}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
