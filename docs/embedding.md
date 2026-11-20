# Hosted clipboard and input

The native Vivid video decoder and verified FFmpeg ABI are shared through
`vivid_sdk::video` (`video-decode`). Vivido retains its per-channel worker,
source playback clock, linked audio, protocol validation and display receipts.
The shared decoder creates no GUI or native event loop; its RGBA buffers retain
the existing bounded two-buffer recycling behavior.

Audio decoding and microphone resampling now use `vivid_sdk::audio`
(`audio-decode`). Vivido retains the device stream, bounded ring, channel-scoped
output controls and linked audio/video clock. The shared decoder validates
canonical initialization and bounds conversion batches, packet size and trim;
it creates no device or playback authority. See the SDK's native audio contract.

The device ring and callback now use independent `vivid_sdk::audio_output`
(`audio-output`). Vivido keeps protocol admission, complete source identity,
scene state and synchronized group coordination. Timed output advances its
clock through underrun silence and discards late samples; live capture keeps its
adaptive reserve. Device callbacks consume whole interleaved frames so partial
publication or fractional gaps cannot move audio between channels.

An embedding host owns its event loop and uses `Processor::new_headless`,
`create_initial_window`, and the existing headless event pump. Resolve a public
window ID through `platform_window_id`; separate processors may reuse the same
public IDs. Call `finish_headless` during worker teardown.

Install a `Box<dyn HostClipboard>` with `Processor::set_host_clipboard` before
creating any windows. The provider supplies text reads/writes and reports whether
primary selection exists. Vivido limits hosted text to 1 MiB. Without primary
selection, selection reads fall back to the ordinary clipboard and selection
writes do nothing. For a user-initiated paste, `load_media` can return validated
local paths or a bounded PNG. Its default delegates to the host’s `load_files`.
Vivido names each pasted image and offers it to the pane’s current file-drop receiver.
A successful transfer can paste the committed path. With no receiver, ordinary
text paste remains available.

Provider calls run on the processor's worker. The provider must bridge native
operations to the platform thread, bound waiting and queued resources, and cancel
stale operations on focus loss or shutdown. Return errors without clipboard
contents. Failed reads suppress the paste entirely, including bracketed-paste
markers. The compositor uses `vivid_sdk::gui_host::clipboard` for these guarantees.

Native key shortcuts delivered with `host_key_input` use the provider and retain
Vivido's selection, OSC 52 permissions, paste protection, and bracketed-paste
behavior. An SDK/platform callback that already supplies clipboard text should
call `Processor::host_clipboard_paste`, which enforces the same 1 MiB bound and
native UI/paste policy. The host must target only its active pane and invalidate
queued input when focus is revoked. Clipboard paste must retain its provenance;
IME commits have different semantics and are unsuitable for clipboard callbacks.
Hosted key and direct clipboard-paste delivery flush that pane's earlier staged
native events first, preserving pointer/modifier ordering without pumping another
owner's queue.

`WindowContext::set_host_scale_factor` updates native fonts and geometry from a
validated scale. `host_ime_cursor_area` exposes the cached terminal/overlay editor
rectangle for host candidate placement. `presented_frame_sequence` lets hosts
skip unnecessary readback of unchanged frames.

The off-by-default `gpu-sharing` feature adds macOS
`display.begin_shared_frame(&vivid_sdk::gpu_share::Pool)` on the renderer worker.
It submits GPU conversion/copy into an immutable IOSurface and returns an owned
`renderer::SharedFrame`, or None while local/aggregate quotas are full. Poll it
without waiting so input and the native event pump continue. It retains its
original device-health monitor; both pre/post-poll checks reject failed work.
`display.poll_shared_frame` additionally rejects a replaced renderer. Revoked
exports can poll their own handle to retire work after hide or geometry changes;
`SharedFrame::wait` supplies a bounded worker-teardown drain. The synchronous
`display.shared_frame` remains available for finite worker-only use. Export
requires a presented frame and neither enables GUI nor changes screenshot transfer.
The host owns publication, complete app identity and consumer lease lifetime.
It should clear the pool's reusable conversion cache on its native worker when
hiding or retiring the renderer, without revoking already-submitted copies.
This also releases idle image allocations; active frame leases remain charged
until producer and consumer retirement. Pool `allocated()` includes idle slots,
while `retained()` counts active leases.
See the SDK's experimental GPU-sharing contract before integrating this API.

