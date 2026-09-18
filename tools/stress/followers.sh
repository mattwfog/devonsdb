#!/bin/sh
# One persistent CLI writer plus persistent MCP/read-only-shell followers.
# FOLLOWER_READERS and FOLLOWER_BATCHES scale the fanout and commit count;
# each commit contains five rows so a visible non-multiple proves a torn batch.
set -eu
. "$(dirname "$0")/common.sh"
stress_init followers "${1:-smoke}"

SEED=${FOLLOWER_SEED:-275005}
case "$TIER" in
    smoke)
        DEFAULT_READERS=4
        DEFAULT_BATCHES=6
        ;;
    standard)
        DEFAULT_READERS=8
        DEFAULT_BATCHES=40
        ;;
    torture)
        DEFAULT_READERS=16
        DEFAULT_BATCHES=200
        ;;
esac
READERS=${FOLLOWER_READERS:-$DEFAULT_READERS}
BATCHES=${FOLLOWER_BATCHES:-$DEFAULT_BATCHES}
BATCH_SIZE=5
TOTAL_BATCHES=$((BATCHES + 1))

require_positive() {
    case "$1" in
        ''|*[!0-9]*|0) stress_fail "$2 must be a positive integer, got '$1'" ;;
    esac
}

wait_for_marker() {
    MARKER=$1
    while [ ! -f "$MARKER" ]; do
        sleep 0.02
    done
}

wait_for_matches() {
    MATCH_FILE=$1
    MATCH_PATTERN=$2
    MATCH_EXPECTED=$3
    MATCH_LABEL=$4
    MATCH_ATTEMPT=0
    while [ "$MATCH_ATTEMPT" -lt 1000 ]; do
        MATCH_COUNT=$(grep -c "$MATCH_PATTERN" "$MATCH_FILE" 2>/dev/null || true)
        if [ "$MATCH_COUNT" -ge "$MATCH_EXPECTED" ]; then
            return 0
        fi
        MATCH_ATTEMPT=$((MATCH_ATTEMPT + 1))
        sleep 0.02
    done
    stress_fail "timed out waiting for $MATCH_LABEL"
}

extract_reader_counts() {
    READER_NUMBER=$1
    READER_ROLE=$(sed -n '1p' "$SCENARIO_OUT/reader-$READER_NUMBER.role")
    if [ "$READER_ROLE" = shell ]; then
        sed -n '/^[0-9][0-9]*$/p' "$SCENARIO_OUT/reader-$READER_NUMBER.out"
    else
        sed -n 's/.*total:Int64\\n\([0-9][0-9]*\)\\n(1 rows).*/\1/p' \
            "$SCENARIO_OUT/reader-$READER_NUMBER.out"
    fi
}

wait_for_reader_round() {
    READER_NUMBER=$1
    READER_EXPECTED=$2
    READER_ATTEMPT=0
    while [ "$READER_ATTEMPT" -lt 1000 ]; do
        READER_OBSERVED=$(extract_reader_counts "$READER_NUMBER" | wc -l | tr -d ' ')
        if [ "$READER_OBSERVED" -ge "$READER_EXPECTED" ]; then
            return 0
        fi
        READER_PID=$(sed -n '1p' "$SCENARIO_OUT/reader-$READER_NUMBER.pid")
        if ! kill -0 "$READER_PID" 2>/dev/null; then
            stress_fail "reader $READER_NUMBER exited before observation $READER_EXPECTED"
        fi
        READER_ATTEMPT=$((READER_ATTEMPT + 1))
        sleep 0.02
    done
    stress_fail "timed out waiting for reader $READER_NUMBER observation $READER_EXPECTED"
}

writer_feed() {
    exec 3>"$SCENARIO_OUT/writer.in"
    printf '%s\n' 'nodes(Item) as item | aggregate count(item.id) as total' >&3
    WRITER_BATCH=0
    while IFS= read -r WRITER_STATEMENT; do
        WRITER_BATCH=$((WRITER_BATCH + 1))
        wait_for_marker "$SCENARIO_OUT/writer-$WRITER_BATCH.go"
        printf '%s\n' "$WRITER_STATEMENT" >&3
    done <"$SCENARIO_OUT/writer-batches.txt"
    wait_for_marker "$SCENARIO_OUT/writer-exit.go"
    printf '%s\n' '.exit' >&3
}

reader_feed() {
    READER_NUMBER=$1
    READER_ROLE=$2
    exec 3>"$SCENARIO_OUT/reader-$READER_NUMBER.in"
    if [ "$READER_ROLE" = mcp ]; then
        printf '%s\n' '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"stress","version":"0"}}}' >&3
    fi
    READER_ROUND=0
    while [ "$READER_ROUND" -le "$TOTAL_BATCHES" ]; do
        wait_for_marker "$SCENARIO_OUT/read-$READER_ROUND.go"
        if [ "$READER_ROLE" = shell ]; then
            printf '%s\n' 'nodes(Item) as item | aggregate count(item.id) as total' >&3
        else
            READER_ID=$((READER_ROUND + 1))
            printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$READER_ID,\"method\":\"tools/call\",\"params\":{\"name\":\"query\",\"arguments\":{\"text\":\"nodes(Item) as item | aggregate count(item.id) as total\"}}}" >&3
        fi
        READER_ROUND=$((READER_ROUND + 1))
    done
    if [ "$READER_ROLE" = shell ]; then
        printf '%s\n' '.exit' >&3
    fi
}

