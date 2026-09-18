//! `devondb pack <in> <out>` writes the DEVONPACK read-only distribution
//! container described in `docs/SCALE.md` §7.

use std::path::Path;
use std::process::ExitCode;

#[cfg(feature = "pack")]
use devondb::Database;
use devondb::Options;

/// Writer policy default for `--frame-pages` (`docs/SCALE.md` §7.1);
/// `devondb_storage::pack::DEFAULT_FRAME_PAGES` pins the same number.
pub(crate) const DEFAULT_FRAME_PAGES: u32 = 256;

/// Opens `<in>` normally (recovery runs), checkpoints, and writes the
/// container to `<out>`.
#[cfg(feature = "pack")]
pub(crate) fn run(input: &Path, output: &Path, frame_pages: u32, options: Options) -> ExitCode {
    let mut database = match Database::open_with(input, options) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    match database.pack(output, frame_pages) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The no-feature refusal, mirroring the `mcp`/`ui` stubs.
#[cfg(not(feature = "pack"))]
pub(crate) fn run(_input: &Path, _output: &Path, _frame_pages: u32, _options: Options) -> ExitCode {
    eprintln!("error: this binary was built without the \"pack\" feature");
    ExitCode::FAILURE
}