With both `test-util` and `gpu-sharing` on macOS,
`display.destroy_gpu_device_for_test` destroys its actual logical WGPU device and
waits at most five seconds for loss notification. It runs only on the native
worker and affects every renderer sharing that logical device. It is absent from
ordinary builds and does not simulate physical GPU removal. A focused native
regression verifies rejection of pending/new exports after loss, retained image
quota through retirement and fresh export after renderer rebuild:

```sh
cargo test --lib --features gpu-sharing,test-util gpu_exports_reject_a_destroyed_device_and_a_rebuilt_renderer_exports_again -- --ignored
```

`display.window.mouse_cursor()` and `mouse_visible()` expose retained pointer
state for native, embedded, and headless windows. Hosts should scope native
pointer changes to that pane's viewport and reject stale asynchronous visibility
requests after focus or pointer input changes.

`WindowContext::host_accessible_text` returns a validated read-only SDK document
of the current terminal viewport, with Unicode scalars, hidden-cell masking,
character geometry, caret, wholly visible linear selection and the visible rows of a rectangular selection. Call it on the
processor worker only when the containing platform accessibility adapter is active
and the pane is visible. The capture preflights aggregate rows, cells, combining
marks, UTF-8 bytes and geometry before allocating the document. Oversized viewports
return an error without terminal contents. The SDK's `accessible-text` feature
adds no GUI dependencies to Vivido.

Hosts must retire snapshots on hide, stop and owner/focus generation changes,
scope synthetic row identities by the containing view, and let their own modal
and workspace policy arbitrate accessible focus actions. This snapshot does not
copy retained history or expose Vivid semantic overlay
trees/actions. It does not replace standalone native accessibility adapters or
alter the presenter's negotiated wire profiles. When a Vivid overlay has focus,
the underlying terminal document does not claim accessible focus.

User-facing hosts should query `WindowContext::running_program` on the processor's
worker before closing a pane or a group. Unix detection distinguishes idle shells
from foreground children, direct programs in the original PTY group, and integrated
in-process commands; unknown executable identity is conservative. This is a recent
advisory sample, not a process-state lock. Keep names/arguments out of host control
mailboxes when a fixed readiness reason suffices. After obtaining scoped consent,
`Processor::close_window(native_id, true)` signals the pane's foreground group;
use the resolved native ID, then call `finish_headless` during worker teardown.
Headless workers do not open their own confirmation dialogs.

For trusted app-content inspection, `WindowContext::host_visible_text(start, count)`
copies selected physical rows of the displayed viewport on the processor worker.
It follows native copy semantics for soft wraps, wide characters, combining marks
and concealed cells, including history currently scrolled into view. It does not
copy undisplayed history or activate accessibility. The caller must enforce its
own complete owner identity and query policy. Preflight rejects invalid ranges,
more than 32 rows, 4,096 selected cells or 16 KiB of encoded text before snapshot
allocation. Independent queries do not form an atomic paginated snapshot.

Use `vivido::is_supported_host_key` before queuing app-defined named key input.
It accepts the existing native logical-key names and printable Unicode while
rejecting raw control characters and names over 128 bytes. Validation alone
does not authorize input; keep the existing focus, visibility and generation
checks at delivery. A queued-input receipt is not evidence of PTY consumption or
displayed output. The compositor's `inspect`, `send_text` and `send_key` methods
apply these helpers through the SDK's bounded application-call lifecycle.

The explicit native `compositor_host` regressions exercise independent owners
reusing public window ID 7, terminal encoding, scale changes, cleanup, and canceled
clipboard reads. Run them where GPU, PTY, and socket access are available:

```sh
cargo test --test compositor_host -- --ignored
```