require_positive "$SEED" FOLLOWER_SEED
require_positive "$READERS" FOLLOWER_READERS
require_positive "$BATCHES" FOLLOWER_BATCHES
[ "$READERS" -ge 2 ] || stress_fail "FOLLOWER_READERS must be at least 2 (one MCP and one shell follower)"
stress_note "seed=$SEED readers=$READERS batches=$BATCHES batch_size=$BATCH_SIZE"

DB="$SCENARIO_OUT/followers.devondb"
cat >"$SCENARIO_OUT/seed.txt" <<'EOF'
create node table Item (id Int64 primary key, batch Int64)
.checkpoint
.exit
EOF
stress_cli "$DB" <"$SCENARIO_OUT/seed.txt" \
    >"$SCENARIO_OUT/seed.out" 2>"$SCENARIO_OUT/seed.err" \
    || stress_fail "seed writer exited nonzero"
[ ! -s "$SCENARIO_OUT/seed.err" ] || stress_fail "seed writer emitted stderr"

stress_cli activate "$DB" </dev/null \
    >"$SCENARIO_OUT/activate.out" 2>"$SCENARIO_OUT/activate.err" \
    || stress_fail "offline activation exited nonzero"
[ ! -s "$SCENARIO_OUT/activate.err" ] || stress_fail "offline activation emitted stderr"
grep -q '^activated ' "$SCENARIO_OUT/activate.out" \
    || stress_fail "offline activation did not report activation"

awk -v batches="$BATCHES" -v size="$BATCH_SIZE" -v seed="$SEED" 'BEGIN {
    for (batch = 1; batch <= batches; batch++) {
        printf "insert into Item values "
        for (row = 1; row <= size; row++) {
            if (row > 1) printf ", "
            id = seed + (batch - 1) * size + row
            printf "(%d, %d)", id, batch
        }
        printf "\n"
    }
}' >"$SCENARIO_OUT/writer-batches.txt"

mkfifo "$SCENARIO_OUT/writer.in"
writer_feed &
WRITER_FEED_PID=$!
stress_cli "$DB" <"$SCENARIO_OUT/writer.in" \
    >"$SCENARIO_OUT/writer.out" 2>"$SCENARIO_OUT/writer.err" &
WRITER_PID=$!
STRESS_CHILD_PIDS="$STRESS_CHILD_PIDS $WRITER_FEED_PID $WRITER_PID"

# The aggregate result is the holder's readiness signal: it can only be
# printed after the REPL has opened the target and acquired its writer lease.
wait_for_matches "$SCENARIO_OUT/writer.out" '^0$' 1 "writer readiness"

if stress_cli "$DB" </dev/null \
    >"$SCENARIO_OUT/busy-probe.out" 2>"$SCENARIO_OUT/busy-probe.err"; then
    stress_fail "second writer opened while the observed holder was live"
fi
grep -q '^error: busy:' "$SCENARIO_OUT/busy-probe.err" \
    || stress_fail "second writer refusal did not preserve the engine busy error"

READER_PIDS=
READER_FEED_PIDS=
READER_NUMBER=1
while [ "$READER_NUMBER" -le "$READERS" ]; do
    if [ $((READER_NUMBER % 2)) -eq 0 ]; then
        READER_ROLE=mcp
    else
        READER_ROLE=shell
    fi
    printf '%s\n' "$READER_ROLE" >"$SCENARIO_OUT/reader-$READER_NUMBER.role"
    mkfifo "$SCENARIO_OUT/reader-$READER_NUMBER.in"
    reader_feed "$READER_NUMBER" "$READER_ROLE" &
    READER_FEED_PID=$!
    if [ "$READER_ROLE" = shell ]; then
        stress_cli "$DB" --read-only <"$SCENARIO_OUT/reader-$READER_NUMBER.in" \
            >"$SCENARIO_OUT/reader-$READER_NUMBER.out" \
            2>"$SCENARIO_OUT/reader-$READER_NUMBER.err" &
    else
        stress_cli mcp "$DB" <"$SCENARIO_OUT/reader-$READER_NUMBER.in" \
            >"$SCENARIO_OUT/reader-$READER_NUMBER.out" \
            2>"$SCENARIO_OUT/reader-$READER_NUMBER.err" &
    fi
    READER_PID=$!
    printf '%s\n' "$READER_PID" >"$SCENARIO_OUT/reader-$READER_NUMBER.pid"
    READER_FEED_PIDS="$READER_FEED_PIDS $READER_FEED_PID"
    READER_PIDS="$READER_PIDS $READER_PID"
    STRESS_CHILD_PIDS="$STRESS_CHILD_PIDS $READER_FEED_PID $READER_PID"
    READER_NUMBER=$((READER_NUMBER + 1))
done

