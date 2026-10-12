# Vivido Microsoft Rust guideline gaps

Audit date: **2026-10-08**. Guideline baseline: the repository's
[ms-rust skill](../../.agents/skills/ms-rust/SKILL.md), compliance date **2026-10-07**.

The original audit findings below are retained as a baseline. Remediation on **2026-10-08**
implements **G01–G06, G08–G10, G12–G13, and G15–G16**. **G07, G11, and G14 were explicitly
excluded**. Vivido is not declared fully guideline compliant: panic continuation has a documented
project exception, broader legacy lint debt is ratcheted, and native platform CI must verify its
own platform code.

## Remediation status

| Findings | Result |
| --- | --- |
| G01 | Safe row initialization, release-active nonzero validation, adversarial Default/drop/ZST tests, and Miri coverage. |
| G02 | `TerminfoGuard` owns explicit child defaults; PTY constructors apply them without changing the host environment. Concurrent setup is tested. |
| G03 | Quarantine discards parser/write/terminal state and requires a fresh client restart. IPC and Vivid failures retire affected owners. See [the recovery policy and boundary inventory](panic-recovery.md). |
| G04 | Removed unnecessary raw grid operations, added native-call invariants, checked FFmpeg ABI majors and allocation conversions, corrected native path lifetimes and pointer-aligned Windows buffers, and enabled unsafe linting/Miri. |
| G05–G06 | Public summaries, failure sections, module introductions, a runnable IPC host example, strict rustdoc gates, and bounded resource Debug implementations. |
| G08 | Handshakes read the owning Processor's instance and capability state; two hosts can advertise independently even with reused local IDs. |
| G09–G10 | Expanded platform CI, feature/security/dependency/Miri checks, reasoned lint overrides, and a broader guideline lint ratchet. See [verification policy](rust-verification.md). |
| G12 | Structured JSON events, private-by-default properties, sensitive-context removal, and redaction regressions. See [logging contract](logging.md). |
| G13 | Feature-gated deterministic monitor events/time and update results, cancellation, timeout, and worker-start failures, injected per Processor. |
| G15–G16 | Application-only mimalloc; documented installer-helper exception; explanations for timeout, debounce, and decoder limits. |
| G07, G11, G14 | Deferred at the user's request; error types, export paths, and subsystem/API organization remain. |

### Compatibility impact

Existing public function signatures and export paths are retained. The new service controls and
child-environment methods are additive. Three intentional behavior changes require attention:

- `tty::setup_env()` no longer mutates the calling process. Existing callers using `tty::new`
  receive defaults automatically; callers spawning their own `Command` must retain the guard and
  call `guard.apply(&mut command)` before applying their overrides.
- A quarantined pane rejects `reset_terminal`; use `restart_terminal` to obtain new state.
- Diagnostic logs now use JSON, so external plain-text log parsers need updating.

`Row::new(0)` now panics reliably instead of permitting release-mode undefined behavior. Required
Serde implementations are available with defaults disabled; the existing feature name remains.
The preexisting 0.5.4 and shared-protocol 1.5.11 manifest/lock changes are preserved.

### Remediation validation

Verified locally on macOS with Rust 1.98.1 and FFmpeg 9.0.1. Socket-using test suites
ran with local socket creation allowed. Commands used the existing lockfile; offline mode
was used when the local cache was sufficient.

| Check | Result |
| --- | --- |
| `cargo fmt --all --check` | Pass; stable rustfmt reports the repository's ignored nightly-only settings. |
| `cargo test --workspace --all-targets --locked --offline` | Pass: 936 tests, 28 ignored. Four custom native macOS harnesses also decline execution without `--ignored`. |
| Same test command with `--all-features` | Pass: 938 tests, 29 ignored; same four custom native harnesses require opt-in. |
| `cargo clippy --workspace --all-targets --all-features --locked --offline -- -D warnings` | Pass. |
| Strict all-features documentation (`-D warnings -D missing_docs`) | Pass. |
| `cargo test --doc --all-features --locked --offline` | Pass: two doctests. |
| `cargo run --example library_api --locked --offline` | Pass: real IPC handshake, claimed host request/reply, and Processor cleanup. |
| `cargo hack check --feature-powerset --all-targets --locked` | Pass: all 36 configurations. |
| `cargo audit` | No vulnerable dependencies; one informational unmaintained `ttf-parser` notice remains, documented in the verification policy. |
| `cargo +nightly udeps --workspace --all-targets --all-features` | Pass, with the documented platform-specific `clap_complete` development-dependency exception. |
| Miri row construction and grid tests | Pass: three adversarial construction tests and 28 grid tests. |
| Guideline lint ratchet | Pass with the reviewed legacy baseline; it does not establish full guideline compliance. |
| Root workflow YAML and Git whitespace checks | Pass. |

