# Terminal Benchmarks

## Terminal throughput comparison

<img width="1910" height="1074" alt="Screenshot 2026-09-22 at 10 52 07 PM" src="https://github.com/user-attachments/assets/0c451c1e-f8c9-49cc-94b9-378f824582b1" />

The measurement was recorded and available on [youtube](https://youtu.be/vswkj2AqZGc)

Measured with `kitten __benchmark__` [source](https://github.com/kovidgoyal/kitty/blob/master/tools/cmd/benchmark/main.go).

It works by dumping large amounts of data of different types into the tty device and measuring how fast the terminal parses and responds to it.

**Results** (in MB/s — higher is better)

| Terminal  | ASCII | Unicode | CSI codes | Long escape codes | Images  | Average |
|-----------|-------|---------|-----------|-------------------|---------|---------|
| Vivido    | 133.8 | 186.3   | 98.0      | **517.0**         | 394.8   | **265.98**  |
| Kitty     | 83.4  | 148.4   | 69.3      | 325.3             | 291.6   | 183.6   |
| Ghostty   | 93.2  | 133.8   | 49.5      | 96.6              | 73.6    | 89.34   |
| WezTerm   | 34.1  | 51.1    | 22.1      | 309.8             | 263.6   | 136.14  |
| iTerm2    | 19.2  | 23.9    | 2.2       | 76.5              | 31.4    | 30.64   |
| Tabby     | 19.0  | 20.9    | 11.4      | 25.3              | 24.7    | 20.26   |
| Orca      | 12.0  | 32.0    | 7.4       | 13.4              | 13.4    | 15.64   |
| Alacritty | **157.5** | **220.4** | **105.1** | 247.2 | **450.2** | 236.08  |


## Printable-run optimization measurements (2026-10-27)

On the local Apple Silicon host with Rust 1.98.1, five alternating samples of
`cargo test --release --lib benchmark_printable_runs -- --ignored --nocapture`
produced the following medians. Each sample applies twenty identical in-memory
payloads to a 120×40 alternate-screen grid. The scalar reference calls the
existing `Handler::input` for each character; the batch path calls `input_str`.

| Workload | Scalar MiB/s | Batched MiB/s | Ratio |
|---|---:|---:|---:|
| ASCII | 233.140 | 378.645 | 1.62× |
| Mixed Unicode | 354.634 | 381.505 | 1.08× |
| Three combining marks per base | 146.556 | 156.091 | 1.07× |

These are grid-mutation measurements, not application throughput or presentation
rates. They include scrolling and cell cleanup but exclude UTF-8 parsing, PTY
transport, GPU rendering, and input/query latency. Samples are short and have
visible timing variation; the test intentionally contains no timing assertions.
The scalar oracle and semantic differential tests remain available.

The implementation keeps printable spans through vvte's new borrowed callbacks,
shares guarded narrow/wide row fills with REP, and appends combining runs with
one copy-on-write check. CSI parameter spans avoid repeated state dispatch.
Unsupported OSC bodies can be discarded without buffering by the ANSI adapter;
generic parser consumers retain capture by default, and debug diagnostics retain
payload capture. No protocol crate or Vivid wire contract changed.

`benchmark/throughput` now supports seeded payload export and exact replay, with
separate original-size text corpora. `examples/terminal_stream.rs` measures the
ANSI parser or real terminal against an exported payload without a PTY or GPU.
Parser-mode image results measure APC handling only; terminal mode decodes images.

## PTY, snapshot, and allocation measurements (2026-10-27)

Measured on an Apple M4 with 32 GiB RAM, macOS arm64, Rust 1.98.1. These compare
release builds **after the earlier printable-run/parser stage**: the baseline has
those changes, and the candidate adds gathering, owned snapshots, bounded parsing
batches, and inline combining suffixes. They are not comparisons against an
unmodified release. Five alternating samples per build, 50 repetitions per
workload, identical replay bytes, 120×40 geometry, and no concurrent compilation
or other benchmarks. The production headless GPU renderer uses its existing
16 ms minimum draw interval; native desktop frame/pane tests are separate.

MacOS gathering is enabled by default; `VIVIDO_PTY_GATHER=0` disables it for
comparison. Four reusable 64 KiB buffers bound read-ahead. Small bursts publish
immediately. Saturated refill gaps receive at most 16 immediate retries; only a
busy parser permits 1 ms bridge polls, within a 3 ms bridging budget. Both workers
request user-initiated QoS. Reset, EOF, backpressure, and shutdown preserve ordering
and wake blocked workers. Linux/Windows keep their transport implementations.

The first candidate regressed original-size synchronized Unicode by 8.8%; an
isolated gather comparison reproduced a 10.2% loss. It was rejected. Matching the
reference's retry/poll ordering and worker QoS, plus inline suffix storage, removed
that regression. The final worst median throughput regression is below 3%. Every
workload passes the 5% throughput gate, and every query p99 passes the larger-of-5%
or-1-ms gate. The [raw samples, hashes, and baseline patches](performance/2026-10-27/)
include the rejected experiment rather than hiding it.

The Kitty workload transfers and deletes raw RGBA images; it exercises base64
decoding and image storage, not visible image placement, PNG decoding, or video.

Four suffix characters now fit inside `CellExtra` without a second allocation.
The `union` SmallVec representation preserves the previous vector field size on
64-bit targets; the cell itself stays at most 24 bytes. Longer suffixes spill to
heap storage. Copy-on-write and sequence serialization remain intact.

Values below are median MiB/s, **before → after**. Synchronized updates retain
their existing size/time limits. Original-size ASCII deliberately exceeds the
2 MiB buffer limit; no limit was raised to improve a result.

### Current payload sizes

| Workload | Synchronized | Live rendering | Sync + scrollback | Live + scrollback |
|---|---:|---:|---:|---:|
| ASCII | 131.2 → 131.3 | 142.5 → 192.4 | 123.0 → 124.7 | 122.5 → 158.1 |
| Unicode | 172.9 → 172.2 | 177.3 → 232.7 | 168.9 → 171.1 | 172.1 → 252.7 |
| Combining-heavy Unicode | 121.0 → 131.1 | 127.0 → 163.8 | 114.0 → 126.5 | 117.2 → 157.7 |
| CSI | 60.6 → 66.7 | 60.6 → 75.1 | 60.8 → 66.0 | 60.2 → 74.3 |
| Long OSC | 243.0 → 302.7 | 229.7 → 307.9 | 249.8 → 307.1 | 226.5 → 314.0 |
| Kitty images | 206.6 → 297.5 | 206.8 → 306.2 | 206.5 → 303.8 | 206.3 → 310.1 |

| Mode | CPU seconds before → after | Peak RSS MiB before → after | Loaded query p99 ms before → after |
|---|---:|---:|---:|
| Synchronized | 7.96 → 7.83 | 134.0 → 136.6 | 1.778 → 1.770 |
| Live rendering | 7.91 → 7.25 | 160.3 → 125.0 | 1.846 → 1.870 |
| Synchronized + scrollback | 7.90 → 7.80 | 259.7 → 249.0 | 1.482 → 1.543 |
| Live + scrollback | 7.99 → 7.35 | 309.9 → 260.8 | 1.797 → 1.652 |

[Raw samples](performance/2026-10-27/current/results.jsonl), [build/corpus hashes](performance/2026-10-27/current/metadata.json), [all acceptance checks](performance/2026-10-27/current/summary.json).

### Original Kitty text sizes

| Workload | Synchronized | Live rendering | Sync + scrollback | Live + scrollback |
|---|---:|---:|---:|---:|
| ASCII | 136.5 → 136.0 | 145.2 → 175.0 | 129.0 → 128.7 | 131.4 → 163.9 |
| Unicode | 176.5 → 172.2 | 177.8 → 246.2 | 171.1 → 169.9 | 171.9 → 252.5 |
| Combining-heavy Unicode | 121.2 → 130.7 | 125.5 → 163.0 | 115.8 → 127.0 | 117.6 → 164.4 |
| CSI | 61.7 → 66.3 | 61.8 → 77.3 | 61.0 → 65.9 | 61.2 → 75.0 |
| Long OSC | 246.2 → 308.3 | 225.4 → 308.0 | 243.8 → 305.9 | 226.6 → 308.7 |
| Kitty images | 205.3 → 308.0 | 203.5 → 303.1 | 204.5 → 302.6 | 202.5 → 306.0 |

| Mode | CPU seconds before → after | Peak RSS MiB before → after | Loaded query p99 ms before → after |
|---|---:|---:|---:|
| Synchronized | 9.26 → 9.16 | 134.2 → 140.9 | 1.927 → 1.652 |
| Live rendering | 9.20 → 8.37 | 170.6 → 140.2 | 1.817 → 1.724 |
| Synchronized + scrollback | 9.47 → 9.30 | 275.0 → 263.8 | 1.435 → 1.453 |
| Live + scrollback | 9.37 → 8.39 | 316.4 → 276.6 | 1.632 → 1.501 |

[Raw samples](performance/2026-10-27/original/results.jsonl), [build/corpus hashes](performance/2026-10-27/original/metadata.json), [all acceptance checks](performance/2026-10-27/original/summary.json).

CPU and RSS are process-launch measurements from `/usr/bin/time -l`, including
startup and benchmark work. Idle query p99 remained below 0.07 ms in both builds.
Each run contains 100 idle and 100 loaded queries; tables show the median of each
run's p99. A loaded query follows 106,496 bytes of text and includes output draining.
This is not physical keyboard latency or presentation latency. Native tests verify
continued visible frames, final dirty-frame delivery, and no hidden-pane presentation.

### Reproduce

Build executables before measuring, then export both corpora with the same Go
benchmark revision and seed. Always use a new output directory:

```sh
cd benchmark/throughput
go build -o /tmp/terminal-benchmark .
/tmp/terminal-benchmark --seed 1 --export /tmp/current-corpus
/tmp/terminal-benchmark --seed 1 --original-sizes --export /tmp/original-corpus
cd ../../vivido
python3 scripts/performance-e2e.py --before /path/to/baseline --after /path/to/candidate \
  --benchmark /tmp/terminal-benchmark --corpus /tmp/current-corpus \
  --output /tmp/results-current --samples 5 --repetitions 50 --latency-samples 100
python3 scripts/performance-summary.py /tmp/results-current/results.jsonl
```

Repeat with the original corpus and another output directory. The baseline patches
apply to the separate repository revisions recorded in `baseline.json`; apply them
only in disposable checkouts, with Vivido and vvte as siblings. No protocol or SDK
patch is required. The measured candidate hashes are recorded explicitly; subsequent
EOF ordering and registration cleanup fixes do not change its steady-state hot paths.

For diagnostic builds, use `cargo build --release --bin vivido --example terminal_stream
--features perf` and set `VIVIDO_PERF_FILE` to an output JSON path. The `perf` feature
is off by default. Counters include Rust allocation/reallocation calls, requested
allocation bytes, live/peak requested bytes, PTY/gather/parse batch histograms,
terminal wait/hold time, snapshot/draw time, delivered frames, and per-pane frame
intervals. Histograms use power-of-two upper bounds; frame intervals include idle
gaps. GPU and foreign-library allocations are not included in allocator counters.
Run diagnostics separately: atomic accounting changes timings.

### Allocation and lock diagnostics

Separate instrumented runs used the current-size corpus at 20 repetitions and
100 idle/loaded queries. Comparing the same new build with gathering off/on:

| Mode | Native read mean | Gathered batch mean | Parser lock acquisitions off → on | Parser hold p99 upper bound on | Snapshot hold p99 upper bound on | Delivered frames off → on |
|---|---:|---:|---:|---:|---:|---:|
| sync | 1023 B | 60.0 KiB | 167,372 → 5,939 | 4.194 ms | 0.066 ms | 113 → 99 |
| render | 1023 B | 60.7 KiB | 181,893 → 5,933 | 1.049 ms | 0.066 ms | 94 → 68 |

Gathering reduces lock acquisition frequency while doing more work per acquisition;
it does not promise shorter individual parser holds. Frame counts cover complete
runs of different durations, not a fixed-duration FPS comparison. All successful
presentations and per-pane intervals are recorded; idle gaps at workload boundaries
make interval p99 unsuitable as an interaction-latency metric.

With gathering disabled on both instrumented candidates, 50 repetitions of the
combining-heavy corpus used **14,859,400 → 7,481,155 allocations** (49.7% fewer)
and **961,302,138 → 841,581,430 requested allocation bytes** (12.5% fewer).
Peak requested live memory stayed approximately 20.9 MiB. These are whole-process
observations, including startup, queries, and rendering; they are not timing claims.
The inline-storage regression separately verifies four inline suffixes, arbitrary
spillover, shared-cell independence, and unchanged JSON sequence representation.

[Batch/lock/frame counters](performance/2026-10-27/diagnostics/results.jsonl) and
[suffix allocation comparison](performance/2026-10-27/suffix-allocations/results.jsonl)
include the numeric source data and matching build manifests.

### In-memory parser and terminal replay

Five alternating parser/terminal samples, twenty repetitions each, using the same
current-size exported payloads. Throughput uses normal release executables;
allocation figures come from a separate instrumented run of the example. They
include payload loading and terminal construction, which are outside the throughput
timer. Parser mode counts callbacks/APC bytes; terminal mode mutates the real grid
and decodes Kitty uploads. Neither includes PTY transport or GPU presentation.

| Workload | Parser MiB/s | Terminal MiB/s | Terminal allocations | Terminal peak requested MiB |
|---|---:|---:|---:|---:|
| Only ASCII chars | 844.9 | 250.2 | 99 | 1.22 |
| Unicode chars | 890.7 | 367.5 | 92,259 | 1.24 |
| Unique multi-codepoint Unicode cells | 961.4 | 229.0 | 2,949,219 | 1.57 |
| CSI codes with few chars | 362.7 | 90.9 | 99,419 | 1.23 |
| Long escape codes | 4083.9 | 3916.3 | 100 | 8.06 |
| Images | 38820.4 | 4421.6 | 142 | 9.56 |

[Raw replay samples and allocations](performance/2026-10-27/stream-results.json).
Run `cargo run --release --example terminal_stream -- parser PAYLOAD 20` (or
`terminal`) against any exported payload to repeat this measurement.

### Validation and remaining constraints

Vivido's workspace format, check, tests, and Clippy commands passed, with and
without `perf`: 1,062 library tests passed, including all 45 reference fixtures.
The 20 headless integration tests passed with gathering enabled by default,
including recovery, teardown, rendering, and two owners reusing local window ID 1.
Gather regressions cover bounded backpressure, partial-batch readiness, reset,
stop while full/idle, ordered tail draining, and natural EOF. Printable runs retain
the scalar differential oracle; suffix/snapshot tests cover copy-on-write reuse.

vvte passed default tests (41), ANSI/serde tests (69 plus logging integration),
formatting, and Clippy. vvte also passed ANSI/serde `cargo check` for
`x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`. The Go benchmark tests passed. Vvmux, Vivida, and vvbox passed
compilation, formatting, tests, and Clippy after their intentional lockfile updates.
Vvmux's existing `clippy::assert_is_empty` allowance emits an unknown-lint warning
on this toolchain; its required Clippy command exits successfully.

The final native macOS frame, pane, and background-pane tests all passed with
gathering enabled by default after the desktop was unlocked. These verify visible
frame delivery, hidden-pane behavior, and presentation after background panes are
revealed. An unlocked desktop is required; earlier attempts while macOS reported
a locked console could not present startup frames, including with gathering disabled.
Repeat on an unlocked desktop:

```sh
cargo test --test macos_background_panes --test macos_output_frames --test macos_panes -- --ignored
```

Linux and Windows native execution was not performed on this macOS host. Their
transport implementations are preserved; cross-platform native CI remains required.
`vivid_protocol` and `vivid_sdk` have no changes. No Vivid wire formats, authentication,
profiles, limits, ordering, history limits, or image work were relaxed.

Native macOS reads still average only about 1 KiB, so syscall/scheduling cost remains
a bottleneck. Large synchronized flushes and expansive CSI/REP operations can still
make a bounded-byte parser batch expensive. Snapshot capture is viewport-bounded;
font shaping, GPU work, active search/hints, and foreign-library allocations remain
outside the terminal parser optimizations. The measurements do not establish video,
physical-keyboard, or native Linux/Windows speedups.
