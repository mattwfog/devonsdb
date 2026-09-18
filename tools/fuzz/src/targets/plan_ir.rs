use devondb::{Plan, StatementEnvelope};

use crate::{mutate::mutate_bytes, rng::Rng};

const MAX_PLAN_INPUT_BYTES: usize = 64 * 1024;
const DECODE_BYTE_BUDGET: usize = 4 * 1024 * 1024;

/// Generates canonical plan/statement JSON plus structured and byte mutations.
pub(crate) fn generate(execution: u64, rng: &mut Rng, intensity: usize) -> Vec<u8> {
    let mut bytes = canonical_seed(rng).into_bytes();
    if execution == 0 {
        return bytes;
    }
    if execution.is_multiple_of(2) {
        structural_mutation(&mut bytes, rng);
    }
    if execution.is_multiple_of(11) {
        nesting_mutation(&mut bytes, rng);
    }
    let rounds = rng.range(1, intensity.max(1));
    mutate_bytes(&mut bytes, rng, rounds, MAX_PLAN_INPUT_BYTES);
    bytes
}

fn canonical_seed(rng: &mut Rng) -> String {
    let value = (rng.next_u64() % 100_000) as i64 - 50_000;
    match rng.index(6) {
        0 => format!(
            r#"{{"v":0,"plan":{{"op":"Project","exprs":[{{"expr":{{"add":[{{"col":"p.score"}},{{"lit":{value}}}]}},"as":"sum"}}],"input":{{"op":"ScanNodes","table":"Person","binding":"p"}}}}}}"#
        ),
        1 => format!(
            r#"{{"v":0,"plan":{{"op":"Filter","predicate":{{"and":[{{"gt":[{{"col":"p.id"}},{{"lit":{value}}}]}},{{"lit":true}}]}},"input":{{"op":"ScanNodes","table":"Person","binding":"p"}}}}}}"#
        ),
        2 => format!(
            r#"{{"v":0,"stmt":{{"stmt":"InsertNode","table":"Person","rows":[[{{"Int64":{value}}},{{"String":"seed"}}]]}}}}"#
        ),
        3 => r#"{"v":0,"stmt":{"stmt":"PinPlan","name":"seed","text":"nodes(Person) as p","plan":{"v":0,"plan":{"op":"ScanNodes","table":"Person","binding":"p"}}}}"#.to_owned(),
        4 => r#"{"v":0,"plan":{"op":"KnnScan","table":"Person","column":"embedding","query":[0.0,1.5,-2.25],"k":3,"metric":"cosine"}}"#.to_owned(),
        _ => r#"{"v":0,"plan":{"op":"Project","exprs":[{"expr":{"scoreof":"p"},"as":"score"}],"input":{"op":"TextScan","table":"Person","column":"name","query":"rust graph","k":3,"binding":"p"}}}"#.to_owned(),
    }
}

fn structural_mutation(bytes: &mut Vec<u8>, rng: &mut Rng) {
    const REPLACEMENTS: [(&[u8], &[u8]); 9] = [
        (b"\"v\":0", b"\"v\":4294967295"),
        (b"\"op\"", b"\"op\":\"ScanNodes\",\"op\""),
        (b"ScanNodes", b"Teleport"),
        (b"\"plan\"", b"\"unknown_plan\""),
        (b"\"stmt\"", b"\"stmt\":null,\"stmt\""),
        (b"\"lit\"", b"\"lit\":999999999999999999999999,\"lit\""),
        (b"[0.0,1.5,-2.25]", b"[1e999,-1e999,0]"),
        (b"\"k\":3", b"\"k\":18446744073709551615"),
        (b"}", b",\"extra\":true}"),
    ];
    let (needle, replacement) = REPLACEMENTS[rng.index(REPLACEMENTS.len())];
    if let Some(start) = find_subslice(bytes, needle, rng) {
        bytes.splice(start..start + needle.len(), replacement.iter().copied());
    }
}

fn nesting_mutation(bytes: &mut Vec<u8>, rng: &mut Rng) {
    let depth = rng.range(129, 512);
    let mut nested = Vec::with_capacity(depth * 2 + 4);
    nested.extend(std::iter::repeat_n(b'[', depth));
    nested.extend_from_slice(b"null");
    nested.extend(std::iter::repeat_n(b']', depth));
    *bytes = format!(r#"{{"v":0,"plan":{}}}"#, String::from_utf8_lossy(&nested)).into_bytes();
}

fn find_subslice(bytes: &[u8], needle: &[u8], rng: &mut Rng) -> Option<usize> {
    let positions: Vec<usize> = bytes
        .windows(needle.len())
        .enumerate()
        .filter_map(|(index, window)| (window == needle).then_some(index))
        .collect();
    positions.get(rng.index(positions.len())).copied()
}

/// Runs both real envelope decoders under the harness byte budgets.
pub(crate) fn exercise(input: &[u8]) -> Result<(), String> {
    let mut budget = DecodeBudget::new(DECODE_BYTE_BUDGET);
    budget.charge("input", input.len())?;
    if input.len() > MAX_PLAN_INPUT_BYTES {
        return Err(format!(
            "PLAN_IR input is {} bytes, over the {MAX_PLAN_INPUT_BYTES}-byte cap",
            input.len()
        ));
    }
    let text = match std::str::from_utf8(input) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("error: PLAN_IR input is not UTF-8: {error}");
            return Ok(());
        }
    };
    decode_plan(text, &mut budget)?;
    decode_statement(text, &mut budget)
}

fn decode_plan(text: &str, budget: &mut DecodeBudget) -> Result<(), String> {
    match Plan::from_json(text) {
        Ok(plan) => {
            let canonical = plan
                .to_json()
                .map_err(|error| format!("decoded plan would not re-encode: {error}"))?;
            budget.charge("canonical plan", canonical.len())
        }
        Err(error) => {
            eprintln!("error: {error}");
            Ok(())
        }
    }
}

fn decode_statement(text: &str, budget: &mut DecodeBudget) -> Result<(), String> {
    match StatementEnvelope::from_json(text) {
        Ok(statement) => {
            let canonical = statement
                .to_json()
                .map_err(|error| format!("decoded statement would not re-encode: {error}"))?;
            budget.charge("canonical statement", canonical.len())
        }
        Err(error) => {
            eprintln!("error: {error}");
            Ok(())
        }
    }
}

struct DecodeBudget {
    remaining: usize,
}

impl DecodeBudget {
    fn new(bytes: usize) -> Self {
        Self { remaining: bytes }
    }

    fn charge(&mut self, label: &str, bytes: usize) -> Result<(), String> {
        self.remaining = self.remaining.checked_sub(bytes).ok_or_else(|| {
            format!("PLAN_IR {label} exceeded the {DECODE_BYTE_BUDGET}-byte decode budget")
        })?;
        Ok(())
    }
}