Terminal and SDK overlay shaping use the same pinned Parley fallback correction.
Complete cluster coverage remains preferred; otherwise a font covering the base
character wins over a font covering only its marks. This preserves an emoji base
when an unsupported accent would otherwise select the primary Latin font. Vivido can paint missing combining marks from additional faces over the
original terminal cell. Marks that share a fallback face are shaped together.
The hosted terminal preserves the grid advance even when script itemization
splits a marked pictograph across runs. Native font selection and shaping run on the processor worker and retain the existing bounded layout caches.

## Overlay frame reservations

Overlay preparation now reserves an explicit immutable snapshot. At most two
reservations may be retained; a consumer must finish or abandon its own handle.
A newer candidate cannot overwrite an older frame's receipt. Admission checks
complete owner/surface/track/channel/epoch identity, local retirement guards,
window stack and placement, viewport and target generations. A stale mixed image
cannot publish input hits or presentation outcomes for any owner. Source binding
validation and receipt publication share one short scene-lock lease, released
before any subsequent GPU work.

The terminal consumer now runs the SDK checked texture renderer on one dedicated
worker per active pane. One pending job coalesces later candidates; the persistent
pool bounds retained exports to two and survives worker/renderer replacement.
Completed immutable images are imported on the host's original renderer and
explicitly unregistered behind GPU retirement fences. Existing displayed output
remains independent of a pending replacement. Final admission checks that the
composed Vello scene actually references that exact imported image, so an
unrelated blank or older terminal scene cannot acknowledge ready output.

Cancellation never joins a worker or destroys the shared logical device on the
terminal thread. The existing frame timer polls pending work and reuses cached
terminal scenes when the overlay image is unchanged. The finite local Metal
checks include original pixel/receipt identity, pending work with eight primary
frames, coalescing, owner close and fresh same-pool peer output. A fourth case
rejects an expired real completion before publication, retains original receipts
and pixels, and recovers with a changed candidate on the same device.
The installed Python blocking/async, Node and Lua socket producers pass using
the working local Node 24 runtime. The existing 240-frame movement gate now
includes checked completion and actual composition; p95 is 15.389 ms against
16.667 ms, with an 18.745 ms maximum. These finite measurements do not establish
declared peak throughput or a broader source mix.
[Asynchronous consumer evidence](../../vivid_compositor/docs/performance/2026-11-14-vivido-vector-async-macos.json).

The [raw-texture integration plan](../../vivid_sdk/docs/RAW-TEXTURE-PLAN.md) retains
source-isolated work refusal, broader asynchronous recovery/visibility checks
and ordinary-window/device/platform gates. The legacy SDK submission-only APIs
have been removed; its last GPU regression caller now uses checked immutable
exports and explicit import retirement. A stale
mixed image is withdrawn in full while a fresh image is prepared; finite output
checks do not prove uninterrupted peer visibility during irreducible workloads
or physical display/GPU faults. The earlier reservation evidence remains
[recorded separately](../../vivid_compositor/docs/performance/2026-11-14-vivido-vector-reservations-macos.json).

Vivido's worker now diagnoses typed checked allocation/work refusal on a healthy
device using private single-drawing probes. Probe starts share a ten-second and
64-observed-attempt allowance; the last native job keeps its own existing limit,
so diagnosis adds at most 127 attempts. Cancellation remains atomic and no host
or terminal lock is held during GPU work. An unattributed composition stays
suppressed without guessing an owner. A proven bad drawing is reported with an
optional checked composition omitting it. The host rejects only an exact current
reservation, latest accepted scene/compiled allocation, window placement,
channel/epoch/retirement guard and target. A shown or replaced drawing cannot be
closed by delayed diagnosis. Ordinary descendants close, and no peer receipt or
input grant is implied by refusal.

When that checked survivor composition still matches a newly reserved snapshot,
it replaces the old mixed image in the same draw. Final primary-frame sampling
still controls admission. A local 8192² Metal regression keeps checked blue peer
pixels through the actual mixed/isolated work bounds, then replaces them with
green without returning an absent overlay. The original bad receipt is
Superseded; the peer's original receipt remains pending until its imported image
is composed. Exact-owner scene-revision Unicode, a later magenta update, the
original shared device and the two-export pool bound pass. This finite fixture
uses direct producer-admission helpers; authenticated hostile-work mixtures and
ordinary-window work refusal remain separate gates.

