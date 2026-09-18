#!/bin/sh
# Sequential open / one full battery / close storms while a committed WAL is
# deliberately left nonempty. REOPEN_CYCLES defaults to 100 / 1,000 / 20,000.
# Every fresh process must emit bytes identical to generator expectations.
set -eu
. "$(dirname "$0")/common.sh"
stress_init reopen-storm "${1:-smoke}"

# This scenario repeatedly recovers the same committed WAL. Disable automatic
# checkpoints for the seed and every reader; the query battery never checkpoints.
DEVONDB_AUTOCHECKPOINT=off
export DEVONDB_AUTOCHECKPOINT

SEED=${REOPEN_SEED:-275003}
case "$TIER" in
    smoke) DEFAULT_CYCLES=100 ;;
    standard) DEFAULT_CYCLES=1000 ;;
    torture) DEFAULT_CYCLES=20000 ;;
esac
CYCLES=${REOPEN_CYCLES:-$DEFAULT_CYCLES}
stress_require_uint "$CYCLES" REOPEN_CYCLES
stress_note "seed=$SEED cycles=$CYCLES"

stress_run_deadlined "$STRESS_GEN_BIN" reopen "$SCENARIO_OUT" "$SEED" \
    >"$SCENARIO_OUT/generator.log" 2>&1 \
    || stress_fail "fixture generation (see $SCENARIO_OUT/generator.log)"
cat "$SCENARIO_OUT/generator.log"

DB="$SCENARIO_OUT/reopen.devondb"
stress_cli "$DB" <"$SCENARIO_OUT/load.txt" \
    >"$SCENARIO_OUT/load.out" 2>"$SCENARIO_OUT/load.err" \
    || stress_fail "WAL seed exited nonzero"
stress_assert_quiet "$SCENARIO_OUT/load.err" "reopen seed"
WAL="$DB-wal"
[ -s "$WAL" ] || stress_fail "seed did not leave a nonempty WAL"
INITIAL_WAL_BYTES=$(wc -c <"$WAL" | tr -d ' ')

ITERATION=1
while [ "$ITERATION" -le "$CYCLES" ]; do
    OUT_FILE="$SCENARIO_OUT/cycle.out"
    ERR_FILE="$SCENARIO_OUT/cycle.err"
    stress_cli "$DB" <"$SCENARIO_OUT/queries.txt" >"$OUT_FILE" 2>"$ERR_FILE" \
        || stress_fail "reopen cycle $ITERATION exited nonzero"
    stress_assert_quiet "$ERR_FILE" "reopen cycle $ITERATION"
    if ! cmp -s "$SCENARIO_OUT/expected-query.out" "$OUT_FILE"; then
        diff "$SCENARIO_OUT/expected-query.out" "$OUT_FILE" \
            >"$SCENARIO_OUT/cycle-$ITERATION.diff" 2>&1 || true
        sed -n '1,120p' "$SCENARIO_OUT/cycle-$ITERATION.diff" >&2
        stress_fail "cycle $ITERATION query bytes changed"
    fi
    [ -s "$WAL" ] || stress_fail "cycle $ITERATION unexpectedly emptied the WAL"
    if [ $((ITERATION % 25)) -eq 0 ]; then
        stress_check_deadline
        stress_check_disk_budget
    fi
    ITERATION=$((ITERATION + 1))
done
cp "$SCENARIO_OUT/cycle.out" "$SCENARIO_OUT/final-query.out"
FINAL_WAL_BYTES=$(wc -c <"$WAL" | tr -d ' ')
[ "$FINAL_WAL_BYTES" -eq "$INITIAL_WAL_BYTES" ] \
    || stress_fail "read-only reopen changed WAL bytes: $INITIAL_WAL_BYTES -> $FINAL_WAL_BYTES"
stress_pass "$CYCLES byte-identical open/read/close cycles with WAL held at $FINAL_WAL_BYTES bytes"