Native Linux and Windows builds, documentation, runtime tests, and the new Windows named-pipe
timeout regression still require their CI hosts. A local MSVC cross-check was blocked by missing
Windows SDK/C headers in transitive C dependencies. Interactive GPU/accessibility harnesses and
release-mode performance measurements were not run. The new CI gates cover supported platform
builds; a configured gate is not a passing CI result.

## Scope and method

- Reviewed the working tree of Vivido 0.5.4 at commit
  `bf58d1c9b370edc7c4bc251ae689900682e7a74f`, including the existing manifest/lockfile edits.
  Those edits were preserved. The original audit changed documentation only; remediation is summarized above.
- Inventoried 164 tracked Rust files, totaling 107,665 lines, including tests, examples, build
  code, and the Windows setup helper. Used repository-wide searches plus focused source review;
  this is not a line-by-line soundness certification of every file.
- Applied both application and library guidance: Vivido publishes an embedding API used by
  native hosts, in addition to the `vivido` and `vvssh` executables.
- Read the root [AGENTS.md](../../AGENTS.md). No closer AGENTS.md or standalone architecture
  document was found in Vivido. Architectural context came from [src/lib.rs](../src/lib.rs),
  [README](../README.md), [IPC](ipc.md), [headless sessions](headless.md), and
  [features](features.md).
- Read guideline files 01–15 because the scope includes agent-facing APIs, desktop behavior,
  correctness, documentation, native interop, declarative macros, performance, project layout,
  and embedding-library design. Files 06 and 10 contain headings only; substantive library
  and unsafe rules come from files 12–15 and 03 respectively.
- Findings identify concrete evidence and a closure criterion. P1 means a safety fix to prioritize;
  P2 means a material contract, resilience, or verification gap; P3 means lower-risk maintenance
  work or a recommendation requiring design/performance judgment. A policy deviation is not,
  by itself, proof of a runtime defect.

## Findings at a glance

| ID | Priority | Gap | Guideline IDs |
| --- | --- | --- | --- |
| G01 | P1 | Zero-column row construction can write outside an allocation | M-UNSOUND, M-UNSAFE |
| G02 | P1 | Safe environment setup cannot uphold Unix environment safety | M-UNSOUND, M-UNSAFE |
| G03 | P2 | Panic containment resumes the process without a restart policy | M-PANIC-CONTINUATION, M-PANIC-IS-STOP |
| G04 | P2 | Unsafe justification and validation are incomplete | M-UNSAFE, M-STATIC-VERIFICATION |
| G05 | P2 | Public documentation and usable embedding examples are incomplete | M-CANONICAL-DOCS, M-MODULE-DOCS, M-DESIGN-FOR-AI |
| G06 | P2 | Public types lack Debug implementations | M-PUBLIC-DEBUG |
| G07 | P2 | Library errors have inconsistent contracts and lose cause information | M-ERRORS-CANONICAL-STRUCTS, M-PUBLIC-DISPLAY |
| G08 | P2 | Embedding correctness depends on process-global registries | M-AVOID-STATICS |
| G09 | P2 | Check-in gates cover only part of the guideline verification set | M-STATIC-VERIFICATION, M-TAUTOLOGICAL-TESTS |
| G10 | P3 | Handwritten lint suppressions lack tracked expectations and reasons | M-LINT-OVERRIDE-EXPECT |
| G11 | P3 | Public exports duplicate paths and omit inline documentation | M-SINGLE-ITEM-PATH, M-DOC-INLINE, M-FOREIGN-REEXPORTS |
| G12 | P2 | Logging flattens events and emits unredacted contextual data | M-LOG-STRUCTURED |
| G13 | P2 | Public I/O services cannot be driven by deterministic test controls | M-MOCKABLE-SYSCALLS |
| G14 | P3 | Embedding APIs expose implementation structure and large subsystems | M-AVOID-WRAPPERS, M-INIT-CASCADED, M-BALANCED-MODULES, M-SMALLER-CRATES |
| G15 | P3 | Executables use the default allocator | M-MIMALLOC-APPS |
| G16 | P3 | Operational constants lack selection rationale | M-DOCUMENTED-MAGIC |

