//! Exercise reserved outcomes through the actual native Vivido compositor.

// Rust guideline compliant 2026-10-07
use std::sync::Arc;
use std::time::{Duration, Instant};

use vello::peniko::Color;
use vello::{Scene, kurbo::Affine};
use vivid_protocol::revision::ChannelGeneration;
use winit::dpi::PhysicalSize;

use super::Fixture;
use crate::display::{SizeInfo, renderer::SceneRenderer, window::RenderSource};

#[test]
#[ignore = "requires a native GPU; run explicitly for the real Vivido reservation consumer"]
fn native_consumer_presents_only_the_snapshot_in_its_final_frame() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let scene = f.scene.clone();
    let size = SizeInfo::new(64., 32., 1., 1., 0., 0., false);
    let mut renderer = SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 32), true)
        .expect("native reservation consumer requires a working GPU");
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    renderer.set_vivid_scene(scene.clone());
    let (original_image, original_generation) = ready(&mut renderer, &size, None);
    assert!(renderer.render(&Scene::new(), Color::WHITE).unwrap());
    assert!(
        scene.overlays().lock().unwrap().displayed.is_empty(),
        "a ready image is not acknowledged by a blank terminal scene"
    );
    let mut old_frame = Scene::new();
    old_frame.draw_image(&original_image, Affine::IDENTITY);

    // This finite fixture has no service actor: move its host out only to reuse
    // the exact producer-admission helper, then install it before final render.
    f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
    f.submit(first, 2, 2);
    let replacement = f.host.prepare(&scene).unwrap();
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    assert!(renderer.render(&old_frame, Color::WHITE).unwrap());
    {
        let host = scene.overlays().lock().unwrap();
        assert_eq!(host.displayed[&first].revision, 1);
        assert_eq!(host.displayed[&peer].revision, 1);
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| { *owner == first.context().session() && s.revision == 2 })
        );
        assert!(host.reserved.iter().any(|s| Arc::ptr_eq(s, &replacement)));
    }
    let capture = |renderer: &SceneRenderer| {
        let readback = renderer.begin_screenshot().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pixels) = renderer.poll_screenshot(&readback).unwrap() {
                break pixels;
            }
            assert!(Instant::now() < deadline, "native reservation screenshot timed out");
            std::thread::yield_now();
        }
    };
    let pixels = capture(&renderer);
    let pixel = |x: usize, y: usize| {
        let offset = y * pixels.padded_bytes_per_row as usize + x * 4;
        pixels.bytes[offset..offset + 4].to_vec()
    };
    assert_eq!(pixel(25, 5), [255, 0, 0, 255], "the final image contains the original red scene");
    assert_eq!(pixel(45, 5), [0, 0, 255, 255], "same-ID peer remains blue");
    assert_eq!(pixel(35, 5), [255, 255, 255, 255], "window gaps remain clipped");

    assert!(renderer.render(&Scene::new(), Color::WHITE).unwrap());
    {
        let mut host = scene.overlays().lock().unwrap();
        assert_eq!(
            host.displayed[&first].revision, 1,
            "unrelated terminal render cannot acknowledge the pending image"
        );
        assert!(!host.finish(&scene, &replacement, false));
    }
    let (current_image, current_generation) =
        ready(&mut renderer, &size, Some(original_generation));
    let mut frame = Scene::new();
    frame.draw_image(&current_image, Affine::IDENTITY);
    scene.detach_channel(first.track(2).unwrap(), ChannelGeneration::ONE).unwrap();
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    {
        let host = scene.overlays().lock().unwrap();
        assert_eq!(
            host.displayed[&first].revision, 1,
            "late detached completion cannot restore revision two hits"
        );
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| { *owner == first.context().session() && s.revision == 2 })
        );
    }
    let (peer_image, _) = ready(&mut renderer, &size, Some(current_generation));
    let mut frame = Scene::new();
    frame.draw_image(&peer_image, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    let pixels = capture(&renderer);
    let at = |x: usize| {
        let offset = 5 * pixels.padded_bytes_per_row as usize + x * 4;
        pixels.bytes[offset..offset + 4].to_vec()
    };
    assert_eq!(at(25), [255, 255, 255, 255]);
    assert_eq!(at(45), [0, 0, 255, 255]);
    let host = scene.overlays().lock().unwrap();
    assert!(!host.displayed.contains_key(&first));
    assert_eq!(host.displayed[&peer].revision, 1);
    assert_eq!(host.outcomes[&peer.context().session()].len(), 1);
}

