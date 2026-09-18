#!/bin/sh
set -u

root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
manifest="$root/Cargo.toml"
binary="$root/target/debug/devondb-fuzz"
mode=${1:-}

usage() {
    echo "usage: $0 <smoke|standard|torture|regressions>" >&2
    exit 2
}

case "$mode" in
    smoke|standard|torture|regressions) ;;
    *) usage ;;
esac

cargo build --quiet --locked --manifest-path "$manifest" || exit $?

if [ "$mode" = regressions ]; then
    exec "$binary" regressions
fi

"$binary" regressions || exit $?

status=0
case "$mode" in
    smoke)
        for target in statements plan_ir container nl; do
            iterations=8
            if [ "$target" = nl ]; then
                iterations=4
            fi
            "$binary" "$target" --iterations "$iterations" --intensity 4 || status=1
        done
        ;;
    standard|torture)
        if [ -n "${FUZZ_MINUTES:-}" ]; then
            minutes=$FUZZ_MINUTES
        elif [ "$mode" = standard ]; then
            minutes=10
        else
            minutes=60
        fi
        case "$minutes" in
            ''|*[!0-9]*)
                echo "FUZZ_MINUTES must be a positive integer" >&2
                exit 2
                ;;
        esac
        if [ "$minutes" -le 0 ]; then
            echo "FUZZ_MINUTES must be a positive integer" >&2
            exit 2
        fi
        seconds=$((minutes * 60))
        if [ "$mode" = standard ]; then
            intensity=8
        else
            intensity=24
        fi
        for target in statements plan_ir container nl; do
            "$binary" "$target" --seconds "$seconds" --intensity "$intensity" || status=1
        done
        ;;
esac

exit "$status"
