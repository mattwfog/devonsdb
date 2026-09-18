#!/bin/sh
# Shared, POSIX-sh-safe machinery for the local stress scenarios.

STRESS_DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$STRESS_DIR/../.." && pwd)
OUT=${STRESS_OUT:-"$STRESS_DIR/out"}
DEVONDB_BIN=${DEVONDB_BIN:-"$ROOT/target/debug/devondb"}
STRESS_GEN_BIN=${STRESS_GEN_BIN:-"$STRESS_DIR/gen/target/debug/stress-gen"}
STRESS_MEMORY_BYTES=${STRESS_MEMORY_BYTES:-536870912}
STRESS_MIN_FREE_KB=15728640
STRESS_RESULT_PRINTED=0
STRESS_CHILD_PIDS=

stress_cleanup() {
    for stress_pid in $STRESS_CHILD_PIDS; do
        if kill -0 "$stress_pid" 2>/dev/null; then
            kill "$stress_pid" 2>/dev/null || true
            wait "$stress_pid" 2>/dev/null || true
        fi
    done
}

stress_exit_trap() {
    stress_status=$?
    stress_cleanup
    if [ "$stress_status" -ne 0 ] && [ "$STRESS_RESULT_PRINTED" -eq 0 ]; then
        STRESS_RESULT_PRINTED=1
        echo "$SCENARIO: FAIL — unexpected exit $stress_status"
    fi
}

stress_signal() {
    stress_signal_status=$1
    trap - EXIT
    stress_cleanup
    if [ "$STRESS_RESULT_PRINTED" -eq 0 ]; then
        STRESS_RESULT_PRINTED=1
        echo "$SCENARIO: FAIL — interrupted"
    fi
    exit "$stress_signal_status"
}

stress_install_traps() {
    trap stress_exit_trap EXIT
    trap 'stress_signal 129' HUP
    trap 'stress_signal 130' INT
    trap 'stress_signal 143' TERM
}

stress_fail() {
    STRESS_RESULT_PRINTED=1
    echo "$SCENARIO: FAIL — $1"
    exit 1
}

stress_pass() {
    STRESS_RESULT_PRINTED=1
    echo "$SCENARIO: PASS — $1"
}

stress_park() {
    STRESS_RESULT_PRINTED=1
    echo "$SCENARIO: PARK — $1"
    exit 77
}

stress_note() {
    echo "$SCENARIO: $1"
}

stress_require_uint() {
    stress_value=$1
    stress_name=$2
    case "$stress_value" in
        ''|*[!0-9]*) stress_fail "$stress_name must be a positive integer, got '$stress_value'" ;;
        0) stress_fail "$stress_name must be greater than zero" ;;
    esac
}

stress_check_free_space() {
    stress_free_kb=$(df -Pk "$STRESS_DIR" | awk 'NR == 2 { print $4 }')
    case "$stress_free_kb" in
        ''|*[!0-9]*) stress_fail "could not determine free space for $STRESS_DIR" ;;
    esac
    if [ "$stress_free_kb" -lt "$STRESS_MIN_FREE_KB" ]; then
        stress_fail "refusing below 15 GiB free ($stress_free_kb KiB available)"
    fi
}

stress_now() {
    date +%s
}

stress_remaining_seconds() {
    stress_current=$(stress_now)
    stress_remaining=$((STRESS_DEADLINE_EPOCH - stress_current))
    if [ "$stress_remaining" -le 0 ]; then
        return 1
    fi
    echo "$stress_remaining"
}

stress_check_deadline() {
    if ! stress_remaining_seconds >/dev/null; then
        stress_fail "STRESS_MINUTES deadline reached"
    fi
}

# Execute one command under the suite-wide wall-clock deadline. SIGALRM is
# inherited across exec, so even one long COPY cannot run past the cap.
stress_run_deadlined() {
    stress_seconds=$(stress_remaining_seconds) || return 124
    perl -e 'alarm shift; exec @ARGV or die "exec $ARGV[0]: $!\n"' \
        "$stress_seconds" "$@"
}

stress_out_bytes() {
    stress_kb=$(du -sk "$OUT" 2>/dev/null | awk '{ print $1 }')
    stress_kb=${stress_kb:-0}
    echo $((stress_kb * 1024))
}

stress_check_disk_budget() {
    stress_used=$(stress_out_bytes)
    if [ "$stress_used" -gt "$STRESS_MAX_BYTES" ]; then
        stress_fail "artifact budget exceeded: $stress_used > $STRESS_MAX_BYTES bytes"
    fi
}

stress_init() {
    SCENARIO=$1
    TIER=${2:-smoke}
    case "$TIER" in
        smoke|standard|torture) ;;
        *) stress_fail "unknown tier '$TIER'" ;;
    esac
    : "${STRESS_DEADLINE_EPOCH:=$(( $(stress_now) + 180 ))}"
    : "${STRESS_MAX_BYTES:=1073741824}"
    SCENARIO_OUT="$OUT/$SCENARIO"
    stress_install_traps
    stress_check_free_space
    stress_check_deadline
    [ -x "$DEVONDB_BIN" ] || stress_fail "real devondb binary is missing: $DEVONDB_BIN"
    [ -x "$STRESS_GEN_BIN" ] || stress_fail "generator is missing: $STRESS_GEN_BIN"
    rm -rf -- "$SCENARIO_OUT"
    mkdir -p "$SCENARIO_OUT"
}

stress_manifest_get() {
    stress_key=$1
    stress_manifest=$2
    awk -F= -v key="$stress_key" '$1 == key { print substr($0, length(key) + 2); found=1 } END { if (!found) exit 1 }' "$stress_manifest"
}

stress_assert_quiet() {
    stress_file=$1
    stress_what=$2
    if [ -s "$stress_file" ]; then
        sed -n '1,80p' "$stress_file" >&2
        stress_fail "unexpected stderr during $stress_what (see $stress_file)"
    fi
}

stress_assert_no_error_lines() {
    stress_file=$1
    stress_what=$2
    if grep -q '^error:' "$stress_file"; then
        sed -n '1,80p' "$stress_file" >&2
        stress_fail "error line during $stress_what (see $stress_file)"
    fi
}

stress_cli() {
    stress_db=$1
    shift
    # No database file may consume more than 3/8 of the whole artifact cap.
    # Volume reserves only 1/4 for logical payload; the extra eighth admits
    # format overhead while preventing a checkpoint leak from consuming the
    # remaining scenario evidence before the next shell-side budget poll.
    stress_file_blocks=$((STRESS_MAX_BYTES * 3 / 8 / 512))
    [ "$stress_file_blocks" -gt 0 ] || return 125
    stress_run_deadlined sh -c \
        'ulimit -f "$1" || exit 125; shift; exec "$@"' \
        stress-file-limit "$stress_file_blocks" "$DEVONDB_BIN" "$stress_db" \
        --memory-limit "$STRESS_MEMORY_BYTES" "$@"
}
