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

## Ignored native tests on Windows

`cargo test -- --ignored --test-threads=1` includes GPU, default-audio-device,
language-binding and ConPTY playback acceptance. Run it on a desktop with those
devices available. The sibling SDK bindings and player/mux executables must be
built from the current checkout; Cargo does not rebuild these external fixtures.
A stale Python extension can fail import with a missing SDK constant even when
Vivido itself builds successfully.

From PowerShell in `vivido/`, refresh the prerequisites:

```powershell
cargo build --manifest-path ../vivi/Cargo.toml --bin vivi
cargo build --manifest-path ../vvmux/Cargo.toml --bin vvmux
Push-Location ../vivid_sdk
# Requires the SDK's existing virtualenv with maturin and installed npm dependencies.
./.venv/Scripts/maturin.exe develop
npm.cmd run build:debug
Pop-Location
```

The Lua fixture also needs LuaJIT and a matching current SDK module. Set
`VIVID_OVERLAY_TEST_LUA` to its executable, or stage `luajit.exe` and `lua51.dll`
in `vivido/target/overlay-test-tools/`. To build LuaJIT locally, clone the official
`https://github.com/LuaJIT/LuaJIT.git` into
`target/overlay-test-tools/LuaJIT`, then run `src/msvcbuild.bat` from its `src`
directory in an x64 MSVC developer command prompt. Copy the resulting interpreter
and DLL to `target/overlay-test-tools/`. Build and stage the SDK module:

```powershell
$env:LUA_LIB = (Resolve-Path target/overlay-test-tools/LuaJIT/src).Path
$env:LUA_LIB_NAME = 'lua51'
cargo build --manifest-path ../vivid_sdk/lua-bindings/Cargo.toml
Copy-Item ../vivid_sdk/lua-bindings/target/debug/vivid_sdk_lua.dll ../vivid_sdk/lua/vivid_sdk.dll
```

Use `VIVI_TEST_BIN` and `VVMUX_TEST_BIN` for other executable locations, and
`VIVI_TEST_MEDIA` for a sufficiently long AV1/Opus fixture instead of the default
`../medias/under_attack.webm`. `VIVID_OVERLAY_TEST_PYTHON` selects another Python
environment with the current native extension installed.

The Windows headless app checks also use built `vvrd` and `vvpaint` siblings
(or `VVRD_BINARY` and `VVPAINT_BINARY`). The Kitty compatibility check requires
`KITIM_BINARY`, a built `kitim` sibling, or
`target/overlay-test-tools/bin/kitim.exe`. These are real external applications,
not replaced by synthetic producers when unavailable.

The overlay movement measurement reports p50, p95 and maximum elapsed time in
every build. Its rendering, compilation-reuse and allocation assertions always
apply. A frame-time budget is hardware/build-specific and must be selected
explicitly; for the original 60 Hz performance gate on a calibrated machine:

```powershell
$env:VIVIDO_OVERLAY_FRAME_BUDGET_US = '16667'
cargo test --release overlay_window_movement_performance_measurement -- --ignored --test-threads=1 --nocapture
Remove-Item Env:VIVIDO_OVERLAY_FRAME_BUDGET_US
```

## Publishing

Publish local path dependencies before Vivido. In particular, Vivido requires `vvte` 0.15.2 or
later for its APC and cell-size callbacks. Cargo removes dependency paths from the published
manifest, so a successful local build does not verify the registry version. From `vvte/`, run
`cargo publish --dry-run` and publish that release first; then run `cargo publish --dry-run`
from `vivido/` to verify the packaged crate against crates.io dependencies.

## Lint policy

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

Ignored tests need native GPU, PTY and private socket access; run them outside a
sandbox that denies those resources. The overlay binding test uses the first
Node executable on `PATH` that successfully runs `--version`, so an installation
with missing shared libraries cannot shadow a working later installation. Set
`VIVID_OVERLAY_TEST_NODE` to select a specific runtime; explicit selections are
used directly and their errors remain visible.

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