fn ready(
    renderer: &mut SceneRenderer,
    size: &SizeInfo,
    previous: Option<u64>,
) -> (vello::peniko::ImageData, u64) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(media) = renderer.prepare_media(size, 0, &Default::default())
            && previous != Some(media.image_generation)
            && let Some(image) = media.overlay
        {
            return (image, media.image_generation);
        }
        assert!(Instant::now() < end, "checked Vivido image did not become ready");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn held(renderer: &mut SceneRenderer, size: &SizeInfo) {
    let end = Instant::now() + Duration::from_secs(10);
    while !renderer.overlay_held_for_test() {
        renderer.prepare_media(size, 0, &Default::default());
        assert!(Instant::now() < end, "checked worker did not reach a pending GPU poll");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn submit(f: &mut Fixture, surface: vivid_protocol::identity::SurfaceIdentity, revision: u64) {
    let scene = f.scene.clone();
    // No service actors run in this fixture. Only this producer-admission helper
    // temporarily moves the host out; it is restored before any consumer call.
    f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
    f.submit(surface, 2, revision);
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
}

fn capture(renderer: &SceneRenderer) -> crate::display::renderer::ScreenshotPixels {
    let readback = renderer.begin_screenshot().unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pixels) = renderer.poll_screenshot(&readback).unwrap() {
            return pixels;
        }
        assert!(Instant::now() < end, "primary renderer readback did not complete");
        std::thread::yield_now();
    }
}
fn pixel(pixels: &crate::display::renderer::ScreenshotPixels, x: usize) -> [u8; 4] {
    let offset = 5 * pixels.padded_bytes_per_row as usize + x * 4;
    pixels.bytes[offset..offset + 4].try_into().unwrap()
}

