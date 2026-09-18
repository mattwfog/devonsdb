#!/bin/sh
# THE ONE-STATEMENT CHURN PATTERN — one upsert and one .checkpoint per fresh CLI process.
#
# A 500-row seed once grew from 233,472 B to 1,757,184 B after 15 cycles
# (7.53x), demonstrating severe physical amplification under repeated
# checkpoints. The expected bound is <= 2*S0 plus small allocator slack, with
# growth reaching 0 B/cycle by cycles 31-60. This CLI gate expresses
# that as <= 4x final logical content + 1 MiB fixed-format slack. Tune without
# editing via CHURN_MAX_AMPLIFICATION and CHURN_SLACK_BYTES. Raising the bound
# is evidence collection, not a fix.
#
# CHURN_CYCLES defaults: 100 smoke / 2,000 standard / 20,000 torture. The
# local full tiers carry the thousands-of-processes stress shape; smoke
# keeps the same one-process/one-statement/checkpoint mechanism inside 3 min.
set -eu
. "$(dirname "$0")/common.sh"
stress_init churn "${1:-smoke}"

SEED=${CHURN_SEED:-275002}
# Rows: smoke keeps the 500-row / one-group shape inside its 3-minute budget;
# standard and torture use a 20,000-row / ten-group shape. On the unfixed
# reuse scan, single-row churn against a multi-group table grew from 19.4 MB
# to 85 MB over 150 cycles; the 500-row shape converges and cannot expose the
# defect. Measured cost at
# 20,000 rows, debug binary: ~300 ms per cycle.
case "$TIER" in
    smoke) DEFAULT_CYCLES=100; DEFAULT_ROWS=500 ;;
    standard) DEFAULT_CYCLES=2000; DEFAULT_ROWS=20000 ;;
    torture) DEFAULT_CYCLES=20000; DEFAULT_ROWS=20000 ;;
esac
CYCLES=${CHURN_CYCLES:-$DEFAULT_CYCLES}
ROWS=${CHURN_ROWS:-$DEFAULT_ROWS}
MAX_AMP=${CHURN_MAX_AMPLIFICATION:-4}
SLACK=${CHURN_SLACK_BYTES:-1048576}
stress_require_uint "$CYCLES" CHURN_CYCLES
stress_require_uint "$ROWS" CHURN_ROWS
stress_require_uint "$MAX_AMP" CHURN_MAX_AMPLIFICATION
stress_require_uint "$SLACK" CHURN_SLACK_BYTES
CYCLE_ARTIFACT_BUDGET=$((STRESS_MAX_BYTES / 16))
MAX_BUDGET_CYCLES=$((CYCLE_ARTIFACT_BUDGET / (800 + 128)))
[ "$MAX_BUDGET_CYCLES" -gt 0 ] || stress_fail "STRESS_GB leaves no room for one churn cycle"
if [ "$CYCLES" -gt "$MAX_BUDGET_CYCLES" ]; then
    stress_note "disk cap reduces churn cycles from $CYCLES to $MAX_BUDGET_CYCLES"
    CYCLES=$MAX_BUDGET_CYCLES
fi
stress_note "seed=$SEED cycles=$CYCLES rows=$ROWS max_amplification=${MAX_AMP}x slack=$SLACK"

stress_run_deadlined "$STRESS_GEN_BIN" churn "$SCENARIO_OUT" \
    "$ROWS" "$CYCLES" "$SEED" >"$SCENARIO_OUT/generator.log" 2>&1 \
    || stress_fail "fixture generation (see $SCENARIO_OUT/generator.log)"
cat "$SCENARIO_OUT/generator.log"

DB="$SCENARIO_OUT/churn.devondb"
stress_cli "$DB" <"$SCENARIO_OUT/seed.txt" \
    >"$SCENARIO_OUT/seed.out" 2>"$SCENARIO_OUT/seed.err" \
    || stress_fail "seed CLI exited nonzero"
stress_assert_quiet "$SCENARIO_OUT/seed.err" "churn seed"
BASE_BYTES=$(wc -c <"$DB" | tr -d ' ')
: >"$SCENARIO_OUT/churn.out"
: >"$SCENARIO_OUT/churn.err"

ITERATION=0
while IFS= read -r STATEMENT; do
    ITERATION=$((ITERATION + 1))
    printf '%s\n.checkpoint\n.exit\n' "$STATEMENT" >"$SCENARIO_OUT/one.txt"
    stress_cli "$DB" <"$SCENARIO_OUT/one.txt" \
        >>"$SCENARIO_OUT/churn.out" 2>>"$SCENARIO_OUT/churn.err" \
        || stress_fail "CLI invocation $ITERATION exited nonzero"
    if [ -s "$SCENARIO_OUT/churn.err" ]; then
        stress_assert_quiet "$SCENARIO_OUT/churn.err" "churn invocation $ITERATION"
    fi
    if [ $((ITERATION % 25)) -eq 0 ]; then
        stress_check_deadline
        stress_check_disk_budget
    fi
done <"$SCENARIO_OUT/updates.txt"
[ "$ITERATION" -eq "$CYCLES" ] || stress_fail "ran $ITERATION of $CYCLES generated cycles"

OKS=$(grep -c '^ok$' "$SCENARIO_OUT/churn.out" || true)
EXPECTED_OKS=$((CYCLES * 2))
[ "$OKS" -eq "$EXPECTED_OKS" ] || stress_fail "expected $EXPECTED_OKS upsert/checkpoint ok lines, got $OKS"

stress_cli "$DB" <"$SCENARIO_OUT/queries.txt" \
    >"$SCENARIO_OUT/query.out" 2>"$SCENARIO_OUT/query.err" \
    || stress_fail "final query exited nonzero"
stress_assert_quiet "$SCENARIO_OUT/query.err" "churn final query"
if ! diff "$SCENARIO_OUT/expected-query.out" "$SCENARIO_OUT/query.out" \
    >"$SCENARIO_OUT/query.diff" 2>&1; then
    sed -n '1,120p' "$SCENARIO_OUT/query.diff" >&2
    stress_fail "final content differs from generator expectations"
fi

FINAL_BYTES=$(wc -c <"$DB" | tr -d ' ')
LOGICAL_BYTES=$(stress_manifest_get logical_bytes "$SCENARIO_OUT/manifest.txt") \
    || stress_fail "logical_bytes missing from manifest"
BOUND=$((LOGICAL_BYTES * MAX_AMP + SLACK))
[ "$FINAL_BYTES" -le "$BOUND" ] \
    || stress_fail "churn amplification: final=$FINAL_BYTES exceeds logical bound=$BOUND (logical=$LOGICAL_BYTES, baseline=$BASE_BYTES)"
stress_check_disk_budget
stress_pass "$CYCLES one-statement checkpoint processes; baseline=$BASE_BYTES final=$FINAL_BYTES bound=$BOUND; exact final contents"
