//! Authenticated hostile-work refusal through the production shared-device consumer.

// Rust guideline compliant 2026-10-07
use std::{
    thread,
    time::{Duration, Instant},
};

use vello::{Scene, kurbo::Affine, peniko::Color as GpuColor};
use vivid_sdk::overlay::PresentationOutcome;
use vivid_sdk::overlay::{
    Brush, Canvas, Color, Event, OverlayWindowOptions, Path, Rect, WindowMode,
};
use vivid_sdk::{OverlayLaneEvent, OverlaySession, OverlayWindow};
use winit::dpi::PhysicalSize;

use super::{completed_overlay_image, live_sessions, lock, test_config, test_geometry, wait_until};
use crate::display::{SizeInfo, renderer::SceneRenderer, window::RenderSource};
use crate::vivid::VividService;

mod stream;

fn canvas(count: usize, color: u32) -> Canvas {
    let mut canvas = Canvas::new();
    for _ in 0..count {
        canvas
            .fill(
                Path::rectangle(Rect::new(0., 0., 1024., 1024.).unwrap()).unwrap(),
                Brush::Solid(Color(color)),
            )
            .unwrap();
    }
    canvas
}

fn accepted(window: &OverlayWindow, revision: u64) {
    assert!(wait_until(Duration::from_secs(3), || window.reconcile().unwrap().accepted_revision
        == revision));
}

fn present(renderer: &mut SceneRenderer, image: &vello::peniko::ImageData) {
    let mut frame = Scene::new();
    // Render the real 8192² checked export into a small primary target. This
    // exercises actual import/sampling without a second large window/readback.
    frame.draw_image(image, Affine::scale(1. / 128.));
    assert!(renderer.render(&frame, GpuColor::WHITE).unwrap());
}

fn pixels(renderer: &SceneRenderer) -> crate::display::renderer::ScreenshotPixels {
    let readback = renderer.begin_screenshot().unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(pixels) = renderer.poll_screenshot(&readback).unwrap() {
            return pixels;
        }
        assert!(Instant::now() < until, "primary imported-image readback stalled");
        thread::yield_now();
    }
}

fn at(pixels: &crate::display::renderer::ScreenshotPixels, x: usize) -> [u8; 4] {
    let offset = pixels.padded_bytes_per_row as usize + x * 4;
    pixels.bytes[offset..offset + 4].try_into().unwrap()
}

fn text(session: &OverlaySession, window: &OverlayWindow, revision: u64, expected: &str) {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        match session.wait_event(Duration::from_millis(1)).unwrap() {
            Some(OverlayLaneEvent::Outcome(outcome)) => {
                // Original registered receipts consume their first outcome in
                // the SDK receiver. Generic outcomes indicate duplicates/orphans.
                panic!("unexpected unregistered peer outcome: {outcome:?}");
            },
            Some(OverlayLaneEvent::Input(input)) => {
                if let Event::Text(value) = &input.event {
                    assert_eq!(value, expected);
                    assert!(session.event_targets(&input, window).unwrap());
                    assert_eq!(input.address.context_id, 1);
                    assert_eq!(input.address.surface_id, 1);
                    assert_eq!(input.address.generation, 1);
                    assert_eq!(input.scene_revision, revision);
                    return;
                }
            },
            _ => (),
        }
        assert!(Instant::now() < until, "original surviving input lane stalled");
    }
}

#[test]
#[ignore = "requires native GPU and sockets; original work-refusal receipts and survivor input"]
fn authenticated_work_refusal_preserves_peer_and_keeps_the_original_owner_session_live() {
    refuse(false);
}

#[test]
#[ignore = "requires native GPU and sockets; newer peer media during isolated source diagnosis"]
fn authenticated_refusal_matches_each_original_peer_outcome_to_sampled_pixels() {
    refuse(true);
}

