//! Ordinary native-window acceptance of the checked overlay consumer.

// Rust guideline compliant 2026-10-07
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn checked_overlays_preserve_receipts_pixels_and_input_across_native_visibility() {
    run(false);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn checked_overlays_refuse_hostile_work_in_the_ordinary_native_window() {
    run(true);
}

// Native app activation is desktop-global. Keep these owned-window cases
// serialized even when the Rust harness uses several test threads.
#[cfg(target_os = "macos")]
static WINDOW: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "macos")]
fn run(work_refusal: bool) {
    run_mode(work_refusal, false);
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU, built SDK Python binding and bounded observer"]
fn checked_overlays_stream_original_peers_during_native_work_refusal() {
    run_mode(true, true);
}

#[cfg(target_os = "macos")]
fn run_mode(work_refusal: bool, stream: bool) {
    run_recovery(work_refusal, stream, None);
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn healthy_replacements_survive_native_hide_during_private_work() {
    run_recovery(true, false, Some("hide"));
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn healthy_replacements_survive_native_minimize_during_private_work() {
    run_recovery(true, false, Some("minimize"));
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn healthy_replacements_survive_native_resize_during_private_work() {
    run_recovery(true, false, Some("resize"));
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn retained_hostile_scene_is_refused_after_native_hide() {
    run_recovery(true, false, Some("retain-hide"));
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn retained_hostile_scene_is_refused_after_native_minimize() {
    run_recovery(true, false, Some("retain-minimize"));
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn retained_hostile_scene_is_refused_after_native_resize() {
    run_recovery(true, false, Some("retain-resize"));
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn retained_scene_completes_when_native_downsize_fits_the_work_budget() {
    run_recovery(true, false, Some("retain-downsize"));
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built native_overlay_lease_producer example"]
fn leased_owner_resumes_during_native_private_work() {
    run_recovery(true, false, Some("lease-resume"));
}

#[cfg(all(target_os = "macos", feature = "test-util"))]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built native_overlay_lease_producer example"]
fn leased_owner_resumes_while_hidden_after_native_private_work() {
    run_recovery(true, false, Some("lease-hide"));
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires unlocked macOS desktop, native GPU and built SDK Python binding"]
fn wide_nonzero_winding_preserves_native_pixels_originals_and_peer_input() {
    run_case(false, false, None, true);
}

#[cfg(target_os = "macos")]
fn run_recovery(work_refusal: bool, stream: bool, recovery: Option<&str>) {
    run_case(work_refusal, stream, recovery, false);
}

#[cfg(target_os = "macos")]
fn run_case(work_refusal: bool, stream: bool, recovery: Option<&str>, winding: bool) {
    let _window = WINDOW.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut command = std::process::Command::new("python3");
    command
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/native_overlay_window.py"))
        .arg("--binary")
        .arg(env!("CARGO_BIN_EXE_vivido"));
    if work_refusal {
        command.arg("--work-refusal");
    }
    if stream {
        command.arg("--stream-refusal");
    }
    if let Some(mode) = recovery {
        command.arg("--work-recovery").arg(mode);
    }
    if winding {
        command.arg("--wide-winding");
    }
    let status = command.status().expect("ordinary native window acceptance requires Python 3");
    assert!(status.success(), "ordinary native window acceptance failed");
}
