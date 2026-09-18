#!/bin/sh
# Volume load: default logical payload targets are 100 MiB smoke, 512 MiB
# standard, and 2 GiB torture. Override with VOLUME_MB. The effective target
# is capped at one quarter of STRESS_GB before generation, leaving room for
# the Parquet source, the database (including duplicated CSR properties), and
# transcripts. All expectations are emitted independently by stress-gen.
set -eu
. "$(dirname "$0")/common.sh"
stress_init volume "${1:-smoke}"

SEED=${VOLUME_SEED:-275001}
case "$TIER" in
    smoke) DEFAULT_MB=100 ;;
    standard) DEFAULT_MB=512 ;;
    torture) DEFAULT_MB=2048 ;;
esac
VOLUME_MB=${VOLUME_MB:-$DEFAULT_MB}
stress_require_uint "$VOLUME_MB" VOLUME_MB
REQUESTED_BYTES=$((VOLUME_MB * 1024 * 1024))
BUDGET_TARGET=$((STRESS_MAX_BYTES / 4))
if [ "$REQUESTED_BYTES" -gt "$BUDGET_TARGET" ]; then
    TARGET_BYTES=$BUDGET_TARGET
else
    TARGET_BYTES=$REQUESTED_BYTES
fi
[ "$TARGET_BYTES" -ge 8388608 ] || stress_fail "STRESS_GB leaves less than 8 MiB for the volume target"
stress_note "seed=$SEED target=$TARGET_BYTES logical bytes"

stress_run_deadlined "$STRESS_GEN_BIN" volume "$SCENARIO_OUT" \
    "$TARGET_BYTES" "$SEED" >"$SCENARIO_OUT/generator.log" 2>&1 \
    || stress_fail "fixture generation (see $SCENARIO_OUT/generator.log)"
cat "$SCENARIO_OUT/generator.log"

MANIFEST="$SCENARIO_OUT/manifest.txt"
LOGICAL_BYTES=$(stress_manifest_get logical_bytes "$MANIFEST") \
    || stress_fail "logical_bytes missing from manifest"
NODE_ROWS=$(stress_manifest_get node_rows "$MANIFEST") \
    || stress_fail "node_rows missing from manifest"
REL_ROWS=$(stress_manifest_get rel_rows "$MANIFEST") \
    || stress_fail "rel_rows missing from manifest"
[ "$LOGICAL_BYTES" -ge "$TARGET_BYTES" ] \
    || stress_fail "generator loaded $LOGICAL_BYTES logical bytes below target $TARGET_BYTES"
stress_check_disk_budget

DB="$SCENARIO_OUT/volume.devondb"
stress_cli "$DB" <"$SCENARIO_OUT/load.txt" \
    >"$SCENARIO_OUT/load.out" 2>"$SCENARIO_OUT/load.err" \
    || stress_fail "real CLI load exited nonzero"
stress_assert_quiet "$SCENARIO_OUT/load.err" "volume load"
LOAD_OK=$(grep -c '^ok$' "$SCENARIO_OUT/load.out" || true)
[ "$LOAD_OK" -eq 9 ] || stress_fail "expected 9 successful DDL/DML/COPY/checkpoint operations, got $LOAD_OK"
stress_check_disk_budget

stress_cli "$DB" <"$SCENARIO_OUT/queries.txt" \
    >"$SCENARIO_OUT/query.out" 2>"$SCENARIO_OUT/query.err" \
    || stress_fail "real CLI query battery exited nonzero"
stress_assert_quiet "$SCENARIO_OUT/query.err" "volume query battery"
if ! diff "$SCENARIO_OUT/expected-query.out" "$SCENARIO_OUT/query.out" \
    >"$SCENARIO_OUT/query.diff" 2>&1; then
    sed -n '1,160p' "$SCENARIO_OUT/query.diff" >&2
    stress_fail "query battery differs from generator expectations"
fi

DB_BYTES=$(wc -c <"$DB" | tr -d ' ')
stress_check_disk_budget
stress_pass "$LOGICAL_BYTES logical bytes; $NODE_ROWS nodes + $REL_ROWS rels; database=$DB_BYTES bytes; exact all-type/query battery"

