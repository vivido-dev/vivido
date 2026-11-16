//! Original authenticated receipts and input through an eight-frame native handoff.

// Rust guideline compliant 2026-10-07
use super::*;

#[test]
#[ignore = "requires native GPU and sockets; eight authenticated peer images during private diagnosis"]
fn authenticated_peer_stream_advances_during_private_hostile_work_diagnosis() {
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
    neighbor.request_focus().unwrap();
    service.overlay_focus(true);
    let initial = window.submit(canvas(1, 0xff0000ff)).unwrap();
    let peer_initial = neighbor.submit(canvas(1, 0x0000ffff)).unwrap();
    accepted(&window, initial.revision());
    accepted(&neighbor, peer_initial.revision());
    let drawings = lock(&service.shared.overlays).drawing(&service.scene);
    assert_eq!(drawings.len(), 2);
    let first = drawings.iter().find(|d| d.bounds.origin.x.get() == 0.).unwrap();
    let other = drawings.iter().find(|d| d.bounds.origin.x.get() == 960.).unwrap();
    assert_ne!(first.window.context().session(), other.window.context().session());
    assert_eq!(first.submission, other.submission);
    let peer_track = other.window.track(other.submission.track_id).unwrap();
    let peer_status = service.scene.track_status(peer_track).unwrap();
    let runtimes = live_sessions(&service);
    assert_eq!(runtimes.len(), 2);
    let mut renderer =
        SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 64), true).unwrap();
    renderer.set_vivid_scene(service.scene());
    let size = SizeInfo::new(8192., 8192., 1., 1., 0., 0., false);
    let (image, mut generation) = completed_overlay_image(&mut renderer, &size, None);
    present(&mut renderer, &image);
    let image = pixels(&renderer);
    assert_eq!(at(&image, 1), [255, 0, 0, 255]);
    assert_eq!(at(&image, 61), [0, 0, 255, 255]);
    for receipt in [&initial, &peer_initial] {
        assert_eq!(
            receipt.wait(Duration::from_secs(3)).unwrap(),
            Some(PresentationOutcome::Presented)
        );
    }
    let bad = window.submit(canvas(2048, 0xff000080)).unwrap();
    accepted(&window, bad.revision());
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let media = renderer
            .prepare_media(&size, 0, &Default::default())
            .expect("initial peer remains visible");
        assert_eq!(media.image_generation, generation);
        present(&mut renderer, &media.overlay.unwrap());
        if renderer.overlay_diagnosing_for_test() {
            break;
        }
        assert!(Instant::now() < until, "actual authenticated work never entered diagnosis");
        thread::sleep(Duration::from_millis(1));
    }

    let mut receipts = Vec::new();
    let mut during = Vec::new();
    for revision in 2..=9 {
        let color = ((revision as u32 * 16) << 24) | (192 << 16) | (80 << 8) | 255;
        let receipt = neighbor.submit(canvas(1, color)).unwrap();
        assert_eq!(receipt.revision(), revision);
        accepted(&neighbor, revision);
        let until = Instant::now() + Duration::from_secs(3);
        let image = loop {
            assert_eq!(
                receipt.wait(Duration::ZERO).unwrap(),
                None,
                "only exact primary sampling may admit this original receipt"
            );
            let media = renderer
                .prepare_media(&size, 0, &Default::default())
                .expect("peer stream cannot disappear");
            let image = media.overlay.expect("checked peer stream remains visible");
            assert!(renderer.overlay_allocations_for_test() <= 2);
            if media.image_generation != generation {
                generation = media.image_generation;
                break image;
            }
            present(&mut renderer, &image);
            assert!(Instant::now() < until, "authenticated peer did not advance");
            thread::sleep(Duration::from_millis(1));
        };
        assert!(
            renderer.overlay_diagnosing_for_test(),
            "every peer must arrive during actual private diagnosis"
        );
        assert_eq!(bad.wait(Duration::ZERO).unwrap(), None);
        present(&mut renderer, &image);
        let image = pixels(&renderer);
        assert_eq!(at(&image, 1), [255, 0, 0, 255], "only committed source content may remain");
        assert_eq!(at(&image, 61), [revision as u8 * 16, 192, 80, 255]);
        assert_eq!(
            receipt.wait(Duration::from_secs(3)).unwrap(),
            Some(PresentationOutcome::Presented)
        );
        let status = neighbor.reconcile().unwrap();
        assert_eq!(status.accepted_revision, revision);
        assert_eq!(status.presented_revision, revision);
        let value = format!("authenticated stream Ω {revision}");
        assert!(service.overlay_keyboard(Event::Text(value.clone()), false));
        text(&peer, &neighbor, revision, &value);
        during.push(revision);
        receipts.push(receipt);
    }
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let media = renderer
            .prepare_media(&size, 0, &Default::default())
            .expect("final authenticated peer remains visible");
        let image = media.overlay.expect("surviving peer image is never absent");
        present(&mut renderer, &image);
        generation = media.image_generation;
        if bad.wait(Duration::ZERO).unwrap() == Some(PresentationOutcome::Superseded) {
            break;
        }
        assert!(Instant::now() < until, "authenticated source refusal did not complete");
        thread::sleep(Duration::from_millis(1));
    }
    assert!(window.reconcile().is_err());
    let image = pixels(&renderer);
    assert_eq!(at(&image, 1), [255; 4]);
    assert_eq!(at(&image, 61), [144, 192, 80, 255]);
    let (admitted, spent) = renderer.overlay_diagnostic_stream_for_test();
    assert_eq!(admitted, 8);
    assert!(spent <= 127);
    assert_eq!(during, (2..=9).collect::<Vec<_>>());
    let status = service.scene.track_status(peer_track).unwrap();
    assert_eq!(status.state.channel_generation, peer_status.state.channel_generation);
    assert_eq!(status.state.media_epoch, peer_status.state.media_epoch);
    for runtime in &runtimes {
        assert!(
            live_sessions(&service).iter().any(|current| std::sync::Arc::ptr_eq(runtime, current))
        );
    }
    // A cached sample followed by ordered input detects orphaned/duplicate
    // outcomes on the original authenticated lane, without reconnecting it.
    let media = renderer.prepare_media(&size, 0, &Default::default()).unwrap();
    assert_eq!(media.image_generation, generation);
    present(&mut renderer, &media.overlay.unwrap());
    assert!(service.overlay_keyboard(Event::Text("cached stream Ω".into()), false));
    text(&peer, &neighbor, 9, "cached stream Ω");
    for receipt in &receipts {
        assert_eq!(receipt.wait(Duration::ZERO).unwrap(), Some(PresentationOutcome::Presented));
    }
    let replacement = owner
        .create_window(OverlayWindowOptions::new(
            Rect::new(512., 0., 64., 64.).unwrap(),
            WindowMode::Floating,
        ))
        .unwrap();
    let fresh = replacement.submit(canvas(1, 0x00ffffff)).unwrap();
    accepted(&replacement, fresh.revision());
    let (image, _) = completed_overlay_image(&mut renderer, &size, Some(generation));
    assert_eq!(fresh.wait(Duration::ZERO).unwrap(), None);
    present(&mut renderer, &image);
    let image = pixels(&renderer);
    assert_eq!(at(&image, 33), [0, 255, 255, 255]);
    assert_eq!(at(&image, 61), [144, 192, 80, 255]);
    assert_eq!(fresh.wait(Duration::from_secs(3)).unwrap(), Some(PresentationOutcome::Presented));
    replacement.close().unwrap();
    neighbor.close().unwrap();
    owner.close().unwrap();
    peer.close().unwrap();
    assert!(wait_until(Duration::from_secs(3), || live_sessions(&service).is_empty()));
    renderer.set_vivid_scene(service.scene());
    let until = Instant::now() + Duration::from_secs(3);
    while renderer.overlay_allocations_for_test() != 0 {
        let _ = pixels(&renderer);
        assert!(Instant::now() < until, "authenticated stream exports did not retire");
        thread::sleep(Duration::from_millis(1));
    }
    eprintln!(
        "Authenticated shared-device stream: during={during:?}, worker_admitted={admitted}, shared_attempts={spent}, eight original receipts and exact-scene Unicode, bad Superseded, cached duplicate barrier, original runtimes/peer channel/epoch retained, fresh cyan and zero sessions/exports"
    );
}
