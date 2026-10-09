# Rust verification

Run these commands from `vivido/`:

```sh
cargo fmt --all --check
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS='-D warnings -D missing_docs' cargo doc --no-deps --all-features --locked
cargo test --doc --all-features --locked
cargo run --example library_api --locked
cargo audit
cargo hack check --feature-powerset --all-targets --locked
cargo +nightly udeps --workspace --all-targets --all-features
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test --test row_construction
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test --lib terminal::grid::
```

Linux feature checks add `--at-least-one-of wayland`: the crate explicitly requires that
platform feature even when X11 support is selected. The retained `serde` feature is a
compatibility spelling; required configuration and snapshot serialization works without it.
`test-util` adds isolated filesystem/clock and update-result controllers. Native watchers,
HTTP, and worker threads remain the default implementation.

The compiler lint policy covers the guideline compiler set, public docs, and public Debug.
Clippy denies unsafe operations without adjacent proofs and lint allowances without reasons.
Tracked `expect` attributes name the lint and explain why it is needed. Cross-platform or
macro-dependent exceptions retain reasoned `allow` attributes when an expectation would be
unfulfilled on a supported target.

## Broader lint debt

The legacy codebase still has pedantic, cargo, and restriction diagnostics. They are counted
per file and lint in `scripts/guideline-lint-baseline.json`. Run this ratchet on macOS with
Rust 1.98.1:

```sh
RUSTUP_TOOLCHAIN=1.98.1 python3 scripts/check-guideline-lints.py
```

The ratchet enables the remaining guideline lint set and rejects increases. It does not replace
the strict Clippy command above. The broad legacy categories include numeric conversions,
large routines, naming, suggested `must_use` annotations, dependency metadata, and multiple
transitive dependency versions. Numeric conversion warnings require individual bounds review;
a baseline entry is not a safety proof. Native boundary dimensions, lengths, and ownership are
checked separately in the implementation and tests.

The initial baseline avoids mixing a large style rewrite or public dependency/type migration
with these fixes. Reduce entries as code is improved. An explicit `--write-baseline` is available
for compiler upgrades and reviewed exceptions; inspect the diff and explain every increase.
The checker deduplicates library/test diagnostics and normalizes toolchain paths. This is an
incremental adoption policy, not a declaration of complete guideline compliance.

## Native platforms and dependencies

Root workflows verify macOS, Windows, and Linux. The standalone
`windows/setup-helper/Cargo.toml` workspace has its own fmt/test/Clippy steps in Windows CI.
Its tiny, transient installer launcher retains the system allocator to avoid adding a native
allocator dependency to the installation prerequisite. The long-running `vivido` and `vvssh`
executables use mimalloc; the embedding library selects no global allocator for its host.

Miri runs pure Rust row and grid tests. FFmpeg and native window APIs require their real platform
integration tests; Miri is not a substitute for those checks. The decoder preflight accepts only
reviewed FFmpeg 6–9 major pairs, then probes the relevant structure layouts before accessing them.

Windows named-pipe writes cancel and await completion on every failed overlapped wait before
releasing the write buffer, event, or `OVERLAPPED`. The native regression covers both zero and
nonzero deadlines against an undrained pipe. The timeout classification follows Microsoft's
[GetOverlappedResultEx contract](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-getoverlappedresultex),
including `WAIT_TIMEOUT` and `ERROR_IO_INCOMPLETE`.

On 2026-10-08, advisory fixes updated `bytes`, `ringbuf`, `wayland-scanner`, and its `quick-xml`
dependency. `cargo audit` reports no vulnerable packages and retains the informational
RUSTSEC-2026-0192 notice: `ttf-parser` 0.25.1 is unmaintained. It comes through the Linux window
font stack. No advisory suppression was added. Its replacement requires a separately reviewed
upstream dependency migration.

`cargo-udeps` ignores the `clap_complete` development dependency on macOS because its use is
in Linux completion tests. Linux still compiles those tests. The unused direct `toml_edit`
dependency was removed.