This regression also exposed Vello's recent image-atlas residency retaining an
entire previous output while a distinct full-size image was silently omitted.
Vivido pins a small `vello_encoding` workspace patch: under pressure it reclaims
residency absent from the complete incoming scene, protecting every current image
including later-resolved peers. It preserves ordinary cache reuse and the 8192
atlas bound. Source textures stay registered until their final frame retires.
The patch must accompany a packaged release; other workspaces and registry
consumers are not qualified by this local build. Aggregate current-scene atlas
overcapacity and advancing peers during diagnosis remain open.
[Shared-device refusal evidence](../../vivid_compositor/docs/performance/2026-11-14-vivido-vector-refusal-macos.json).

The authenticated refusal checkpoint adds two real SDK sessions over the private
presenter sockets, including a newer peer submission after native diagnosis has
begun. A derived survivor reservation is installed before the failed mixed
reservation is abandoned. It retains the original checked peer pixels and
receipt even when newer bytes are accepted, then independently admits the newer
image. Reuse still requires exactly the original topology minus the refused
window, unchanged placement/viewport/target and live source binding. Other
topology or descendant changes cannot relabel the staged pixels.

Both same-local-ID authenticated cases keep old primary pixels and responsive
peer control, deliver the original bad Superseded and pending-until-sampled peer
outcomes, route exact-scene Unicode, reject duplicate outcomes across cached
redisplay, and create fresh content under the refused window's original control
runtime. Sessions and export charges retire to zero. The initial cold run also
exposed a temporary host lock surviving through font-system construction on a
new vector channel. Font configuration is now cloned under a short lock before
construction, preserving the existing input renewal deadlines. This finite
overlap does not prove continuously advancing output during probes, arbitrary
topology changes, resume/hide/resize failure mixtures, two-hop or ordinary-window
refusal, physical input/IME or other devices/platforms.
[Authenticated refusal evidence](../../vivid_compositor/docs/performance/2026-11-14-vivido-vector-auth-refusal-macos.json).

The ordinary macOS window now also passes real hostile-work refusal at
2862×1608 output. A compact 4095-fill scene plus one hit fits the existing
4096-command and negotiated record bounds. Only its source window closes as
Superseded; the same-ID peer's original receipt, exact UI-routed key/Unicode,
green pixels and a later magenta update pass on the same native window and
authenticated sessions. Own-window OS captures show the refused red source
removed and its peer retained. No synthetic display-completion callback drives
this case.

This exposed a frame-timer gap: a source wake could be coalesced during checked
work, then polling stopped after admitting the older survivor image. The peer's
newer accepted receipt stayed pending despite live control. Consuming a ready
image now rearms one candidate check; unchanged content returns to idle. A native
regression drives only the frame-timer predicate, reaches the newer scene and
proves polling stops. Both ordinary visibility/minimization and hostile-work
cases pass after the fix. This finite direct Vivido case does not qualify relay
or compositor process-worker windows, hostile-work visibility/resume mixtures,
continuous independent peer output during diagnosis, physical input/IME/scanout,
or other platforms/devices.
[Ordinary-window refusal evidence](../../vivid_compositor/docs/performance/2026-11-14-vivido-vector-window-refusal-macos.json).

The worker now permits independent checked peer imports while a private source
probe remains pending. One lazy private target/cache shares the output device,
queue and health, with no private import or display receipt. Both job types use
one 127-credit allowance, the original 64-attempt per-job limit and ten-second
bounds. The initial refused mixed job and final survivor output remain separate
normal jobs with their original bounds. Only diagnostic polling uses a
five-millisecond yield; ordinary output now uses the bounded native completion
wait described below. One command and one response slot remain.
A completed peer output waits on this worker for actual primary-scene admission
before probing continues; its wait cannot extend the diagnostic episode's
handoff deadline. Cancellation never joins a worker or holds the host lock.

