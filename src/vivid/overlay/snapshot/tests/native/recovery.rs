//! Pending-output lifecycle and logical-device recovery on native GPU resources.

// Rust guideline compliant 2026-10-07
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use vello::{Scene, kurbo::Affine, peniko::Color};
use vivid_protocol::overlay::wire::PresentationOutcome;
use vivid_protocol::revision::{ChannelGeneration, TargetGeneration};
use winit::dpi::PhysicalSize;

use super::{Fixture, capture, held, pixel, ready, submit};
use crate::display::{SizeInfo, renderer::SceneRenderer, window::RenderSource};
use crate::vivid::target::DisplayGeometry;

fn setup() -> (Fixture, SceneRenderer, SizeInfo) {
    let mut f = Fixture::new();
    let scene = f.scene.clone();
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let size = SizeInfo::new(64., 32., 1., 1., 0., 0., false);
    let mut renderer = SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 32), true)
        .expect("native lifecycle qualification requires a working GPU");
    renderer.set_vivid_scene(scene);
    let (original, _) = ready(&mut renderer, &size, None);
    present(&mut renderer, &original);
    (f, renderer, size)
}

fn present(renderer: &mut SceneRenderer, image: &vello::peniko::ImageData) {
    let mut frame = Scene::new();
    frame.draw_image(image, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
}

#[test]
#[ignore = "requires native GPU; qualifies pending channel, retained-session and slot replacement"]
fn rebind_and_slot_replacement_cannot_publish_the_old_pending_snapshot() {
    for mode in 0..3 {
        let (mut f, mut renderer, size) = setup();
        let [first, peer] = f.surfaces;
        let scene = f.scene.clone();
        let peer_status = scene.track_status(peer.track(2).unwrap()).unwrap();
        let peer_compiled = scene.overlays().lock().unwrap().displayed[&peer].compiled.clone();
        renderer.hold_overlay_for_test(true);
        submit(&mut f, first, 2);
        held(&mut renderer, &size);
        assert_eq!(renderer.overlay_allocations_for_test(), 2);
        let track = first.track(2).unwrap();
        if mode < 2 {
            if mode == 1 {
                scene.suspend_session(first.context().session());
            }
            scene.advance_channel(track).unwrap();
            scene.overlays().lock().unwrap().track_lost(track, ChannelGeneration::ONE);
            let next = ChannelGeneration::new(2);
            scene.accept_channel(track, next, 8192, 64).unwrap();
            submit(&mut f, first, 3);
        } else {
            f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
            f.track(first, 3);
            f.submit(first, 3, 3);
            f.activate(first, 3);
            f.host.remove_track(track);
            *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
            scene.destroy_track(track).unwrap();
        }
        renderer.prepare_media(&size, 0, &Default::default());
        {
            let host = scene.overlays().lock().unwrap();
            let outcomes = &host.outcomes[&first.context().session()];
            assert!(
                outcomes
                    .iter()
                    .any(|o| o.submission.revision == 2
                        && o.outcome == PresentationOutcome::Superseded),
                "mode {mode}"
            );
            assert!(
                !outcomes
                    .iter()
                    .any(|o| o.submission.revision == 2
                        && o.outcome == PresentationOutcome::Presented)
            );
            assert_eq!(host.displayed[&first].revision, 1, "pending output did not publish input");
            assert!(host.lanes.contains_key(&peer.context().session()));
        }
        renderer.hold_overlay_for_test(false);
        let (image, _) = ready(&mut renderer, &size, None);
        present(&mut renderer, &image);
        let pixels = capture(&renderer);
        assert_eq!(pixel(&pixels, 25), [0, 255, 0, 255]);
        assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
        assert!(renderer.overlay_allocations_for_test() <= 2);
        let host = scene.overlays().lock().unwrap();
        let displayed = &host.displayed[&first];
        assert_eq!(displayed.revision, 3);
        assert_eq!(displayed.submission.track_id, if mode == 2 { 3 } else { 2 });
        assert_eq!(displayed.submission.channel_generation, if mode == 2 { 1 } else { 2 });
        assert!(Arc::ptr_eq(&host.displayed[&peer].compiled, &peer_compiled));
        assert_eq!(host.outcomes[&peer.context().session()].len(), 1);
        let after = scene.track_status(peer.track(2).unwrap()).unwrap();
        assert_eq!(after.state.channel_generation, peer_status.state.channel_generation);
        assert_eq!(after.state.media_epoch, peer_status.state.media_epoch);
        assert_eq!(after.state.last_media_id, peer_status.state.last_media_id);
        assert_eq!(after.state.milestones, peer_status.state.milestones);
    }
}

fn resize(f: &Fixture, renderer: &mut SceneRenderer) -> SizeInfo {
    let generation = f
        .scene
        .target()
        .offer_geometry(DisplayGeometry {
            viewport_width: 97,
            viewport_height: 33,
            columns: 97,
            rows: 33,
            cell_width: 1,
            cell_height: 1,
        })
        .unwrap();
    f.scene.advance_target_generation(TargetGeneration::new(generation));
    f.scene.overlays().lock().unwrap().update_viewport(97., 33., 1.).unwrap();
    renderer.resize(PhysicalSize::new(97, 33));
    SizeInfo::new(97., 33., 1., 1., 0., 0., false)
}

#[test]
#[ignore = "requires native GPU; qualifies pending and completed output across odd-size retargeting"]
fn pending_and_ready_output_keep_original_receipts_across_resize() {
    for complete_before_resize in [false, true] {
        let (mut f, mut renderer, size) = setup();
        let [first, peer] = f.surfaces;
        let scene = f.scene.clone();
        renderer.hold_overlay_for_test(!complete_before_resize);
        submit(&mut f, first, 2);
        let old = if complete_before_resize {
            // The displayed image is still revision one. Wait for an actual
            // completed replacement without putting it in a terminal frame.
            let current_generation =
                renderer.prepare_media(&size, 0, &Default::default()).unwrap().image_generation;
            Some(ready(&mut renderer, &size, Some(current_generation)).0)
        } else {
            held(&mut renderer, &size);
            None
        };
        assert_eq!(renderer.overlay_allocations_for_test(), 2);
        let new_size = resize(&f, &mut renderer);
        if let Some(old) = &old {
            present(&mut renderer, old);
            assert_eq!(
                scene.overlays().lock().unwrap().displayed[&first].revision,
                1,
                "an old-target image cannot publish revision two hits"
            );
        } else {
            renderer.prepare_media(&new_size, 0, &Default::default());
        }
        {
            let host = scene.overlays().lock().unwrap();
            assert!(
                host.pending
                    .iter()
                    .any(|(owner, s)| *owner == first.context().session() && s.revision == 2)
            );
            assert_eq!(host.outcomes[&peer.context().session()].len(), 1);
        }
        renderer.hold_overlay_for_test(false);
        let (fresh, _) = ready(&mut renderer, &new_size, None);
        assert_eq!((fresh.width, fresh.height), (97, 33));
        if let Some(old) = &old {
            assert_ne!(old.data.id(), fresh.data.id());
        }
        present(&mut renderer, &fresh);
        let pixels = capture(&renderer);
        assert_eq!((pixels.width, pixels.height), (97, 33));
        assert_eq!(pixel(&pixels, 25), [0, 255, 0, 255]);
        assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
        assert_eq!(pixel(&pixels, 90), [255, 255, 255, 255]);
        assert!(renderer.overlay_allocations_for_test() <= 2);
        let host = scene.overlays().lock().unwrap();
        assert_eq!(host.displayed[&first].revision, 2);
        assert_eq!(
            host.outcomes[&first.context().session()]
                .iter()
                .filter(
                    |o| o.submission.revision == 2 && o.outcome == PresentationOutcome::Presented
                )
                .count(),
            1
        );
        assert_eq!(host.outcomes[&peer.context().session()].len(), 1);
    }
}

#[test]
#[ignore = "requires native GPU; qualifies logical device loss with a pending overlay and same-pool rebuild"]
fn pending_device_loss_rebuilds_without_losing_receipts_or_an_independent_renderer() {
    let (commands, requests) = mpsc::sync_channel::<Color>(1);
    let (completed, responses) = mpsc::sync_channel(1);
    let independent = std::thread::spawn(move || {
        let mut renderer =
            SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 32), true).unwrap();
        for color in requests {
            assert!(renderer.render(&Scene::new(), color).unwrap());
            let pixels = capture(&renderer);
            completed.send(pixel(&pixels, 25)).unwrap();
        }
    });
    let (mut f, mut renderer, size) = setup();
    let [first, peer] = f.surfaces;
    let scene = f.scene.clone();
    commands.send(Color::from_rgb8(10, 20, 30)).unwrap();
    assert_eq!(responses.recv_timeout(Duration::from_secs(10)).unwrap(), [10, 20, 30, 255]);
    renderer.hold_overlay_for_test(true);
    submit(&mut f, first, 2);
    held(&mut renderer, &size);
    let original_pool = renderer.overlay_pool_for_test();
    assert_eq!(original_pool.reserved(), 2, "displayed output and pending job hold both credits");
    let old_readback = renderer.begin_screenshot().unwrap();
    renderer.lose_overlay_device_for_test();
    let until = Instant::now() + Duration::from_secs(5);
    while !renderer.gpu_failed() {
        let _ = renderer.poll_screenshot(&old_readback);
        assert!(Instant::now() < until, "logical device loss was not observed");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(renderer.render(&Scene::new(), Color::WHITE).is_err());
    assert!(renderer.poll_screenshot(&old_readback).is_err());
    assert!(renderer.prepare_media(&size, 0, &Default::default()).is_none());
    {
        let host = scene.overlays().lock().unwrap();
        assert_eq!(host.displayed[&first].revision, 1);
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| *owner == first.context().session() && s.revision == 2)
        );
        assert_eq!(host.outcomes[&peer.context().session()].len(), 1);
    }
    commands.send(Color::from_rgb8(40, 50, 60)).unwrap();
    assert_eq!(responses.recv_timeout(Duration::from_secs(10)).unwrap(), [40, 50, 60, 255]);
    renderer.rebuild(RenderSource::Offscreen, PhysicalSize::new(64, 32), true).unwrap();
    assert!(!renderer.gpu_failed());
    renderer.set_vivid_scene(scene.clone());
    let (image, _) = ready(&mut renderer, &size, None);
    present(&mut renderer, &image);
    let pixels = capture(&renderer);
    assert_eq!(pixel(&pixels, 25), [0, 255, 0, 255]);
    assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
    assert_eq!(
        original_pool.reserved(),
        1,
        "the fresh output charges the original budget after old work retires"
    );
    assert_eq!(original_pool.reserved(), renderer.overlay_allocations_for_test());
    let host = scene.overlays().lock().unwrap();
    assert_eq!(host.displayed[&first].revision, 2);
    assert_eq!(
        host.outcomes[&first.context().session()]
            .iter()
            .filter(|o| o.submission.revision == 2 && o.outcome == PresentationOutcome::Presented)
            .count(),
        1
    );
    assert_eq!(host.outcomes[&peer.context().session()].len(), 1);
    drop(host);
    // A second recovered update proves that retirement did not strand a credit
    // while allowing only the first fresh output to slip through.
    let generation =
        renderer.prepare_media(&size, 0, &Default::default()).unwrap().image_generation;
    submit(&mut f, first, 3);
    let (next, _) = ready(&mut renderer, &size, Some(generation));
    present(&mut renderer, &next);
    let pixels = capture(&renderer);
    assert_eq!(pixel(&pixels, 25), [0, 255, 0, 255]);
    assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
    assert_eq!(original_pool.reserved(), 1);
    assert_eq!(original_pool.reserved(), renderer.overlay_allocations_for_test());
    assert_eq!(scene.overlays().lock().unwrap().displayed[&first].revision, 3);
    let final_readback = renderer.begin_screenshot().unwrap();
    renderer.set_vivid_scene(scene.clone());
    let until = Instant::now() + Duration::from_secs(3);
    while original_pool.reserved() != 0 {
        let _ = renderer.poll_screenshot(&final_readback);
        assert!(
            Instant::now() < until,
            "cleared recovered outputs did not release their original budget"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(renderer.overlay_allocations_for_test(), 0);
    commands.send(Color::from_rgb8(70, 80, 90)).unwrap();
    assert_eq!(responses.recv_timeout(Duration::from_secs(10)).unwrap(), [70, 80, 90, 255]);
    drop(commands);
    independent.join().unwrap();
}