#[test]
#[ignore = "requires a native GPU; explicitly qualifies pending worker and primary progress"]
fn pending_worker_preserves_original_pixels_and_coalesces_later_candidates() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let scene = f.scene.clone();
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let size = SizeInfo::new(64., 32., 1., 1., 0., 0., false);
    let mut renderer =
        SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 32), true).unwrap();
    renderer.set_vivid_scene(scene.clone());
    let (original, generation) = ready(&mut renderer, &size, None);
    let mut frame = Scene::new();
    frame.draw_image(&original, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    renderer.hold_overlay_for_test(true);
    submit(&mut f, first, 2);
    submit(&mut f, peer, 2);
    held(&mut renderer, &size);
    assert!(renderer.needs_overlay_frame());
    let start = Instant::now();
    for _ in 0..8 {
        let media = renderer.prepare_media(&size, 0, &Default::default()).unwrap();
        assert_eq!(
            media.image_generation, generation,
            "pending work does not replace displayed pixels"
        );
        assert!(renderer.render(&frame, Color::WHITE).unwrap());
        let pixels = capture(&renderer);
        assert_eq!(pixel(&pixels, 25), [255, 0, 0, 255]);
        assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
        assert!(renderer.overlay_allocations_for_test() <= 2);
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "primary terminal rendering was blocked by the paused worker"
    );
    submit(&mut f, first, 3);
    submit(&mut f, first, 4);
    {
        let host = scene.overlays().lock().unwrap();
        assert_eq!(host.reserved.len(), 1, "later candidates do not accumulate reservations/jobs");
        assert_eq!(host.displayed[&first].revision, 1);
        assert_eq!(host.displayed[&peer].revision, 1);
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| *owner == first.context().session() && s.revision == 2)
        );
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| *owner == first.context().session() && s.revision == 4)
        );
    }
    renderer.hold_overlay_for_test(false);
    let (replacement, next_generation) = ready(&mut renderer, &size, Some(generation));
    // A blank scene cannot settle a completed-but-not-yet-sampled replacement.
    assert!(renderer.render(&Scene::new(), Color::WHITE).unwrap());
    assert_eq!(scene.overlays().lock().unwrap().displayed[&first].revision, 1);
    let mut frame = Scene::new();
    frame.draw_image(&replacement, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    {
        let host = scene.overlays().lock().unwrap();
        assert_eq!(
            host.displayed[&first].revision, 2,
            "the original worker snapshot retains its own receipt"
        );
        assert_eq!(host.displayed[&peer].revision, 2);
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| *owner == first.context().session() && s.revision == 4)
        );
    }
    let (latest, _) = ready(&mut renderer, &size, Some(next_generation));
    let mut frame = Scene::new();
    frame.draw_image(&latest, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    let pixels = capture(&renderer);
    assert_eq!(pixel(&pixels, 25), [0, 255, 0, 255]);
    assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
    let host = scene.overlays().lock().unwrap();
    assert_eq!(host.displayed[&first].revision, 4);
    assert_eq!(host.outcomes[&peer.context().session()].len(), 2);
    let third = host.outcomes[&first.context().session()]
        .iter()
        .find(|o| o.submission.revision == 3)
        .unwrap();
    assert_eq!(third.outcome, vivid_protocol::overlay::wire::PresentationOutcome::Superseded);
    assert!(renderer.overlay_allocations_for_test() <= 2);
}

#[test]
#[ignore = "requires a native GPU; explicitly qualifies worker cancellation and same-pool replacement"]
fn closing_an_owner_cancels_pending_work_and_replaces_the_worker_without_blocking() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let scene = f.scene.clone();
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let size = SizeInfo::new(64., 32., 1., 1., 0., 0., false);
    let mut renderer =
        SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 32), true).unwrap();
    renderer.set_vivid_scene(scene.clone());
    let (original, _) = ready(&mut renderer, &size, None);
    let mut frame = Scene::new();
    frame.draw_image(&original, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    renderer.hold_overlay_for_test(true);
    submit(&mut f, first, 2);
    held(&mut renderer, &size);
    scene.overlays().lock().unwrap().remove_owner(first.context().session());
    scene.remove_session(first.context().session());
    let start = Instant::now();
    renderer.set_vivid_scene(scene.clone());
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "worker retirement waited on the terminal thread"
    );
    renderer.hold_overlay_for_test(false);
    let (peer_only, _) = ready(&mut renderer, &size, None);
    let mut frame = Scene::new();
    frame.draw_image(&peer_only, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    let pixels = capture(&renderer);
    assert_eq!(pixel(&pixels, 25), [255, 255, 255, 255]);
    assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
    assert!(renderer.overlay_allocations_for_test() <= 2);
    let host = scene.overlays().lock().unwrap();
    assert!(!host.displayed.contains_key(&first));
    assert!(!host.outcomes.contains_key(&first.context().session()));
    assert_eq!(host.displayed[&peer].revision, 1);
    assert_eq!(host.outcomes[&peer.context().session()].len(), 1);
}

