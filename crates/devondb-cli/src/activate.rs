//! Offline multiprocess activation command.

use std::{path::Path, process::ExitCode};

use devondb::Database;

const MULTIPROCESS_COORDINATION_FLAG: u64 = 1 << 8;

/// Runs the offline activation command and renders its one-line result.
pub(crate) fn run(path: &Path) -> ExitCode {
    let already_active =
        super::main_has_feature(path, MULTIPROCESS_COORDINATION_FLAG).unwrap_or(false);
    match Database::activate_multiprocess(path) {
        Ok(()) => {
            if already_active {
                println!("already active {}", path.display());
            } else {
                println!("activated {}", path.display());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
