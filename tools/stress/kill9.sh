#!/bin/sh
# Seeded SIGKILL recovery loop. KILL9_N defaults to 3 / 20 / 200. Each
# writer receives a long stream of atomic batch inserts, is killed after a
# generator-selected delay, and must recover an exact committed prefix.
set -eu
. "$(dirname "$0")/common.sh"
stress_init kill9 "${1:-smoke}"

SEED=${KILL9_SEED:-275004}
case "$TIER" in
    smoke) DEFAULT_N=3; WORK_ROWS=100000 ;;
    standard) DEFAULT_N=20; WORK_ROWS=250000 ;;
    torture) DEFAULT_N=200; WORK_ROWS=1000000 ;;
esac
N=${KILL9_N:-$DEFAULT_N}
WORK_ROWS=${KILL9_ROWS:-$WORK_ROWS}
BATCH=${KILL9_BATCH:-10}
PAYLOAD_LEN=${KILL9_PAYLOAD_BYTES:-256}
stress_require_uint "$N" KILL9_N
stress_require_uint "$WORK_ROWS" KILL9_ROWS
stress_require_uint "$BATCH" KILL9_BATCH
stress_require_uint "$PAYLOAD_LEN" KILL9_PAYLOAD_BYTES
[ $((WORK_ROWS % BATCH)) -eq 0 ] || stress_fail "KILL9_ROWS must be divisible by KILL9_BATCH"
WORK_ARTIFACT_BUDGET=$((STRESS_MAX_BYTES / 8))
MAX_BUDGET_ROWS=$((WORK_ARTIFACT_BUDGET / (PAYLOAD_LEN + 96)))
MAX_BUDGET_ROWS=$((MAX_BUDGET_ROWS / BATCH * BATCH))
[ "$MAX_BUDGET_ROWS" -ge "$BATCH" ] || stress_fail "STRESS_GB leaves no room for one kill9 batch"
if [ "$WORK_ROWS" -gt "$MAX_BUDGET_ROWS" ]; then
    stress_note "disk cap reduces kill workload rows from $WORK_ROWS to $MAX_BUDGET_ROWS"
    WORK_ROWS=$MAX_BUDGET_ROWS
fi
stress_note "seed=$SEED iterations=$N work_rows=$WORK_ROWS batch=$BATCH payload_bytes=$PAYLOAD_LEN"

stress_run_deadlined "$STRESS_GEN_BIN" crash "$SCENARIO_OUT" \
    "$WORK_ROWS" "$BATCH" "$PAYLOAD_LEN" "$N" "$SEED" \
    >"$SCENARIO_OUT/generator.log" 2>&1 \
    || stress_fail "fixture generation (see $SCENARIO_OUT/generator.log)"
cat "$SCENARIO_OUT/generator.log"