## Safety and recovery

### G01 — Row::new(0) is unsound in release builds

**Evidence:** [src/terminal/grid/row.rs](../src/terminal/grid/row.rs), lines 33–55; public
re-export in [grid/mod.rs](../src/terminal/grid/mod.rs), line 19.

`Row::new` accepts any `usize`. Its only nonzero check is `debug_assert!(columns >= 1)`.
With zero columns and debug assertions disabled, `Vec::with_capacity(0)` allocates no storage,
the `1..columns` loop does nothing, and the unconditional final `ptr::write` still writes one
`T`. For a nonzero-sized type such as `u8`, safe code can therefore trigger undefined behavior
through `vivido::terminal::grid::Row::<u8>::new(0)`. Internal callers using valid terminal
dimensions do not make this public safe constructor sound.

**Close:** use safe initialization such as `resize_with`, with a release-active check if zero
columns are forbidden. Cover zero, one, and ordinary widths, zero-sized elements, and a
panicking `Default` implementation. Run the pure-Rust cases under Miri. This finding follows
directly from the source; no undefined-behavior reproducer was executed during the audit.

### G02 — tty::setup_env exposes unsafe Unix environment mutation as a safe API

**Evidence:** [src/terminal/tty/mod.rs](../src/terminal/tty/mod.rs), lines 145–197.

The public safe function calls `std::env::set_var` and `remove_var` inside unsafe blocks. On
Unix, these operations require ensuring other threads do not concurrently read or write the
environment, including through native libraries. An embedding caller can call `setup_env`
after starting threads; the function neither enforces the requirement nor exposes it as an
unsafe caller contract. A startup-only call in Vivido's own binary does not constrain library
callers. This is a Unix safety finding, not a claim that the Windows implementation of
environment mutation has the same restriction.

**Close:** preferably provision terminfo and shell-integration resources into an explicit
environment object applied to each child's `Command`. If process mutation remains necessary,
make that operation unsafe, document its complete `# Safety` contract, and audit every caller's
startup ordering. Test the safe child-environment path without changing global environment.

### G03 — Continued execution after panic is a deliberate guideline deviation

**Evidence:** [src/client_fault.rs](../src/client_fault.rs), lines 87–94;
[src/terminal/event_loop.rs](../src/terminal/event_loop.rs), lines 146–152;
[src/event.rs](../src/event.rs), lines 4764–4784.

`catch_unwind(AssertUnwindSafe(work))` converts panics to bounded fault metadata. The host
continues running, and `ResetClient` resets existing parser/terminal state and resumes a pane.
The Microsoft guidance calls for a controlled application restart after panic containment.
The implementation instead treats pane recovery as sufficient; `AssertUnwindSafe` does not
prove that all shared state remains valid.

**Close:** document and resolve the policy explicitly. To meet the restart guidance while
preserving unrelated owners, isolate panic-prone work in restartable worker processes. If
in-process recovery is retained as a project exception, inventory every catch boundary,
invalidate all affected resources, and fault-inject partial mutations at each boundary with
two owners reusing local IDs. This audit establishes the policy gap, not an additional proven
memory-safety bug in those recovery paths. Malformed client input should remain an ordinary
validated error; it should not be converted into a process-wide panic.

### G04 — Unsafe assurance is not consistently reviewable

**Evidence:** the row initialization above has only an optimization claim; examples of FFI
operations without adjacent safety reasoning occur in [src/vivid/decoder.rs](../src/vivid/decoder.rs),
lines 267–313, and [src/vivid/audio.rs](../src/vivid/audio.rs), lines 1183–1195.
No Miri job was found in the reviewed repository workflows.

