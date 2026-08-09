#!/bin/sh
set -eu

if [ "$#" -lt 3 ] || [ "$2" != "--" ]; then
    echo "usage: $0 SCENARIO -- COMMAND [ARG ...]" >&2
    echo "example: $0 headless -- cargo test --release --test headless" >&2
    exit 2
fi

scenario=$1
shift 2
runs=${VIVIDO_PERF_RUNS:-5}
warmup_seconds=${VIVIDO_PERF_WARMUP_SECONDS:-5}
output=${VIVIDO_PERF_OUTPUT:-target/stage3-performance/$scenario}
mkdir -p "$output"

echo "warming $scenario for at least ${warmup_seconds}s" >&2
warmup_end=$(( $(date +%s) + warmup_seconds ))
while :; do
    "$@" >/dev/null 2>&1
    [ "$(date +%s)" -ge "$warmup_end" ] && break
done

run=1
while [ "$run" -le "$runs" ]; do
    echo "running $scenario $run/$runs" >&2
    { time -p "$@" >"$output/run-$run.stdout" 2>"$output/run-$run.stderr"; } \
        2>"$output/run-$run.time"
    run=$((run + 1))
done

for metric in real user sys; do
    awk -v metric="$metric" '$1 == metric { print $2 }' "$output"/run-*.time \
        | sort -n \
        | awk -v metric="$metric" '
            { values[NR] = $1 }
            END {
                if (NR == 0) exit 1
                if (NR % 2) median = values[(NR + 1) / 2]
                else median = (values[NR / 2] + values[NR / 2 + 1]) / 2
                printf "%s_median_seconds\t%.6f\n", metric, median
            }
        '
done >"$output/medians.tsv"

echo "results: $output" >&2
sed 's/^/  /' "$output/medians.tsv" >&2
