//! Actual elapsed consumer timeout with independent primary GPU progress.

// Rust guideline compliant 2026-10-07
use super::*;
use vivid_protocol::overlay::wire::PresentationOutcome;

#[test]
#[ignore = "requires native GPU and ten elapsed seconds; stalled consumer deadline and peer recovery"]
fn wall_clock_timeout_retains_committed_owners_and_recovers_fresh_scenes() {
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
    let original_pixels = capture(&renderer);
    assert_eq!(pixel(&original_pixels, 25), [255, 0, 0, 255]);
    assert_eq!(pixel(&original_pixels, 45), [0, 0, 255, 255]);
    let peer_status = scene.track_status(peer.track(2).unwrap()).unwrap();
    renderer.hold_overlay_for_test(true);
    let start = Instant::now();
    submit(&mut f, first, 2);
    held(&mut renderer, &size);
    let mut frames = 0;
    let mut verified_readbacks = 0;
    let mut last_readback = Instant::now();
    loop {
        // This component fixture has no service actor or SDK renewal thread.
        // Renew both modeled lanes while the genuine consumer clock advances.
        {
            let mut host = scene.overlays().lock().unwrap();
            host.lane_open(first.context().session(), 1);
            host.lane_open(peer.context().session(), 1);
        }
        let media = renderer.prepare_media(&size, 0, &Default::default()).unwrap();
        assert_eq!(media.image_generation, generation);
        let mut frame = Scene::new();
        frame
            .draw_image(&media.overlay.expect("committed peer cannot disappear"), Affine::IDENTITY);
        assert!(renderer.render(&frame, Color::WHITE).unwrap());
        frames += 1;
        if last_readback.elapsed() >= Duration::from_millis(500) {
            let pixels = capture(&renderer);
            assert_eq!(pixel(&pixels, 25), [255, 0, 0, 255]);
            assert_eq!(pixel(&pixels, 45), [0, 0, 255, 255]);
            verified_readbacks += 1;
            last_readback = Instant::now();
        }
        if !renderer.needs_overlay_frame() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(14), "real consumer timeout did not retire");
        std::thread::sleep(Duration::from_millis(10));
    }
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_secs(10));
    assert!(frames >= 64);
    assert!(verified_readbacks >= 8, "primary pixels must complete throughout the actual stall");
    let current = capture(&renderer);
    assert_eq!(pixel(&current, 25), [255, 0, 0, 255]);
    assert_eq!(pixel(&current, 45), [0, 0, 255, 255]);
    {
        let host = scene.overlays().lock().unwrap();
        assert_eq!(host.displayed[&first].revision, 1);
        assert_eq!(host.displayed[&peer].revision, 1);
        assert!(host.windows.get(first).is_some() && host.windows.get(peer).is_some());
        assert!(host.reserved.is_empty());
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| *owner == first.context().session() && s.revision == 2)
        );
        assert!(
            host.outcomes[&first.context().session()].iter().all(|o| o.submission.revision != 2)
        );
        assert_eq!(host.outcomes[&peer.context().session()].len(), 1);
    }
    let after = scene.track_status(peer.track(2).unwrap()).unwrap();
    assert_eq!(after.state.channel_generation, peer_status.state.channel_generation);
    assert_eq!(after.state.media_epoch, peer_status.state.media_epoch);
    renderer.hold_overlay_for_test(false);
    submit(&mut f, first, 3);
    submit(&mut f, peer, 2);
    let (fresh, _) = ready(&mut renderer, &size, Some(generation));
    let mut frame = Scene::new();
    frame.draw_image(&fresh, Affine::IDENTITY);
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
    let current = capture(&renderer);
    assert_eq!(pixel(&current, 25), [0, 255, 0, 255]);
    assert_eq!(pixel(&current, 45), [0, 0, 255, 255]);
    let mut host = scene.overlays().lock().unwrap();
    assert_eq!(host.displayed[&first].revision, 3);
    assert_eq!(host.displayed[&peer].revision, 2);
    for (surface, revision, outcome) in [
        (first, 2, PresentationOutcome::Superseded),
        (first, 3, PresentationOutcome::Presented),
        (peer, 2, PresentationOutcome::Presented),
    ] {
        assert_eq!(
            host.outcomes[&surface.context().session()]
                .iter()
                .filter(|o| o.submission.revision == revision && o.outcome == outcome)
                .count(),
            1
        );
    }
    host.remove_owner(first.context().session());
    host.remove_owner(peer.context().session());
    drop(host);
    renderer.set_vivid_scene(crate::vivid::scene::SharedScene::for_test());
    let until = Instant::now() + Duration::from_secs(3);
    while renderer.overlay_allocations_for_test() != 0 {
        let _ = capture(&renderer);
        assert!(Instant::now() < until, "timed-out exports did not retire");
        std::thread::sleep(Duration::from_millis(1));
    }
    eprintln!(
        "Actual consumer timeout: elapsed_ms={}, independent_primary_frames={frames}, completed_primary_readbacks={verified_readbacks}, original red/blue retained, no fabricated outcome/source close, same-ID peer channel/epoch intact, fresh green/blue original outcomes once and exports=0",
        elapsed.as_millis()
    );
}