FFmpeg interop is a valid reason for unsafe code. The module-level ABI explanation in
[src/vivid/ffmpeg.rs](../src/vivid/ffmpeg.rs) is useful, but does not establish each operation's
pointer lifetime, buffer capacity, aliasing, ownership, and thread-affinity requirements.
The custom row optimization also needs benchmark evidence and adversarial validation under
M-UNSAFE. This is an assurance gap; the cited FFI calls are not all asserted to be unsound.

**Close:** remove unnecessary unsafe code, document the remaining invariants at their use or
shared abstraction, and enable `undocumented_unsafe_blocks`. Build a Miri-compatible target
for pure Rust abstractions; use native integration and sanitizer checks for platform/FFmpeg
code that Miri cannot execute. Preserve existing documented native-handle escape hatches.

## Library contracts and documentation

### G05 — Public docs lack both coverage and canonical failure sections

**Evidence:** `cargo rustdoc --lib -- -W missing_docs` reports **1,013 missing-documentation
warnings** plus **four rustdoc link/HTML warnings** on macOS. Missing-doc categories include
26 modules, 58 structs, 19 enums, 44 associated functions, and 186 methods.

[src/lib.rs](../src/lib.rs) has only a one-line crate introduction;
[src/config/mod.rs](../src/config/mod.rs) has no module introduction. Fallible public methods
such as `Processor::create_initial_window` ([event.rs](../src/event.rs), lines 547–552) and
`WindowContext::initial` ([window_context.rs](../src/window_context.rs), lines 423–429) lack
`# Errors` sections. [examples/library_api.rs](../examples/library_api.rs) checks type sizes
rather than showing a usable embedding lifecycle. Only one doctest runs, for a terminal test
helper, rather than the main embedding entry points.

The four rustdoc diagnostics are an unresolved `IoListener` link in `src/lib.rs:64`, an
unresolved platform-gated `set_taskbar_progress` link in `src/display/progress_indicator.rs:4`,
a public-to-private `spawn_detached` link in `src/headless/macos.rs:18`, and an unescaped
`<pid>` in `src/cli.rs:65`.

**Close:** document the supported embedding lifecycle, ownership, thread affinity, side effects,
shutdown, and error/recovery contracts. Add short summaries and applicable Errors, Panics,
Safety, and Abort sections. Provide a working host/listener/request/shutdown example and
compile it in CI. Fix the four rustdoc warnings and ratchet documentation coverage by module.
Warnings measure missing annotations, not completeness of existing prose.

### G06 — Debug coverage is incomplete

**Evidence:** `cargo rustc --lib -- -W missing_debug_implementations` emits **52 type diagnostics**
on macOS. Publicly reachable examples include `ConfigMonitor`
([monitor.rs](../src/config/monitor.rs):25), `Processor` ([event.rs](../src/event.rs):382), and
`ScreenshotReadback`, `ScreenshotPixels`, and `SceneRenderer`
([renderer.rs](../src/display/renderer.rs):87, 98, 128).

**Close:** add useful, bounded Debug implementations and enable the compiler lint. Implement
manual redaction for state containing credentials, terminal content, or capability material,
with regression tests for leakage. The 52 diagnostics include internal public declarations;
they are not an exact count of externally reachable missing implementations.

### G07 — Library errors need canonical contracts and intact cause chains

**Evidence:** public `config::Error`, renderer `Error`, and `UpdateError` are exposed enums
without captured backtraces; embedding methods return `Box<dyn Error>`. These diverge from
the situation-specific error structs required by M-ERRORS-CANONICAL-STRUCTS.
The application-only relaxation in M-APP-ERROR does not cover an API consumed by other crates.

There are also narrower actionable defects:

- [config/mod.rs](../src/config/mod.rs), lines 59–65, forwards each wrapped error's
  `source()` instead of returning the wrapped error itself. This skips the immediate cause
  and often produces no cause at all. `UpdateError::source()` already uses `Some(error)`.
- [renderer.rs](../src/display/renderer.rs), line 125, implements `Error` without forwarding
  its wrapped Vello/Windows causes.
