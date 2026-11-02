#!/bin/sh
# Measure in-memory parse capacity on the Kitty "Only ASCII chars" payload.
#
# Builds the release `terminal_stream` example, generates the payloads, and prints the median
# MiB/s of SAMPLES runs for the parser alone and for parser + Term (120x40 alternate screen,
# 64 KiB feeds). The parser + Term number is the in-memory parse capacity (P).
#
# Usage: scripts/ascii-throughput.sh [SAMPLES] [ITERATIONS]
set -eu

samples=${1:-5}
iterations=${2:-50}
cd "$(dirname "$0")/.."

cargo build --release --locked --example terminal_stream >&2
payloads=$(mktemp -d "${TMPDIR:-/tmp}/vivido-ascii.XXXXXX")
trap 'rm -r "$payloads"' EXIT
python3 -I scripts/kitty-ascii-payload.py "$payloads" >&2

median() {
    sort -n | awk '{ v[NR] = $1 } END { print (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2 }'
}

for payload in ascii ascii_sync nolf; do
    for mode in parser terminal; do
        rate=$(i=0; while [ "$i" -lt "$samples" ]; do
            target/release/examples/terminal_stream "$mode" "$payloads/$payload.bin" "$iterations" | cut -d, -f5
            i=$((i + 1))
        done | median)
        printf '%-10s %-8s %10s MiB/s\n' "$payload" "$mode" "$rate"
    done
done