`prepare_retaining` requires the exact latest admitted snapshot allocation and
all current context/placement/binding guards. Selected windows keep their exact
committed scene and original resources while newer bytes stay pending. Fresh
peers use current content. Vivido keeps its existing two-reservation limit: one
diagnostic frame and one peer output; displayed snapshots are not reservations.
Output exports retain their two-slot pool. A latest-admitted peer witness permits
checked survivors to replace a refused source without an absent peer turn.
Refusal still uses the original isolated drawing's exact latest submission and
compiled allocation; newer healthy content cannot inherit that proof.

Two native component cases cover eight receipt-paced peer revisions and a healthy
source replacement after an admitted peer. They use two complete owners reusing
local IDs, 8192² checked exports and a 64² primary readback. All eight peer images
advance during diagnosis with matching scene-revision Unicode events, retained
red pixels, original outcomes and 96 shared credits. The late proof preserves
new cyan content and its green peer. These are direct scene-admission fixtures.
Two authenticated socket regressions
also pass on this scheduler: the unchanged peer receives Presented only after
its exact pixels are sampled; when a newer accepted scene overtakes an unseen
intermediate scene, only the sampled original receipt receives Presented and
the unseen original receives Superseded. The failed source is never Presented.
Exact-scene Unicode, cached outcome barriers, both original control runtimes and
zero retained sessions/exports pass. The finite authenticated stream follow-up
below adds receipt-paced wire submissions. Sustained streams, ordinary-window
interleaving/recovery and arbitrary source/rate/aggregate mixtures remain separate
acceptance gates.

Combined runs exposed a real Metal GPU address fault and discarded submissions.
The private Vello backend had disabled runtime bounds checks, although its tile
allocator can report overflow and still attempt writes outside the fixed arena.
Checked shader loading now bounds those accesses; failed coarse work also skips
fine dispatch. A zeroed allocator download is never accepted as valid output.
The original tiling, quotas and work limits remain. Generic native command-error
propagation/recovery, physical faults and other devices/platforms remain open.
The final normal-dependency build also passes the existing ordinary AppKit
visibility and hostile-work fixtures. Six-second hide/minimize holds, restore,
exact encoded pixels, synthetic UI-routed Unicode, original receipts, surviving
peer updates and clean process retirement pass. Owned-window OS captures show the
refused source removed while the green peer remains. These finite regressions do
not exercise continuously advancing peers during an ordinary-window probe.

The timer-polling checkpoint failed the 240-frame movement gate at p95 26.893 ms
against 16.667 ms. Prior-loader and prior-renderer controls also failed, so the
shader safety fix was retained. Those failed results remain in the original
checkpoint; the completion-wait follow-up below addresses that local gate.
[Shared-device interleaving evidence](../../vivid_compositor/docs/performance/2026-11-15-vivido-vector-interleave-macos.json).

Ordinary output now waits for native GPU completion on the render worker with a
requested one-millisecond timeout instead of sleeping through a completed job.
A wait timeout means pending work and grants no source-fault evidence. Other
poll errors cancel that job. The UI never performs this wait; cancellation,
per-job/episode deadlines, five-millisecond diagnostic yields, work credits and
reservation/export bounds remain unchanged. A controlled timer/wait comparison
fails at 26.858 ms p95 with the timer and passes at 15.398 ms with the wait.
Six frozen-final-source 240-frame runs pass at p95 14.806–15.096 ms against
the unchanged 16.667 ms gate (1,440 frames). This finite p95 measurement does not guarantee every frame or arbitrary sustained/overlapping load.

A new native fixture uses two real authenticated SDK sessions and eight original
peer submissions, revisions 2–9. Every exact checked image is sampled while
private hostile-work diagnosis is still active, with retained committed red pixels,
pending-until-sampled original Presented receipts and exact-scene Unicode on the
original input lane. The worker admits all eight using 96 shared credits; only
the bad source becomes Superseded. A cached outcome barrier, unchanged peer
channel/epoch and both control runtimes, fresh cyan under the refused owner's
same session and zero retained sessions/exports pass. This is a finite offscreen
wire/Metal burst, not ordinary-window stream or sustained-rate acceptance.
[Authenticated stream and completion-wait evidence](../../vivid_compositor/docs/performance/2026-11-15-vivido-vector-stream-poll-macos.json).

