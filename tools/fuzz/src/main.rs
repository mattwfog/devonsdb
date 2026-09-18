mod mutate;
mod rng;
mod runner;
mod targets;

use std::{fmt, path::Path, process::ExitCode, time::Duration};

const DEFAULT_SEED: u64 = 0x4456_4e46_555a_5a31;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Statements,
    PlanIr,
    Container,
    Nl,
}

impl Target {
    const ALL: [Self; 4] = [Self::Statements, Self::PlanIr, Self::Container, Self::Nl];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "statements" => Some(Self::Statements),
            "plan_ir" => Some(Self::PlanIr),
            "container" => Some(Self::Container),
            "nl" => Some(Self::Nl),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Statements => "statements",
            Self::PlanIr => "plan_ir",
            Self::Container => "container",
            Self::Nl => "nl",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Statements => 0,
            Self::PlanIr => 1,
            Self::Container => 2,
            Self::Nl => 3,
        }
    }

    fn seed_salt(self) -> u64 {
        match self {
            Self::Statements => 0x5354_4d54_0000_0001,
            Self::PlanIr => 0x504c_414e_0000_0002,
            Self::Container => 0x434f_4e54_0000_0003,
            Self::Nl => 0x4e4c_434f_4d50_0004,
        }
    }

    fn input_cap(self) -> u64 {
        match self {
            Self::Statements | Self::PlanIr | Self::Nl => 64 * 1024,
            Self::Container => 4 * 1024 * 1024,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

struct RunConfig {
    target: Target,
    seed: u64,
    limit: RunLimit,
    intensity: usize,
}

impl RunConfig {
    fn should_continue(&self, executions: u64, elapsed: Duration) -> bool {
        match self.limit {
            RunLimit::Iterations(limit) => executions < limit,
            RunLimit::Duration(limit) => executions == 0 || elapsed < limit,
        }
    }
}

#[derive(Clone, Copy)]
enum RunLimit {
    Iterations(u64),
    Duration(Duration),
}

fn main() -> ExitCode {
    match real_main() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("harness error: {error}");
            ExitCode::from(2)
        }
    }
}

fn real_main() -> Result<bool, String> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = arguments.first().map(String::as_str) else {
        return Err(usage());
    };
    if command == "__case" {
        return run_hidden_case(&arguments).map(|()| true);
    }
    if command == "regressions" {
        return runner::regressions();
    }
    let target = Target::parse(command).ok_or_else(usage)?;
    let config = parse_run_config(target, &arguments[1..])?;
    println!(
        "fuzz target={} seed={:#018x} intensity={}",
        config.target, config.seed, config.intensity
    );
    runner::run(&config)
}

fn parse_run_config(target: Target, arguments: &[String]) -> Result<RunConfig, String> {
    let mut limit = None;
    let mut intensity = 8_usize;
    let mut seed = environment_seed()?;
    let mut index = 0;
    while index < arguments.len() {
        let flag = arguments[index].as_str();
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value\n{}", usage()))?;
        match flag {
            "--iterations" => {
                let count = parse_positive_u64(value, "iterations")?;
                set_limit(&mut limit, RunLimit::Iterations(count))?;
            }
            "--seconds" => {
                let seconds = parse_positive_u64(value, "seconds")?;
                set_limit(&mut limit, RunLimit::Duration(Duration::from_secs(seconds)))?;
            }
            "--intensity" => intensity = parse_positive_usize(value, "intensity")?,
            "--seed" => seed = parse_seed(value)?,
            _ => return Err(format!("unknown option `{flag}`\n{}", usage())),
        }
        index += 2;
    }
    Ok(RunConfig {
        target,
        seed,
        limit: limit.unwrap_or(RunLimit::Iterations(64)),
        intensity,
    })
}

fn set_limit(destination: &mut Option<RunLimit>, value: RunLimit) -> Result<(), String> {
    if destination.is_some() {
        return Err("choose only one of --iterations and --seconds".to_owned());
    }
    *destination = Some(value);
    Ok(())
}

fn run_hidden_case(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 5 {
        return Err(
            "internal __case requires target, input, scratch, and fixture paths".to_owned(),
        );
    }
    let target = Target::parse(&arguments[1])
        .ok_or_else(|| format!("internal child target is unknown: {}", arguments[1]))?;
    let input_path = Path::new(&arguments[2]);
    let length = std::fs::metadata(input_path)
        .map_err(|error| format!("child stat input {}: {error}", input_path.display()))?
        .len();
    if length > target.input_cap() {
        return Err(format!(
            "{} input is {length} bytes, over the {}-byte preallocation cap",
            target,
            target.input_cap()
        ));
    }
    let input = std::fs::read(input_path)
        .map_err(|error| format!("child read input {}: {error}", input_path.display()))?;
    let fixture = (arguments[4] != "-").then_some(Path::new(&arguments[4]));
    targets::exercise(target, &input, Path::new(&arguments[3]), fixture)
}

fn environment_seed() -> Result<u64, String> {
    match std::env::var("FUZZ_SEED") {
        Ok(value) => parse_seed(&value),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_SEED),
        Err(error) => Err(format!("FUZZ_SEED is not valid Unicode: {error}")),
    }
}

fn parse_seed(value: &str) -> Result<u64, String> {
    let parsed = if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else {
        value.parse::<u64>()
    };
    parsed.map_err(|error| format!("invalid seed `{value}`: {error}"))
}

fn parse_positive_u64(value: &str, label: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|error| format!("invalid {label} `{value}`: {error}"))?;
    if parsed == 0 {
        return Err(format!("{label} must be positive"));
    }
    Ok(parsed)
}

fn parse_positive_usize(value: &str, label: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid {label} `{value}`: {error}"))?;
    if parsed == 0 {
        return Err(format!("{label} must be positive"));
    }
    Ok(parsed)
}

fn usage() -> String {
    "usage: devondb-fuzz <statements|plan_ir|container|nl> [--iterations N | --seconds N] [--intensity N] [--seed N]\n       devondb-fuzz regressions".to_owned()
}
