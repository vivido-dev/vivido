Scripts
=======

## Flamegraph

Run the release version of Vivido while recording call stacks. After the
Vivido process exits, a flamegraph will be generated and it's URI printed
as the only output to STDOUT.

```sh
./create-flamegraph.sh
```

Running this script depends on an installation of `perf`.

## ANSI Color Tests

We include a few scripts for testing the color of text inside a terminal. The
first shows various foreground and background variants. The second enumerates
all the colors of a standard terminal. The third enumerates the 24-bit colors.

```sh
./fg-bg.sh
./colors.sh
./24-bit-color.sh
```

## Throughput benchmarks

`ascii-throughput.sh` measures in-memory parse capacity on the payload of Kitty's
"Only ASCII chars" benchmark: the parser alone, and parser plus terminal grid at
120×40. It prints the median of several runs. `kitty-ascii-payload.py` writes
that payload; the shell script calls it for you.

```sh
./ascii-throughput.sh [SAMPLES] [ITERATIONS]
```

Build the binaries you compare with one codegen unit, each in its own target
directory, for example
`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_TARGET_DIR=target/bench-cgu1 cargo build --release --locked`.
With the default sixteen units, unrelated edits can move results by 15–20%.

`performance-e2e.py` runs a real Vivido process with a benchmark inside its PTY,
alternating two builds, and `performance-summary.py` turns the results into a
per-workload comparison. It needs Unix sockets, so run it outside any sandbox
that blocks them. With Kitty's own benchmark:

```sh
python3 performance-e2e.py --driver kitten --benchmark "$(command -v kitten)" \
  --before OLD_VIVIDO --after NEW_VIVIDO --output NEW_EMPTY_DIR \
  --samples 3 --repetitions 100 --modes sync render \
  --workloads ascii unicode csi long_escape_codes images
python3 performance-summary.py NEW_EMPTY_DIR/results.jsonl
```

See `../docs/benchmarks.md` for the corpus-replay driver and recorded results.