#[test]
#[ignore = "requires a native GPU; explicitly qualifies post-poll deadline rejection"]
fn completion_after_the_local_deadline_never_replaces_the_displayed_image() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let scene = f.scene.clone();
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let size = SizeInfo::new(64., 32., 1., 1., 0., 0., false);
    let mut renderer =
        SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 32), true).unwrap();
    renderer.set_vivid_scene(scene.clone());
    let (original, generation) = ready(&mut renderer, &size, None);
    let mut frame = Scene::new();
    frame.draw_image(&original, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    // Inject expiration only after a real checked GPU result becomes complete,
    // immediately before the worker's post-poll publication check.
    renderer.expire_completed_overlay_for_test(true);
    submit(&mut f, first, 2);
    renderer.prepare_media(&size, 0, &Default::default());
    let end = Instant::now() + Duration::from_secs(10);
    while renderer.needs_overlay_frame() {
        renderer.prepare_media(&size, 0, &Default::default());
        assert!(Instant::now() < end, "late completion was not retired");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    let pixels = capture(&renderer);
    assert_eq!(pixel(&pixels, 25), [255, 0, 0, 255]);
    assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
    let media = renderer.prepare_media(&size, 0, &Default::default()).unwrap();
    assert_eq!(media.image_generation, generation);
    {
        let host = scene.overlays().lock().unwrap();
        assert_eq!(host.displayed[&first].revision, 1);
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| *owner == first.context().session() && s.revision == 2)
        );
        assert!(
            host.reserved.is_empty(),
            "unchanged expired work is not resubmitted on each frame"
        );
        assert_eq!(host.displayed[&peer].revision, 1);
    }
    renderer.expire_completed_overlay_for_test(false);
    submit(&mut f, first, 3);
    let (fresh, _) = ready(&mut renderer, &size, Some(generation));
    let mut frame = Scene::new();
    frame.draw_image(&fresh, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    let pixels = capture(&renderer);
    assert_eq!(pixel(&pixels, 25), [0, 255, 0, 255]);
    assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
    assert_eq!(scene.overlays().lock().unwrap().displayed[&first].revision, 3);
    assert!(renderer.overlay_allocations_for_test() <= 2);
}

mod recovery;
mod refusal;

#[test]
#[ignore = "requires native GPU; a coalesced candidate must rearm the frame timer exactly once"]
fn admitted_output_rearms_coalesced_candidates_then_returns_to_idle() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let scene = f.scene.clone();
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let size = SizeInfo::new(64., 32., 1., 1., 0., 0., false);
    let mut renderer =
        SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 32), true).unwrap();
    renderer.set_vivid_scene(scene.clone());
    let (original, generation) = ready(&mut renderer, &size, None);
    let present = |renderer: &mut SceneRenderer, image: &vello::peniko::ImageData| {
        let mut frame = Scene::new();
        frame.draw_image(image, Affine::IDENTITY);
        assert!(renderer.render(&frame, Color::WHITE).unwrap());
    };
    present(&mut renderer, &original);
    renderer.hold_overlay_for_test(true);
    submit(&mut f, first, 2);
    held(&mut renderer, &size);
    submit(&mut f, first, 3);
    renderer.hold_overlay_for_test(false);
    let (reserved, generation) = ready(&mut renderer, &size, Some(generation));
    present(&mut renderer, &reserved);
    assert_eq!(scene.overlays().lock().unwrap().displayed[&first].revision, 2);
    assert!(
        renderer.needs_overlay_frame(),
        "the native frame timer must discover coalesced revision three"
    );
    let until = Instant::now() + Duration::from_secs(3);
    let mut latest = false;
    while renderer.needs_overlay_frame() {
        let media = renderer.prepare_media(&size, 0, &Default::default()).unwrap();
        if let Some(image) = media.overlay {
            if media.image_generation != generation {
                latest = true;
            }
            present(&mut renderer, &image);
        }
        assert!(Instant::now() < until, "unchanged overlay kept the frame timer running");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(latest);
    let host = scene.overlays().lock().unwrap();
    assert_eq!(host.displayed[&first].revision, 3);
    assert_eq!(host.displayed[&peer].revision, 1);
    assert!(host.reserved.is_empty());
    assert!(renderer.overlay_allocations_for_test() <= 2);
}

mod refusal_stream;

mod wall_clock;