The ordinary AppKit window now also advances all eight original peer revisions
3–10 during actual private diagnosis. Two final-source runs use real authenticated
SDK producers, native queue presentation and ordinary frame admission at
2862×1608/2×. Each exact frame retains committed red beside the expected peer
palette; original receipts and scene-bound committed text agree. Both runs
record eight worker admissions and 72 shared credits. Only the hostile source
is Superseded and removed; the same control sessions, fresh peer input and a
later magenta update remain live. Owned-window OS captures and clean app exits
pass. These results do not establish physical scanout or physical keyboard/IME.

A bounded observer exists only with `test-util`, explicitly enabled by the
fixture's `VIVIDO_OVERLAY_STREAM_CHECK=1`. It selects the fixture's displayed
56-square right-edge peer and known logical sample points, validates coordinates
against both the displayed image and current target, and queues two one-pixel
readbacks after each real native presentation. Eight rows retain at most 4 KiB
of staging buffers. It never creates a frame, waits for the GPU or holds a
private probe. `inspect` reports the exact displayed revisions, native frame and
image generations, pixels and real worker counters. Synthetic text is sent
through the normal service after that draw, and the original producer validates
its target and scene revision before offering the next image. Unit-test GPU hold
and forced-expiry fields remain absent from this development build.

Run this finite window gate on an unlocked Mac with the built SDK Python binding:

```sh
cargo test --offline --locked --features test-util --test native_overlay_window checked_overlays_stream_original_peers_during_native_work_refusal -- --ignored --nocapture
```

Both normal/development all-target suites and strict Clippy, eleven native
snapshot cases, three authenticated refusal/stream cases, three authenticated
resume cases and all three ordinary-window cases pass. The unchanged movement
gate remains below 16.667 ms p95. Sustained arbitrary rates, hostile-work native
hide/resize/resume/recovery overlaps, aggregate atlas/Arena mixtures, generic
Metal command-error recovery and other devices/platforms remain open.
[Ordinary Vivido streaming evidence](../../vivid_compositor/docs/performance/2026-11-15-vivido-vector-window-stream-macos.json).

Three ordinary-window recovery cases now witness actual private work before
native hide, minimize or resize. With the pixel/input observer disabled,
`test-util` inspection exposes only bounded phase, live pass count, hidden-release
and target-size metadata. Hide/minimize accept healthy revision-three source and
revision-two peer scenes while invisible. Six-second holds cross the normal
five-second GPU release timer: frame counters freeze, both fresh receipts remain
pending, accepted/presented floors stay 3/1 and 2/1, and control plus PTY output
advance. Restore produces exact cyan/green pixels and original Presented outcomes.
The stale hostile revision is Superseded without closing either window.

That checkpoint's resize case reaches the exact even 2296×1312 native target before updating
producer bounds from its real viewport. The old private proof cannot close the
healthy replacement. Both original control sessions, complete track/channel
identities, native window identity, source and peer scene-bound input, later
magenta peer output and owned-window captures pass. These finite cases replace
hostile content with healthy bytes; they do not qualify retaining the same hostile
scene across arbitrary recovery or uninterrupted visibility during retargeting.

A separate native component case holds consumer polling after a real GPU job
starts and lets the unchanged ten-second deadline expire on the actual clock.
Primary renders and repeated completed pixel readbacks retain original red/blue
content throughout the stall. Timeout grants neither a new outcome nor source
blame; a fresh candidate later resolves the old original as Superseded and fresh
source/peer receipts as Presented once. Export charges retire to zero. This
uses the unit-test hold, with modeled lane renewal; it is not an authenticated
window stall, a physical GPU hang or a shortened/injected deadline.

That checkpoint runs six finite ordinary-window cases; select its replacement
cases with the `healthy_replacements_survive_native_` filter.
Normal/development Rust gates, the twelve-case native snapshot suite, authenticated
refusal/stream/resume companions and the unchanged movement gate pass. Broader
hostile-work resume/input/epoch/device mixtures, sustained rates, aggregate/Arena,
physical input/IME/scanout and other platform/device gates remain open.
[Native window recovery and elapsed-stall evidence](../../vivid_compositor/docs/performance/2026-11-15-vivido-vector-window-recovery-macos.json).

