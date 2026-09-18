pub(crate) mod container;
pub(crate) mod nl;
pub(crate) mod plan_ir;
pub(crate) mod statements;

use std::path::Path;

use crate::{Target, rng::Rng};

/// Builds one deterministic input for a target.
pub(crate) fn generate(
    target: Target,
    execution: u64,
    rng: &mut Rng,
    intensity: usize,
    fixtures: Option<&container::Fixtures>,
) -> Result<Vec<u8>, String> {
    match target {
        Target::Statements => Ok(statements::generate(execution, rng, intensity)),
        Target::PlanIr => Ok(plan_ir::generate(execution, rng, intensity)),
        Target::Container => {
            let fixtures = fixtures.ok_or_else(|| "container fixtures are missing".to_owned())?;
            Ok(container::generate(execution, rng, intensity, fixtures))
        }
        Target::Nl => Ok(nl::generate(execution, rng, intensity)),
    }
}

/// Exercises one input inside the watchdog child.
pub(crate) fn exercise(
    target: Target,
    input: &[u8],
    scratch: &Path,
    fixture_root: Option<&Path>,
) -> Result<(), String> {
    match target {
        Target::Statements => statements::exercise(input, scratch),
        Target::PlanIr => plan_ir::exercise(input),
        Target::Container => {
            let root = fixture_root
                .ok_or_else(|| "container child did not receive fixture root".to_owned())?;
            container::exercise(input, scratch, root)
        }
        Target::Nl => nl::exercise(input),
    }
}