: >"$SCENARIO_OUT/read-0.go"
READER_NUMBER=1
while [ "$READER_NUMBER" -le "$READERS" ]; do
    wait_for_reader_round "$READER_NUMBER" 1
    READER_NUMBER=$((READER_NUMBER + 1))
done

WRITER_BATCH=1
while [ "$WRITER_BATCH" -le "$BATCHES" ]; do
    : >"$SCENARIO_OUT/writer-$WRITER_BATCH.go"
    wait_for_matches "$SCENARIO_OUT/writer.out" '^ok$' "$WRITER_BATCH" \
        "writer batch $WRITER_BATCH acknowledgement"
    : >"$SCENARIO_OUT/read-$WRITER_BATCH.go"
    READER_NUMBER=1
    while [ "$READER_NUMBER" -le "$READERS" ]; do
        wait_for_reader_round "$READER_NUMBER" $((WRITER_BATCH + 1))
        READER_NUMBER=$((READER_NUMBER + 1))
    done
    WRITER_BATCH=$((WRITER_BATCH + 1))
done

: >"$SCENARIO_OUT/writer-exit.go"
wait "$WRITER_FEED_PID" || stress_fail "writer input feeder failed"
wait "$WRITER_PID" || stress_fail "first writer exited nonzero"
[ ! -s "$SCENARIO_OUT/writer.err" ] || stress_fail "first writer emitted stderr"
STRESS_CHILD_PIDS="$READER_FEED_PIDS $READER_PIDS"

TAKEOVER_BATCH=$TOTAL_BATCHES
TAKEOVER_START=$((SEED + BATCHES * BATCH_SIZE + 1))
{
    printf 'insert into Item values '
    TAKEOVER_ROW=0
    while [ "$TAKEOVER_ROW" -lt "$BATCH_SIZE" ]; do
        [ "$TAKEOVER_ROW" -eq 0 ] || printf ', '
        printf '(%s, %s)' $((TAKEOVER_START + TAKEOVER_ROW)) "$TAKEOVER_BATCH"
        TAKEOVER_ROW=$((TAKEOVER_ROW + 1))
    done
    printf '\n.exit\n'
} >"$SCENARIO_OUT/takeover.txt"
stress_cli "$DB" <"$SCENARIO_OUT/takeover.txt" \
    >"$SCENARIO_OUT/takeover.out" 2>"$SCENARIO_OUT/takeover.err" \
    || stress_fail "successor writer could not take over"
[ ! -s "$SCENARIO_OUT/takeover.err" ] || stress_fail "successor writer emitted stderr"
[ "$(grep -c '^ok$' "$SCENARIO_OUT/takeover.out" || true)" -eq 1 ] \
    || stress_fail "successor writer did not acknowledge its commit"

: >"$SCENARIO_OUT/read-$TOTAL_BATCHES.go"
EXPECTED_OBSERVATIONS=$((TOTAL_BATCHES + 1))
READER_NUMBER=1
while [ "$READER_NUMBER" -le "$READERS" ]; do
    wait_for_reader_round "$READER_NUMBER" "$EXPECTED_OBSERVATIONS"
    READER_NUMBER=$((READER_NUMBER + 1))
done

for READER_FEED_PID in $READER_FEED_PIDS; do
    wait "$READER_FEED_PID" || stress_fail "reader input feeder failed"
done
for READER_PID in $READER_PIDS; do
    wait "$READER_PID" || stress_fail "reader process exited nonzero"
done
STRESS_CHILD_PIDS=

FINAL_ROWS=$((TOTAL_BATCHES * BATCH_SIZE))
READER_NUMBER=1
while [ "$READER_NUMBER" -le "$READERS" ]; do
    [ ! -s "$SCENARIO_OUT/reader-$READER_NUMBER.err" ] \
        || stress_fail "reader $READER_NUMBER emitted stderr"
    extract_reader_counts "$READER_NUMBER" >"$SCENARIO_OUT/reader-$READER_NUMBER.counts"
    if ! awk -v observations="$EXPECTED_OBSERVATIONS" -v size="$BATCH_SIZE" \
        -v final="$FINAL_ROWS" '
        BEGIN { previous = 0 }
        {
            maximum = (NR - 1) * size
            if ($1 < previous || $1 % size != 0 || $1 > maximum) exit 1
            previous = $1
        }
        END {
            if (NR != observations || previous != final) exit 1
        }
    ' "$SCENARIO_OUT/reader-$READER_NUMBER.counts"; then
        stress_fail "reader $READER_NUMBER did not observe monotone committed batches ending at $FINAL_ROWS rows"
    fi
    READER_NUMBER=$((READER_NUMBER + 1))
done

PASS_DETAIL="$READERS persistent MCP/shell followers saw $TOTAL_BATCHES atomic batches; busy refusal and writer takeover proved"
stress_pass "$PASS_DETAIL"
{
    printf 'followers: seed=%s readers=%s batches=%s batch_size=%s\n' \
        "$SEED" "$READERS" "$BATCHES" "$BATCH_SIZE"
    printf 'followers: PASS — %s\n' "$PASS_DETAIL"
} >"$OUT/followers.log"