Four further ordinary-window cases retain the source's exact original scene,
bounds and revision-two receipt while private work is active. Hide and minimize
hold both original receipts pending at accepted/presented floors 2/1 for six
seconds, crossing normal hidden GPU release without closing either window.
Restoration runs fresh checked work, refuses only the offending overlay window,
resolves its original as Superseded and presents the green peer. A 2816×1536
native resize also requires fresh mixed and isolated checks before refusal.
Both authenticated sessions and complete track/channel/media-epoch identities
survive, as do peer scene-bound input and a later magenta submission.

At the smaller 2296×1312 target the same unchanged source legitimately completes
within the checked work budget. Its original receipt becomes Presented, exact
black source and green peer pixels are captured, and both sources accept input.
The peer explicitly raises its own window above the retained large source bounds;
the source is never replaced or resized by the producer. A prior work refusal
does not make the scene permanently invalid at a different target.

Run all ten ordinary-window cases with the existing native command, or filter
`retained_` for these four. These finite cases do not qualify uninterrupted peer
visibility during retargeting, arbitrary repeated recovery, native lease/input/
epoch/device overlaps, sustained aggregate loads or physical/platform gates.
[Retained-scene native recovery evidence](../../vivid_compositor/docs/performance/2026-11-15-vivido-vector-retained-recovery-macos.json).

Two additional ordinary-window cases combine real private work with authenticated
source lease resume and input-lane replacement, both visible and hidden. A Rust
SDK producer creates a ten-second suspension lease, adopts its complete live
window set through `OverlaySession::resume`, and republishes a full scene.
The source retains its owner/window/track IDs, advances only its media channel
from one to two and keeps media epoch one; the same-ID peer stays on channel one.
The old unresolved receipt becomes unusable on transport retirement. Focus is
denied before fresh publication, then cyan/green pixels, original fresh Presented
outcomes and scene-bound source/peer input pass. The old connection's cancellation
handle is called twice after recovery and cannot terminate the resumed owner.

The hidden case holds fresh originals pending at 3/1 and 2/1 for six seconds,
crossing normal hidden GPU release while control and PTY output advance. Neither
window closes; restoration, later magenta peer output, owned-window captures and
clean shutdown pass. These are real socket/native-window cases without a private
GPU hold or observer injection; they do not qualify standalone lane takeover,
media-epoch changes, descendants, device loss or arbitrary repeated combinations.
The current strict healthy-replacement resize case uses 2816×1536: at the
smaller target the original scene can legitimately finish before its replacement
arrives. The separate retained-scene downsize case continues to check that valid
Presented outcome. Final completion is published only after explicit cleanup;
the parent permits child exit only while consuming that final completion.

Build the feature-gated producer before running the twelve-case native suite:

```sh
cargo build --features test-util --example native_overlay_lease_producer
cargo test --features test-util --test native_overlay_window -- --include-ignored --test-threads=1
```

[Ordinary-window leased recovery evidence](../../vivid_compositor/docs/performance/2026-11-15-vivido-vector-leased-recovery-macos.json).
The shared checked MSAA8 backend now repairs the observed high-winding overflow
with full-width counters for crowded nonzero tiles. A preserved pre-fix ordinary
window produces 6,461 wrong outside pixels at 128 contours even though both
original receipts report Presented. The repaired window passes 1/127/128/255/256/
682 contours with zero mismatches among 4,049,280 classified pixels per capture;
the conservative edge band and fixed terminal prefix remain explicit exclusions.
An extra small fill checks shared-state reuse after the crowded path. Source and
peer original revisions, scene-bound Unicode/key input, source close and fresh
peer output remain exact. All 13 ordinary-window cases and a separate winding
repeat pass, along with native worker/refusal/resume regressions and the unchanged
movement percentile gate. Sampling, accepted limits and work budgets are unchanged.
These finite cases leave broader sustained/recovery and physical input/IME/scanout,
other platform/device and publication gates open.
[Original SDK failure](../../vivid_compositor/docs/performance/2026-11-15-sdk-vector-winding-failure-macos.json).
[Wide-winding repair and ordinary-window evidence](../../vivid_compositor/docs/performance/2026-11-15-sdk-vector-wide-winding-macos.json).




