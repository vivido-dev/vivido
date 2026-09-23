#!/usr/bin/env bash

# Capture one comparable memory snapshot of a running Vivido process.
#
# Every number in ram-optimization-plan.md comes from this script, so that a
# before/after pair is always measured the same way.
#
#   ./mem-snapshot.sh                      # newest vivido process, report to stdout
#   ./mem-snapshot.sh -p 1234              # a specific pid
#   ./mem-snapshot.sh -l baseline-4tabs    # label the report and write it to a file
#
# macOS only: footprint(1), vmmap(1) and heap(1) have no portable equivalent.

set -u

label=""
pid=""
outdir=""

usage() {
    echo "usage: ${0##*/} [-p pid] [-l label] [-o outdir]" >&2
    exit 2
}

while getopts "p:l:o:h" opt; do
    case "$opt" in
        p) pid=$OPTARG ;;
        l) label=$OPTARG ;;
        o) outdir=$OPTARG ;;
        *) usage ;;
    esac
done

if [ "$(uname -s)" != "Darwin" ]; then
    echo "mem-snapshot.sh needs macOS (footprint/vmmap/heap)." >&2
    exit 1
fi

# Newest matching process, so that relaunching under MallocStackLogging does not
# pick up the previous instance. pgrep does not match an .app bundle's vivido by
# name, so match the executable's basename out of ps instead.
if [ -z "$pid" ]; then
    pid=$(ps -eo pid=,lstart=,comm= \
        | awk '{ exe = $NF; sub(/.*\//, "", exe); if (exe == "vivido") print $1 }' \
        | tail -1)
fi

if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
    echo "No running vivido process; pass -p <pid>." >&2
    exit 1
fi

report() {
    echo "# vivido memory snapshot"
    echo "date:    $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    echo "pid:     $pid"
    echo "label:   ${label:-none}"
    echo "binary:  $(ps -o comm= -p "$pid")"
    echo "uptime:  $(ps -o etime= -p "$pid" | tr -d ' ')"
    echo "panes:   $(ps -eo pid=,ppid= | awk -v p="$pid" '$2 == p' | grep -c . || true) direct children"
    echo

    echo "## footprint"
    footprint "$pid" 2>/dev/null | sed -n '/Dirty/,/TOTAL/p'
    echo

    echo "## phys_footprint"
    footprint "$pid" 2>/dev/null | grep -E 'phys_footprint'
    echo

    echo "## vmmap summary"
    vmmap -summary "$pid" 2>/dev/null | sed -n '/REGION TYPE/,/^TOTAL/p'
    echo

    echo "## malloc zones"
    vmmap -summary "$pid" 2>/dev/null | sed -n '/MALLOC ZONE/,$p'
    echo

    # The GPU half of the footprint lands in unmapped "graphics" regions, one per
    # allocation. Their sizes identify individual vello buffers (48M lines,
    # 48M segments, 32M ptcl, 16M tiles, 16M seg_counts) and the render target.
    echo "## graphics regions by resident size"
    vmmap "$pid" 2>/dev/null \
        | grep 'owned unmapped (graphics)' \
        | grep -E '\[ *[0-9.]+[KMG]?' \
        | sed -E 's/.*\[ *[0-9.]+[KMG]? +([0-9.]+[KMG]?) .*/\1/' \
        | grep -E '^[0-9.]+[KMG]?$' \
        | sort | uniq -c | sort -rn
    echo

    echo "## IOSurface regions"
    vmmap "$pid" 2>/dev/null | grep -E '^IOSurface .*[0-9]x[0-9]'
    echo

    # The size histogram is the actionable part: a spike at one size class points
    # straight at a single structure. Counts are live allocations, not totals.
    echo "## heap size histogram"
    heap "$pid" 2>/dev/null | grep -E '^All zones: [0-9]+ nodes malloced' | tr ' ' '\n' | grep -E '^[0-9.]+(KB|MB)?\[[0-9]+\]$'
    echo

    echo "## heap totals"
    heap "$pid" 2>/dev/null | grep -E '^All zones: [0-9]+ nodes \('
}

if [ -n "$outdir" ]; then
    mkdir -p "$outdir"
    stamp=$(date -u '+%Y%m%dT%H%M%SZ')
    out="$outdir/${stamp}${label:+-$label}.txt"
    report > "$out"
    echo "$out"
else
    report
fi
