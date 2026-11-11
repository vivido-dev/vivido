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
writes do nothing. Hosted media/file clipboard access is unavailable.

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
character geometry, caret and wholly visible linear selection. Call it on the
processor worker only when the containing platform accessibility adapter is active
and the pane is visible. The capture preflights aggregate rows, cells, combining
marks, UTF-8 bytes and geometry before allocating the document. Oversized viewports
return an error without terminal contents. The SDK's `accessible-text` feature
adds no GUI dependencies to Vivido.

Hosts must retire snapshots on hide, stop and owner/focus generation changes,
scope synthetic row identities by the containing view, and let their own modal
and workspace policy arbitrate accessible focus actions. This snapshot does not
copy retained history, represent block selections or expose Vivid semantic overlay
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
when an unsupported accent would otherwise select the primary Latin font. It
does not synthesize marks from a second font. Native font selection and shaping
run on the processor worker and retain the existing bounded layout caches.