- The public `host::IpcError` ([polling/ipc.rs](../src/polling/ipc.rs), lines 325–343) has
  structured JSON fields but no `Display` or `std::error::Error` implementation, despite being
  returned as a Rust error by APIs such as `Processor::reset_terminal`.

**Close:** expose domain error structs with private kinds, cause forwarding, optional captured
backtraces, and useful predicates. Repair source chains and make the Rust IPC error usable
with ordinary error propagation. Keep serialized IPC errors bounded and secret-free; do not
send backtraces or internal causes over the wire. Application entry points may retain a
consistent application error type.

### G08 — Host capability registries are process-global

**Evidence:** `HOST_METHODS` and `HOST_METHOD_CAPABILITIES` in
[polling/ipc.rs](../src/polling/ipc.rs), lines 152–159; instance state at line 85;
`Processor::claim_ipc_methods` and `claim_ipc_method_capabilities` in
[event.rs](../src/event.rs), lines 3800–3821.

Each Processor has its own claimed methods, but publishes them into the same global handshake
registry. One host's claim replaces the published set for another host in the same process.
Multiple linked Vivido versions instead have independent copies of that state. Correctness
therefore depends on an implicit singleton arrangement rather than explicit ownership.

**Close:** give Processor and IoListener an explicit shared host/instance context. Verify
independent capability advertisements for two contexts, including overlapping method names
and teardown of one context. If only one context per process is supported, enforce and
document that constraint. Do not mechanically remove immutable caches or performance-only
statics, which M-AVOID-STATICS permits.

## Verification and operations

### G09 — Passing default gates do not cover the guideline baseline

**Evidence:** the reviewed [macOS workflow](../../.github/workflows/vivido-macos-ci.yml) and
[Windows workflow](../../.github/workflows/vivido-windows-ci.yml) run fmt, all-target tests,
and Clippy with warnings denied. They do not configure the full recommended compiler,
pedantic/cargo/restriction Clippy set or run cargo-audit, cargo-hack, cargo-udeps, Miri, rustdoc
warning checks, or doctests. There is no Linux verification workflow in the inspected root
workflow directory. The Windows job explicitly runs ignored headless tests; the macOS job
does not exercise its opt-in desktop/pane tests.

`clippy::all` in [src/lib.rs](../src/lib.rs), lines 3–5, omits several guideline categories,
which explains how substantial documentation and Debug gaps coexist with a clean Clippy run.
`windows/setup-helper` has its own workspace and is not a member of the root Vivido workspace;
`--workspace` at the Vivido root does not verify that crate.

The tail of `every_worker_class_contains_its_panic_and_the_host_keeps_running`
([client_fault.rs](../src/client_fault.rs), lines 131–133) increments a local integer and
asserts it equals one. That assertion does not exercise a host event loop; the earlier
worker-catch assertions do test real behavior.

**Close:** ratchet in the guideline lint set with reasoned exceptions; add dependency/feature
checks, rustdoc/doctest gates, Linux coverage, and a focused unsafe-validation target. Schedule
native desktop tests where supported and verify the separate Windows helper explicitly.
Replace the local-counter assertion with observable host progress after an injected failure.
Treat unavailable tooling as unverified, not as a clean result.

### G10 — Lint overrides should track expectations

**Evidence:** source search found 27 direct `allow` attributes and one direct `expect`
attribute. Handwritten examples include [cli.rs](../src/cli.rs):353 and
[terminal/grid/row.rs](../src/terminal/grid/row.rs):128, without `reason` fields.
Counts are textual and exclude conditional `cfg_attr` overrides.

**Close:** use `#[expect(..., reason = "...")]` for intentional item-level exceptions.
Review platform-dependent exceptions separately so they do not become unfulfilled on another
target. Retain `allow` where justified for generated code or macros, as the guideline permits.

### G11 — Public item paths and re-export documentation need consolidation

**Evidence:** [src/lib.rs](../src/lib.rs), lines 13–59, exposes both public modules and root
re-exports: `vivido::Processor`/`vivido::event::Processor`,
`vivido::WindowOptions`/`vivido::cli::WindowOptions`, and
`vivido::WindowContext`/`vivido::window_context::WindowContext`, for example.
The crate-owned root and host re-exports lack `#[doc(inline)]`.
[terminal/mod.rs](../src/terminal/mod.rs):23 publicly re-exports the external `vvte` crate.