The [native lifecycle checkpoint](../../vivid_compositor/docs/performance/2026-11-14-vivido-vector-recovery-macos.json)
adds direct scene-method rebind/suspension/slot-replacement cases, 97×33 retargeting
before and after GPU completion, and actual logical-device destruction with a
pending overlay. Fresh images preserve peer state/pixels and settle current
receipts once. Two successive images after rebuild charge the original pool;
the meter returns to zero after clear. An independent renderer on another
logical device preserves and refreshes pixels. These offscreen tests do not
perform an authenticated lease resume or channel-open handshake, exercise native
window visibility, or qualify physical GPU removal and other platforms.

The [authenticated recovery checkpoint](../../vivid_compositor/docs/performance/2026-11-14-vivido-vector-wire-resume-macos.json)
adds real leased SDK sessions and generation-two channel-open handshakes with a
pending checked Metal job. Policy-one suspension retains window/object metadata
and accepted revision floors, closes old media/input sockets and clears old
scenes, reservations and input. A valid immediate resume waits for prior runtime
retirement within the original handshake deadline; invalid proofs do not wait,
and a healthy control is never forcibly replaced. Late old-lane cleanup checks
the complete control runtime, including when lane generation one is reused.
Fresh exact receipts, owner/peer pixels and committed Unicode input pass. Input
stays revoked until every visible retained window has fresh output; revealing a
stale hidden window revokes that owner's input again until it refreshes. The
hidden-window case is a model regression. These finite offscreen tests do not
qualify ordinary AppKit visibility, physical input/IME, two-hop hostile-work
isolation, physical GPU faults or other platforms.

## Ordinary overlay window acceptance

Native draw admission now checks explicit mapping and minimized state as well as
occlusion. A queued native redraw cannot acknowledge a hidden overlay or consume
its pending damage. Explicit unmapping revokes overlay focus, and macOS reveal
restores a miniaturized window before applying its stacking policy.

The [window follow-up](../../vivid_compositor/docs/performance/2026-11-14-vivido-vector-native-window-macos.json)
records an unlocked prototype with two real socket producers in an ordinary
Vivido window. Six-second unmapped and minimized holds preserve frame sequences
and original receipts while PTY/control state advances. Restore, 1152×784 Retina
resize, exact encoded RGB samples, owner/peer key and Unicode input, and fresh
peer output after owner close pass. Captures in that prototype come from Vivido's
last presented GPU frame, rather than an OS screen image. An odd physical resize
request at 2× scale did not reach its exact requested dimensions; that case stays
unqualified.

Run the final native regression explicitly, with the SDK Python binding built
at its default `.venv` path and the Mac unlocked. The app running `cargo test`
(for example Vivido) must have macOS Screen Recording permission. Enable it in
System Settings → Privacy & Security → Screen & System Audio Recording, then
quit and reopen the app. Permission granted to Codex does not grant it to Vivido.

```sh
cargo test --offline --locked --test native_overlay_window -- --ignored --nocapture
```

The test spawns only its own window/private IPC endpoint and records bounded
public observations and captures in a private temporary directory. It adds
explicit same-local-ID owner and input scene-revision checks plus OS capture of
only its own on-screen window. A preflight rejects a locked session or missing
Screen Recording permission before launch. Its portable prerequisite checks run
with `python3 -m unittest discover -s tests -p 'test_native_overlay_window.py'`.
The strengthened run now passes on the unlocked Mac, including its
own-window OS captures and exact owner/input revision assertions. Both fixture
processes retire normally with no producer cleanup error. See the
[final window checkpoint](../../vivid_compositor/docs/performance/2026-11-14-sdk-vector-checked-only-macos.json). Synthetic UI-routed keys and
Unicode paste do not qualify hardware keyboard/IME candidates, scanout, physical
GPU faults, visible GPU stalls, cross-application activation or other platforms.
