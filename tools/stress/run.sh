#!/bin/sh
# run.sh <smoke|standard|torture>
#
# Suite caps:
#   STRESS_MINUTES  wall-clock cap (defaults: 3 / 20 / 180)
#   STRESS_GB       total tools/stress/out artifact cap (1 / 5 / 12 GiB)
# Both may be lowered. The volume target is reduced before generation so its
# Parquet source + database + headroom fit under STRESS_GB.
set -eu

STRESS_DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$STRESS_DIR/../.." && pwd)
OUT="$STRESS_DIR/out"
TIER=${1:-}

case "$TIER" in
    smoke)
        DEFAULT_MINUTES=3
        DEFAULT_GB=1
        ;;
    standard)
        DEFAULT_MINUTES=20
        DEFAULT_GB=5
        ;;
    torture)
        DEFAULT_MINUTES=180
        DEFAULT_GB=12
        ;;
    *)
        echo "usage: sh tools/stress/run.sh <smoke|standard|torture>" >&2
        exit 2
        ;;
esac

STRESS_MINUTES=${STRESS_MINUTES:-$DEFAULT_MINUTES}
STRESS_GB=${STRESS_GB:-$DEFAULT_GB}
case "$STRESS_MINUTES" in
    ''|*[!0-9]*|0) echo "stress: STRESS_MINUTES must be a positive integer" >&2; exit 2 ;;
esac
if ! awk -v value="$STRESS_GB" 'BEGIN { exit !(value ~ /^[0-9]+([.][0-9]+)?$/ && value > 0) }'; then
    echo "stress: STRESS_GB must be a positive number" >&2
    exit 2
fi

START_EPOCH=$(date +%s)
STRESS_DEADLINE_EPOCH=$((START_EPOCH + STRESS_MINUTES * 60))
STRESS_MAX_BYTES=$(awk -v gb="$STRESS_GB" 'BEGIN { printf "%.0f\n", gb * 1073741824 }')
export STRESS_DEADLINE_EPOCH STRESS_MAX_BYTES STRESS_OUT="$OUT"

run_capped_in_dir() {
    RUN_NOW=$(date +%s)
    RUN_SECONDS=$((STRESS_DEADLINE_EPOCH - RUN_NOW))
    [ "$RUN_SECONDS" -gt 0 ] || return 124
    RUN_DIRECTORY=$1
    shift
    perl -e '
        $seconds = shift;
        $directory = shift;
        chdir $directory or die "chdir $directory: $!\n";
        alarm $seconds;
        exec @ARGV or die "exec $ARGV[0]: $!\n";
    ' "$RUN_SECONDS" "$RUN_DIRECTORY" "$@"
}

rm -rf -- "$OUT"
mkdir -p "$OUT"

if [ "${STRESS_SKIP_BUILD:-0}" != 1 ]; then
    STRESS_PROFILE=${STRESS_PROFILE:-debug}
    case "$STRESS_PROFILE" in
        debug)
            BUILD_ARGS="--locked -p devondb-cli"
            : "${DEVONDB_BIN:=$ROOT/target/debug/devondb}"
            ;;
        release)
            BUILD_ARGS="--locked --release -p devondb-cli"
            : "${DEVONDB_BIN:=$ROOT/target/release/devondb}"
            ;;
        *)
            echo "stress: STRESS_PROFILE must be debug or release" >&2
            exit 2
            ;;
    esac
    BUILD_ATTEMPT=1
    while ! run_capped_in_dir "$ROOT" cargo build $BUILD_ARGS >"$OUT/devondb-build.log" 2>&1; do
        if [ "$BUILD_ATTEMPT" -ge 5 ] || [ "$(date +%s)" -ge "$STRESS_DEADLINE_EPOCH" ]; then
            tail -40 "$OUT/devondb-build.log" >&2
            echo "stress: FAIL — devondb $STRESS_PROFILE build after $BUILD_ATTEMPT attempts" >&2
            exit 1
        fi
        echo "stress: build attempt $BUILD_ATTEMPT saw a concurrent checkout failure; retrying"
        sleep 3
        BUILD_ATTEMPT=$((BUILD_ATTEMPT + 1))
    done
else
    : "${DEVONDB_BIN:=$ROOT/target/${STRESS_PROFILE:-debug}/devondb}"
fi

if ! run_capped_in_dir "$STRESS_DIR/gen" cargo build --locked >"$OUT/generator-build.log" 2>&1; then
    tail -40 "$OUT/generator-build.log" >&2
    echo "stress: FAIL — stress generator build" >&2
    exit 1
fi
STRESS_GEN_BIN="$STRESS_DIR/gen/target/debug/stress-gen"
export DEVONDB_BIN STRESS_GEN_BIN

echo "stress: tier=$TIER minutes_cap=$STRESS_MINUTES gb_cap=$STRESS_GB"
echo "stress: binary=$DEVONDB_BIN ($("$DEVONDB_BIN" --version))"

SCOREBOARD="$OUT/scoreboard.txt"
: >"$SCOREBOARD"
SUITE_STATUS=0
for SCENARIO in volume churn reopen-storm kill9 followers; do
    LOG="$OUT/$SCENARIO.log"
    if sh "$STRESS_DIR/$SCENARIO.sh" "$TIER" >"$LOG" 2>&1; then
        RESULT=PASS
    else
        RESULT_CODE=$?
        if [ "$RESULT_CODE" -eq 77 ]; then
            RESULT=PARK
        else
            RESULT=FAIL
            SUITE_STATUS=1
        fi
    fi
    sed -n '1,200p' "$LOG"
    printf '%-14s %s\n' "$SCENARIO" "$RESULT" >>"$SCOREBOARD"
done

echo "stress: final scoreboard"
sed 's/^/stress:   /' "$SCOREBOARD"
END_EPOCH=$(date +%s)
ELAPSED=$((END_EPOCH - START_EPOCH))
USED_KB=$(du -sk "$OUT" | awk '{ print $1 }')
USED=$((USED_KB * 1024))
echo "stress: elapsed=${ELAPSED}s artifacts=${USED}B cap=${STRESS_MAX_BYTES}B"
if [ "$USED" -gt "$STRESS_MAX_BYTES" ]; then
    echo "stress: FAIL — artifact cap exceeded" >&2
    exit 1
fi
if [ "$END_EPOCH" -gt "$STRESS_DEADLINE_EPOCH" ]; then
    echo "stress: FAIL — wall-clock cap exceeded" >&2
    exit 1
fi
exit "$SUITE_STATUS"