ITERATION=1
while [ "$ITERATION" -le "$N" ]; do
    DB="$SCENARIO_OUT/kill-$ITERATION.devondb"
    stress_cli "$DB" <"$SCENARIO_OUT/load.txt" \
        >"$SCENARIO_OUT/load-$ITERATION.out" 2>"$SCENARIO_OUT/load-$ITERATION.err" \
        || stress_fail "iteration $ITERATION seed open failed"
    stress_assert_quiet "$SCENARIO_OUT/load-$ITERATION.err" "kill9 seed $ITERATION"
    DELAY_MS=$(awk -v iteration="$ITERATION" '$1 == iteration { print $2 }' "$SCENARIO_OUT/delays.tsv")
    [ -n "$DELAY_MS" ] || stress_fail "missing delay for iteration $ITERATION"
    DELAY_SECONDS=$(awk -v ms="$DELAY_MS" 'BEGIN { printf "%.3f\n", ms / 1000 }')
    stress_note "iteration=$ITERATION delay_ms=$DELAY_MS"

    FILE_BLOCKS=$((STRESS_MAX_BYTES * 3 / 8 / 512))
    sh -c 'ulimit -f "$1" || exit 125; shift; exec "$@"' \
        stress-file-limit "$FILE_BLOCKS" "$DEVONDB_BIN" "$DB" \
        --memory-limit "$STRESS_MEMORY_BYTES" \
        <"$SCENARIO_OUT/work.txt" >"$SCENARIO_OUT/writer-$ITERATION.out" \
        2>"$SCENARIO_OUT/writer-$ITERATION.err" &
    WRITER_PID=$!
    STRESS_CHILD_PIDS="$STRESS_CHILD_PIDS $WRITER_PID"
    sleep "$DELAY_SECONDS"
    if ! kill -0 "$WRITER_PID" 2>/dev/null; then
        wait "$WRITER_PID" 2>/dev/null || true
        stress_fail "writer $ITERATION finished before its seeded kill point"
    fi
    kill -9 "$WRITER_PID" 2>/dev/null || stress_fail "SIGKILL failed for writer $ITERATION"
    WAIT_STATUS=0
    wait "$WRITER_PID" 2>/dev/null || WAIT_STATUS=$?
    STRESS_CHILD_PIDS=
    [ "$WAIT_STATUS" -ne 0 ] || stress_fail "writer $ITERATION did not report signal termination"

    stress_cli "$DB" <"$SCENARIO_OUT/queries.txt" \
        >"$SCENARIO_OUT/recovered-$ITERATION.out" \
        2>"$SCENARIO_OUT/recovered-$ITERATION.err" \
        || stress_fail "reopen after kill $ITERATION exited nonzero"
    stress_assert_quiet "$SCENARIO_OUT/recovered-$ITERATION.err" "recovery $ITERATION"

    ROW=$(sed -n '3p' "$SCENARIO_OUT/recovered-$ITERATION.out")
    COUNT=$(printf '%s\n' "$ROW" | awk -F ' \\| ' '{ print $1 }')
    TOTAL=$(printf '%s\n' "$ROW" | awk -F ' \\| ' '{ print $2 }')
    FIRST=$(printf '%s\n' "$ROW" | awk -F ' \\| ' '{ print $3 }')
    LAST=$(printf '%s\n' "$ROW" | awk -F ' \\| ' '{ print $4 }')
    MONEY=$(printf '%s\n' "$ROW" | awk -F ' \\| ' '{ print $5 }')
    case "$COUNT" in ''|*[!0-9]*) stress_fail "recovery $ITERATION produced an unparsable count: $ROW" ;; esac
    [ "$COUNT" -le "$WORK_ROWS" ] || stress_fail "recovery $ITERATION count $COUNT exceeds workload $WORK_ROWS"
    [ $((COUNT % BATCH)) -eq 0 ] || stress_fail "recovery $ITERATION exposed partial batch count $COUNT"
    if [ "$COUNT" -eq 0 ]; then
        [ "$TOTAL | $FIRST | $LAST | $MONEY" = "null | null | null | null" ] \
            || stress_fail "empty recovery $ITERATION has non-null aggregates: $ROW"
    else
        EXPECTED_TOTAL=$((COUNT * (COUNT + 1) / 2))
        EXPECTED_DECIMAL=$(awk -v cents="$EXPECTED_TOTAL" 'BEGIN { printf "decimal(\"%d.%02d\")\n", int(cents / 100), cents % 100 }')
        [ "$TOTAL" = "$EXPECTED_TOTAL" ] || stress_fail "recovery $ITERATION sum $TOTAL != $EXPECTED_TOTAL"
        [ "$FIRST" = 1 ] || stress_fail "recovery $ITERATION first id $FIRST != 1"
        [ "$LAST" = "$COUNT" ] || stress_fail "recovery $ITERATION last id $LAST != count $COUNT"
        [ "$MONEY" = "$EXPECTED_DECIMAL" ] || stress_fail "recovery $ITERATION decimal $MONEY != $EXPECTED_DECIMAL"
    fi

    stress_cli "$DB" <"$SCENARIO_OUT/checkpoint.txt" \
        >"$SCENARIO_OUT/checkpoint-$ITERATION.out" \
        2>"$SCENARIO_OUT/checkpoint-$ITERATION.err" \
        || stress_fail "checkpoint after recovery $ITERATION exited nonzero"
    stress_assert_quiet "$SCENARIO_OUT/checkpoint-$ITERATION.err" "post-kill checkpoint $ITERATION"
    stress_cli "$DB" <"$SCENARIO_OUT/queries.txt" \
        >"$SCENARIO_OUT/checkpointed-$ITERATION.out" \
        2>"$SCENARIO_OUT/checkpointed-$ITERATION.err" \
        || stress_fail "checkpointed reopen $ITERATION exited nonzero"
    stress_assert_quiet "$SCENARIO_OUT/checkpointed-$ITERATION.err" "checkpointed query $ITERATION"
    cmp -s "$SCENARIO_OUT/recovered-$ITERATION.out" "$SCENARIO_OUT/checkpointed-$ITERATION.out" \
        || stress_fail "recovery $ITERATION changed after checkpoint/reopen"

    rm -f -- "$DB" "$DB-wal" "$DB.lock-writer" "$DB.lock-publish"
    stress_check_deadline
    stress_check_disk_budget
    ITERATION=$((ITERATION + 1))
done
stress_pass "$N seeded SIGKILLs recovered only whole committed prefixes; every checkpointed reopen was byte-identical"