**Close:** choose one user-facing path per item and make implementation modules private where
appropriate. Inline crate-owned exports, and justify any foreign re-export as an umbrella or
technical-split exception. The hidden `binary` module is not counted as a duplicate-user-path
gap, and platform HAL glob exports in `terminal::tty` are explicitly permitted by
M-NO-GLOB-REEXPORTS.

### G12 — Logs flatten structured context and lack a redaction contract

**Evidence:** [logging.rs](../src/logging.rs), lines 160–179, eagerly renders `record.args()`
into a String. [config/mod.rs](../src/config/mod.rs):133, 154–158 and
[config/monitor.rs](../src/config/monitor.rs):87 interpolate paths/errors into free-form
messages. The logger has levels and targets, but no preserved named event properties.

**Close:** retain stable event names and structured fields through the logging backend, with
explicit redaction of identifying paths and sensitive values. Test diagnostic output with
synthetic secrets. Measure overhead on parser/media hot paths before choosing instrumentation
density. This finding does not assert that Vivid root secrets are currently being logged.
Keep intentional CLI stdout and Cargo build-script directives as output, not telemetry.

### G13 — Mockability stops short of public service boundaries

**Evidence:** `ConfigMonitor::new` ([config/monitor.rs](../src/config/monitor.rs), lines 32–107)
directly creates a native watcher and thread and reads `Instant::now()`.
`update::spawn_check` and `spawn_download` ([update/mod.rs](../src/update/mod.rs), lines 273–333)
own their workers and real fetch/download operations. Callers cannot inject watcher events,
clock advancement, HTTP failures, or worker-start failures through these public APIs.

Some lower-level helpers already accept explicit inputs, such as stale-update cleanup with a
supplied directory/time and progress-throttle checks. The gap is the remaining orchestration,
not an absence of tests throughout the project.

**Close:** add narrow internal I/O/clock boundaries and expose controlled test support when
downstream hosts need it, behind a clear `test-util` feature. Verify debounce, cancellation,
timeout, and failure behavior deterministically without network or desktop dependencies.

## Lower-priority design and performance work

### G14 — Simplify the embedding surface before splitting implementation

**Evidence:** `WindowContext::initial/additional`
([window_context.rs](../src/window_context.rs), lines 423–479) expose `Rc<UiConfig>`, with four
or five constructor parameters; the private constructor has six. The publicly visible event
module is 7,586 lines; `vivid/mod.rs` is 11,382 lines and `polling/ipc.rs` is 4,575 lines.
These sizes identify review candidates, not automatic violations by line count.

**Close:** group dependencies and options semantically and hide ownership wrappers behind
host-facing handles where practical. Separate IPC dispatch, host integration, and update UI
from the main event module, then evaluate independently useful crates. Winit/native handle
types may be justified interoperability dependencies; document that contract instead of
wrapping every foreign type mechanically. Preserve native GUI thread affinity rather than
forcing all renderer types to implement Send.

### G15 — Evaluate mimalloc at the executable boundary

**Evidence:** [Cargo.toml](../Cargo.toml), [src/main.rs](../src/main.rs), and
[src/bin/vvssh.rs](../src/bin/vvssh.rs) have no mimalloc dependency or global allocator.
This differs from the application allocator recommendation.

**Close:** benchmark mimalloc for shipped executables on macOS, Linux, and Windows, including
parser throughput, frame latency, memory use, and startup. Adopt it where suitable or record
an evidence-based exception. Do not install a global allocator from the embedding library.
No performance improvement is claimed without measurement.

### G16 — Explain timeout and geometry-limit choices

**Evidence:** [config/monitor.rs](../src/config/monitor.rs):19 specifies a 10 ms debounce
without explaining its selection; [cli.rs](../src/cli.rs):852–854 returns a bare 30,000 ms
default; [vivid/decoder.rs](../src/vivid/decoder.rs):271 embeds an 8192 dimension limit.