fn refuse(advance_peer: bool) {
    let _gpu = crate::display::renderer::gpu_test_lock();
    let service =
        VividService::start_with_wake(test_geometry(), std::sync::Arc::new(|_| {})).unwrap();
    service.update_overlay_viewport(8192., 8192., 8.);
    let peer = OverlaySession::connect(test_config(&service)).unwrap();
    let neighbor = peer
        .create_window(OverlayWindowOptions::new(
            Rect::new(960., 0., 64., 64.).unwrap(),
            WindowMode::Floating,
        ))
        .unwrap();
    let owner = OverlaySession::connect(test_config(&service)).unwrap();
    let window = owner
        .create_window(OverlayWindowOptions::new(
            Rect::new(0., 0., 960., 1024.).unwrap(),
            WindowMode::Floating,
        ))
        .unwrap();
    let initial = window.submit(canvas(1, 0xff0000ff)).unwrap();
    let peer_initial = neighbor.submit(canvas(1, 0x0000ffff)).unwrap_or_else(|error| {
        panic!(
            "initial peer submission: {error}; lane diagnostic: {:?}",
            peer.wait_event(Duration::ZERO)
        );
    });
    accepted(&window, initial.revision());
    accepted(&neighbor, peer_initial.revision());
    let drawings = lock(&service.shared.overlays).drawing(&service.scene);
    assert_eq!(drawings.len(), 2);
    let first = drawings.iter().find(|d| d.bounds.origin.x.get() == 0.).unwrap().clone();
    let other = drawings.iter().find(|d| d.bounds.origin.x.get() == 960.).unwrap().clone();
    assert_ne!(first.window.context().session(), other.window.context().session());
    assert_eq!(first.submission, other.submission);
    let peer_track = other.window.track(other.submission.track_id).unwrap();
    let peer_status = service.scene.track_status(peer_track).unwrap();
    let peer_runtime = live_sessions(&service)
        .into_iter()
        .find(|s| s.identity == other.window.context().session())
        .unwrap();
    let owner_runtime = live_sessions(&service)
        .into_iter()
        .find(|s| s.identity == first.window.context().session())
        .unwrap();
    let mut renderer =
        SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 64), true).unwrap();
    renderer.set_vivid_scene(service.scene());
    let size = SizeInfo::new(8192., 8192., 1., 1., 0., 0., false);
    let (original, generation) = completed_overlay_image(&mut renderer, &size, None);
    present(&mut renderer, &original);
    let image = pixels(&renderer);
    assert_eq!(at(&image, 1), [255, 0, 0, 255]);
    assert_eq!(at(&image, 61), [0, 0, 255, 255]);
    assert_eq!(initial.wait(Duration::from_secs(2)).unwrap(), Some(PresentationOutcome::Presented));
    assert_eq!(
        peer_initial.wait(Duration::from_secs(2)).unwrap(),
        Some(PresentationOutcome::Presented)
    );
    let before = renderer.overlay_metrics();
    let bad = window.submit(canvas(2048, 0xff000080)).unwrap();
    let good = neighbor.submit(canvas(1, 0x00ff00ff)).unwrap();
    accepted(&window, bad.revision());
    accepted(&neighbor, good.revision());
    let peer_compiled = lock(&service.shared.overlays)
        .drawing(&service.scene)
        .into_iter()
        .find(|d| d.window == other.window)
        .unwrap()
        .compiled;
    let until = Instant::now() + Duration::from_secs(20);
    let mut retained_frames = 0;
    let mut advanced: Option<vivid_sdk::OverlaySubmission> = None;
    let mut sampled = std::collections::BTreeSet::new();
    let mut latest = peer_initial.revision();
    let current_generation = loop {
        let media = renderer
            .prepare_media(&size, 0, &Default::default())
            .expect("peer overlay went absent");
        let image = media.overlay.expect("checked peer image disappeared");
        assert!(renderer.overlay_allocations_for_test() <= 2);
        if media.image_generation == generation {
            assert_eq!(image.data.id(), original.data.id());
        }
        if let Some(receipt) = &advanced
            && !sampled.contains(&receipt.revision())
        {
            assert_eq!(
                receipt.wait(Duration::ZERO).unwrap(),
                None,
                "newest original receipt cannot complete before its first sample"
            );
        }
        // A changed image can be a peer handoff while the bad source remains.
        // Match receipts to the actual imported pixels, not to generation alone.
        present(&mut renderer, &image);
        let pixels = pixels(&renderer);
        let revision = match at(&pixels, 61) {
            [0, 0, 255, 255] => peer_initial.revision(),
            [0, 255, 0, 255] => {
                assert_eq!(
                    good.wait(Duration::from_secs(3)).unwrap(),
                    Some(PresentationOutcome::Presented)
                );
                good.revision()
            },
            [255, 255, 0, 255] => {
                let receipt =
                    advanced.as_ref().expect("yellow must match the original newer submission");
                assert_eq!(
                    receipt.wait(Duration::from_secs(3)).unwrap(),
                    Some(PresentationOutcome::Presented)
                );
                receipt.revision()
            },
            other => panic!("unrecognized peer pixels {other:?}"),
        };
        assert!(revision >= latest, "peer image regressed to an older original submission");
        latest = revision;
        sampled.insert(revision);
        assert_eq!(neighbor.reconcile().unwrap().presented_revision, revision);
        let source_live = lock(&service.shared.overlays)
            .drawing(&service.scene)
            .iter()
            .any(|drawing| drawing.window == first.window);
        if source_live {
            assert_eq!(at(&pixels, 1), [255, 0, 0, 255], "only committed source pixels may remain");
            assert_ne!(bad.wait(Duration::ZERO).unwrap(), Some(PresentationOutcome::Presented));
            retained_frames += 1;
        } else {
            assert_eq!(at(&pixels, 1), [255; 4]);
        }
        if advance_peer && advanced.is_none() && renderer.overlay_diagnosing_for_test() {
            let receipt = neighbor.submit(canvas(1, 0xffff00ff)).unwrap();
            accepted(&neighbor, receipt.revision());
            assert_eq!(receipt.wait(Duration::ZERO).unwrap(), None);
            advanced = Some(receipt);
        }
        let final_revision = advanced.as_ref().map_or(good.revision(), |r| r.revision());
        if !source_live && revision == final_revision {
            break media.image_generation;
        }
        assert!(Instant::now() < until, "authenticated interleaved refusal stalled");
        thread::sleep(Duration::from_millis(1));
    };
    assert!(retained_frames > 0);
    assert_eq!(advanced.is_some(), advance_peer, "newer media must arrive during actual diagnosis");
    assert_eq!(bad.wait(Duration::from_secs(3)).unwrap(), Some(PresentationOutcome::Superseded));
    assert!(window.reconcile().is_err());
    // Protocol 1.5 permits a newer accepted scene to supersede an unseen one
    // after its failed reservation is abandoned. It can never relabel pixels.
    let good_outcome = if sampled.contains(&good.revision()) {
        PresentationOutcome::Presented
    } else {
        assert!(advance_peer);
        PresentationOutcome::Superseded
    };
    assert_eq!(good.wait(Duration::from_secs(3)).unwrap(), Some(good_outcome));
    if let Some(receipt) = &advanced {
        assert!(sampled.contains(&receipt.revision()));
        assert_eq!(receipt.wait(Duration::ZERO).unwrap(), Some(PresentationOutcome::Presented));
    }
    let after = renderer.overlay_metrics();
    assert!((128..=255).contains(&(after.render_passes - before.render_passes)));
    if !advance_peer {
        let current = lock(&service.shared.overlays).drawing(&service.scene);
        assert_eq!(current.len(), 1);
        assert!(std::sync::Arc::ptr_eq(&current[0].compiled, &peer_compiled));
    }
    assert_eq!(
        renderer.prepare_media(&size, 0, &Default::default()).unwrap().image_generation,
        current_generation
    );
    assert!(live_sessions(&service).iter().any(|s| std::sync::Arc::ptr_eq(s, &peer_runtime)));
    assert!(live_sessions(&service).iter().any(|s| std::sync::Arc::ptr_eq(s, &owner_runtime)));
    let status = service.scene.track_status(peer_track).unwrap();
    assert_eq!(status.state.channel_generation, peer_status.state.channel_generation);
    assert_eq!(status.state.media_epoch, peer_status.state.media_epoch);

    neighbor.request_focus().unwrap();
    let focused = neighbor.submit(canvas(1, 0xff00ffff)).unwrap();
    accepted(&neighbor, focused.revision());
    let generation =
        renderer.prepare_media(&size, 0, &Default::default()).unwrap().image_generation;
    let (image, generation) = completed_overlay_image(&mut renderer, &size, Some(generation));
    assert_eq!(focused.wait(Duration::ZERO).unwrap(), None);
    present(&mut renderer, &image);
    assert_eq!(at(&pixels(&renderer), 61), [255, 0, 255, 255]);
    assert_eq!(focused.wait(Duration::from_secs(3)).unwrap(), Some(PresentationOutcome::Presented));
    service.overlay_focus(true);
    assert!(service.overlay_keyboard(Event::Text("authenticated Ω".into()), false));
    text(&peer, &neighbor, focused.revision(), "authenticated Ω");

    // A second real sampling of cached output must not emit another terminal
    // outcome. Ordered input supplies a barrier on the original peer lane.
    let media = renderer.prepare_media(&size, 0, &Default::default()).unwrap();
    assert_eq!(media.image_generation, generation);
    present(&mut renderer, &media.overlay.unwrap());
    assert!(service.overlay_keyboard(Event::Text("cached Ω".into()), false));
    text(&peer, &neighbor, focused.revision(), "cached Ω");
    assert_eq!(good.wait(Duration::ZERO).unwrap(), Some(good_outcome));

    // Refusal closes one window, not its authenticated control session. Create
    // fresh content under that same runtime rather than silently reconnecting.
    let replacement = owner
        .create_window(OverlayWindowOptions::new(
            Rect::new(512., 0., 64., 64.).unwrap(),
            WindowMode::Floating,
        ))
        .unwrap();
    let replacement_receipt = replacement.submit(canvas(1, 0x00ffffff)).unwrap();
    accepted(&replacement, replacement_receipt.revision());
    let (image, _) = completed_overlay_image(&mut renderer, &size, Some(generation));
    present(&mut renderer, &image);
    assert_eq!(
        replacement_receipt.wait(Duration::from_secs(3)).unwrap(),
        Some(PresentationOutcome::Presented)
    );
    let image = pixels(&renderer);
    assert_eq!(at(&image, 33), [0, 255, 255, 255]);
    assert_eq!(at(&image, 61), [255, 0, 255, 255]);
    assert!(live_sessions(&service).iter().any(|s| std::sync::Arc::ptr_eq(s, &owner_runtime)));
    replacement.close().unwrap();
    neighbor.close().unwrap();
    owner.close().unwrap();
    peer.close().unwrap();
    assert!(wait_until(Duration::from_secs(3), || live_sessions(&service).is_empty()));
    renderer.set_vivid_scene(service.scene());
    let until = Instant::now() + Duration::from_secs(3);
    while renderer.overlay_allocations_for_test() != 0 {
        let _ = pixels(&renderer);
        assert!(Instant::now() < until, "shared export charges did not retire");
        thread::sleep(Duration::from_millis(1));
    }
    eprintln!(
        "Authenticated native refusal: newer_peer={advance_peer}, original_bad=Superseded, original_peer={good_outcome:?}, sampled_peer_revisions={sampled:?}, retained_primary_frames={retained_frames}, checked_attempts={}, exact scene Unicode and cached barrier, same bad-owner runtime created fresh cyan output, live_sessions=0, exports=0",
        after.render_passes - before.render_passes
    );
}