**Close:** use named constants and state why each limit was chosen, what changing it affects,
and which external contracts constrain it. Units-only comments do not supply that rationale.

## Existing strengths, exceptions, and audit limits

- Edition 2024 and an explicit MSRV are present. The root crate targets Rust 1.95.0;
  the separate setup helper targets 1.88. No unsupported latest-version assumption is needed.
- Public native-parent construction already has a `# Safety` contract and explicit Send/Sync
  reasoning in [cli.rs](../src/cli.rs), lines 856–887.
- Vivid has substantial regression coverage, including two owners reusing local IDs in
  [audit_regressions.rs](../src/vivid/audit_regressions.rs). Preserve and extend that behavior
  during lifecycle changes.
- Parser benchmarks, a flamegraph script, and benchmark documentation exist. This satisfies
  important parts of M-HOTPATH; no blanket claim of missing performance work is warranted.
- Configuration helpers use declarative macros. Companion proc-macro crate/version rules
  are not applicable to these in-crate macros. No exported Rust DLL interoperability boundary
  was identified, so M-ISOLATE-DLL-STATE is not treated as a demonstrated gap.
- The repository explicitly contains separate projects rather than one root Cargo workspace.
  That instruction overrides the generic single-workspace/flat-crates recommendations across
  CI. The nested, independent Windows setup helper remains a local layout/governance deviation
  to document; consolidating the whole repository is not a remediation proposed by this audit.
- Vivido is expressly a native desktop terminal. FFmpeg/pkg-config and platform dependencies
  are documented; M-OOBE has a platform-specific exception. A no-media embedding configuration
  could improve adoption, but these prerequisites alone are not classified as a defect.
- Requiring Wayland on Linux is an explicit platform policy, not proof that features are
  non-additive. Feature combinations were not built during this audit; cargo-hack coverage
  remains open. Generic target-cpu optimization is not prescribed for a public desktop
  distribution under the server-oriented M-TARGET-CPU guidance.
- No compliance markers were added. The skill's marker is conditional on a file being fully
  compliant, which this sampled audit does not certify. Lack of a marker is not itself a gap.
- Windows/Linux execution, native desktop tests, release-mode/Miri validation, dependency
  advisory scans, unused-dependency checks, and exhaustive API/rule review remain unverified.
  No overall percentage score is assigned.

## Original audit validation

Commands ran from `vivido/` on macOS with rustc/cargo 1.98.1. `--locked --offline` preserved
the existing dependency resolution and used the local cache.

| Command | Result |
| --- | --- |
| `cargo metadata --no-deps --format-version 1 --locked --offline` | Pass; root workspace contains only the Vivido package, not the separate Windows setup helper. |
| `cargo fmt --all --check` | Pass; stable rustfmt warns that nightly-only formatting settings are ignored. |
| `cargo test --workspace --all-targets --locked --offline` | Initial sandbox run failed at a vvssh test's socket creation with PermissionDenied. Full rerun outside the sandbox passed: 923 tests passed, 28 ignored, plus four native macOS custom harnesses declined execution without `--ignored`. |
| `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | Pass with the project's existing lint configuration. |
| `cargo rustdoc --lib --locked --offline -- -W missing_docs` | Completed; 1,013 missing-documentation warnings and four link/HTML warnings. |
| `cargo rustc --lib --locked --offline -- -W missing_debug_implementations` | Completed; 52 missing-Debug type diagnostics. |
| `cargo test --doc --locked --offline` | Pass; one doctest. |

The additional documentation/Debug checks intentionally enabled warnings rather than changing
the project's lint policy. Their counts are specific to the macOS configuration and should
not be extrapolated to every target or feature combination.

## Suggested remediation order

1. Fix G01 and G02, with focused regression and unsafe validation.
2. Resolve the recovery policy (G03), inventory unsafe invariants (G04), and make host state
   explicitly owned (G08).
3. Stabilize public error and documentation contracts (G05–G07), then consolidate paths (G11).
4. Ratchet verification (G09–G10), logging (G12), and deterministic service tests (G13).
5. Evaluate API/module changes, allocator choice, and constant rationale (G14–G16) using
   measured benefit and the repository's existing architecture.
