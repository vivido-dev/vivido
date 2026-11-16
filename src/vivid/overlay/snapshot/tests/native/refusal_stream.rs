//! Actual checked peer imports while an isolated source probe remains outstanding.

// Rust guideline compliant 2026-10-07
use super::refusal::{at, candidate, present};
use super::*;
use vivid_protocol::overlay::wire::PresentationOutcome;
use vivid_protocol::overlay::{Event, WindowMode, WindowOptions};
use vivid_protocol::vector::Rect;

#[test]
#[ignore = "requires native GPU; eight checked peer imports during bounded shared-device diagnosis"]
fn native_shared_device_peer_stream_advances_during_actual_work_diagnosis() {
    let (mut f, mut renderer, size, _original, mut generation) = setup();
    let [first, peer] = f.surfaces;
    let scene = f.scene.clone();
    f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
    candidate(&mut f, 0, 3, 2048, 0xff000080);
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let until = Instant::now() + Duration::from_secs(25);
    loop {
        renew(&scene, first, peer);
        let media =
            renderer.prepare_media(&size, 0, &Default::default()).expect("retained initial output");
        present(&mut renderer, &media.overlay.unwrap());
        if renderer.overlay_diagnosing_for_test() {
            break;
        }
        assert!(Instant::now() < until, "actual mixed failure did not enter diagnosis");
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut during = Vec::new();
    let mut generations = Vec::new();
    for revision in 3..=10 {
        let color = ((revision as u32 * 16) << 24) | (192 << 16) | (80 << 8) | 255;
        f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
        candidate(&mut f, 1, revision, 1, color);
        *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
        let until = Instant::now() + Duration::from_secs(25);
        let image = loop {
            renew(&scene, first, peer);
            let media = renderer
                .prepare_media(&size, 0, &Default::default())
                .expect("peer cannot disappear during diagnosis");
            let image = media.overlay.expect("checked peer remains visible");
            assert!(renderer.overlay_allocations_for_test() <= 2);
            if media.image_generation != generation {
                generation = media.image_generation;
                let host = scene.overlays().lock().unwrap();
                assert!(
                    host.pending
                        .iter()
                        .any(|(owner, s)| *owner == peer.context().session()
                            && s.revision == revision),
                    "ready image cannot settle its receipt"
                );
                break image;
            }
            present(&mut renderer, &image);
            assert!(Instant::now() < until, "fresh peer did not advance during native diagnosis");
            std::thread::sleep(Duration::from_millis(1));
        };
        let diagnosing = renderer.overlay_diagnosing_for_test();
        present(&mut renderer, &image);
        let pixels = capture(&renderer);
        assert_eq!(at(&pixels, 61), [revision as u8 * 16, 192, 80, 255]);
        let mut host = scene.overlays().lock().unwrap();
        assert_eq!(host.displayed[&peer].revision, revision);
        if host.windows.get(first).is_some() {
            assert!(diagnosing);
            assert_eq!(at(&pixels, 1), [255, 0, 0, 255], "only committed red may be retained");
            assert_eq!(host.displayed[&first].revision, 2);
            assert!(
                host.pending
                    .iter()
                    .any(|(owner, s)| *owner == first.context().session() && s.revision == 3)
            );
            during.push(revision);
        } else {
            assert_eq!(at(&pixels, 1), [255; 4]);
        }
        host.windows.set_pane_focus(true);
        host.windows.request_focus(peer).unwrap();
        let text = format!("stream Ω {revision}");
        assert!(host.keyboard(Event::Text(text.clone()), false));
        let events: Vec<_> =
            std::iter::from_fn(|| host.windows.take_event(peer.context().session())).collect();
        assert!(events.iter().any(|e| e.window == peer
            && e.scene_revision == revision
            && e.event == Event::Text(text.clone())));
        generations.push(generation);
    }
    let until = Instant::now() + Duration::from_secs(25);
    loop {
        renew(&scene, first, peer);
        let media = renderer
            .prepare_media(&size, 0, &Default::default())
            .expect("final peer remains visible");
        present(&mut renderer, &media.overlay.unwrap());
        if scene.overlays().lock().unwrap().windows.get(first).is_none() {
            break;
        }
        assert!(Instant::now() < until, "hostile diagnosis did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
    let pixels = capture(&renderer);
    assert_eq!(at(&pixels, 1), [255; 4]);
    assert_eq!(at(&pixels, 61), [160, 192, 80, 255]);
    let (admitted, spent) = renderer.overlay_diagnostic_stream_for_test();
    assert!(
        during.len() >= 3 && admitted >= 3,
        "insufficient independent peer admission: {during:?}, worker={admitted}"
    );
    assert!(spent <= 127);
    let mut host = scene.overlays().lock().unwrap();
    assert_eq!(host.displayed[&peer].revision, 10);
    assert!(
        host.outcomes[&first.context().session()]
            .iter()
            .any(|o| o.submission.revision == 3 && o.outcome == PresentationOutcome::Superseded)
    );
    assert!(
        host.outcomes[&first.context().session()]
            .iter()
            .all(|o| o.submission.revision != 3 || o.outcome != PresentationOutcome::Presented)
    );
    for revision in 3..=10 {
        assert_eq!(
            host.outcomes[&peer.context().session()]
                .iter()
                .filter(|o| o.submission.revision == revision
                    && o.outcome == PresentationOutcome::Presented)
                .count(),
            1
        );
    }
    assert!(host.reserved.is_empty());
    host.remove_owner(first.context().session());
    host.remove_owner(peer.context().session());
    drop(host);
    renderer.set_vivid_scene(crate::vivid::scene::SharedScene::for_test());
    let until = Instant::now() + Duration::from_secs(10);
    while renderer.overlay_allocations_for_test() != 0 {
        assert!(Instant::now() < until, "shared texture exports did not retire");
        std::thread::sleep(Duration::from_millis(1));
    }
    eprintln!(
        "Shared-device stream: during={during:?}, worker_admitted={admitted}, shared_attempts={spent}, generations={generations:?}, eight exact receipts/Unicode, no absent peer and all public exports retired"
    );
}

fn renew(
    scene: &crate::vivid::scene::SharedScene,
    first: vivid_protocol::identity::SurfaceIdentity,
    peer: vivid_protocol::identity::SurfaceIdentity,
) {
    let mut host = scene.overlays().lock().unwrap();
    host.lane_open(first.context().session(), 1);
    host.lane_open(peer.context().session(), 1);
}

fn setup() -> (Fixture, SceneRenderer, SizeInfo, vello::peniko::ImageData, u64) {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    f.host.update_viewport(8192., 8192., 8.).unwrap();
    for (surface, bounds) in [
        (first, Rect::new(0., 0., 960., 1024.).unwrap()),
        (peer, Rect::new(960., 0., 64., 64.).unwrap()),
    ] {
        f.host.set_window(surface, 1, 1, WindowOptions::new(bounds, WindowMode::Floating)).unwrap();
    }
    candidate(&mut f, 0, 2, 1, 0xff0000ff);
    candidate(&mut f, 1, 2, 1, 0x0000ffff);
    let scene = f.scene.clone();
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let size = SizeInfo::new(8192., 8192., 1., 1., 0., 0., false);
    let mut renderer =
        SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 64), true).unwrap();
    renderer.set_vivid_scene(scene.clone());
    let until = Instant::now() + Duration::from_secs(25);
    let (original, generation) = loop {
        renew(&scene, first, peer);
        if let Some(media) = renderer.prepare_media(&size, 0, &Default::default())
            && let Some(image) = media.overlay
        {
            break (image, media.image_generation);
        }
        assert!(Instant::now() < until, "initial native stream output absent");
        std::thread::sleep(Duration::from_millis(1));
    };
    present(&mut renderer, &original);
    let original_pixels = capture(&renderer);
    assert_eq!(at(&original_pixels, 1), [255, 0, 0, 255]);
    assert_eq!(at(&original_pixels, 61), [0, 0, 255, 255]);
    (f, renderer, size, original, generation)
}

#[test]
#[ignore = "requires native GPU; late private proof after admitted peer and healthy source replacement"]
fn native_shared_device_late_probe_cannot_close_new_content_after_peer_admission() {
    let (mut f, mut renderer, size, _original, generation) = setup();
    let [first, peer] = f.surfaces;
    let scene = f.scene.clone();
    f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
    candidate(&mut f, 0, 3, 2048, 0xff000080);
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let until = Instant::now() + Duration::from_secs(25);
    while !renderer.overlay_diagnosing_for_test() {
        renew(&scene, first, peer);
        let media = renderer.prepare_media(&size, 0, &Default::default()).unwrap();
        present(&mut renderer, &media.overlay.unwrap());
        assert!(Instant::now() < until, "late-proof fixture never entered actual diagnosis");
        std::thread::sleep(Duration::from_millis(1));
    }
    f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
    candidate(&mut f, 1, 3, 1, 0x00ff00ff);
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let until = Instant::now() + Duration::from_secs(25);
    loop {
        renew(&scene, first, peer);
        let media = renderer.prepare_media(&size, 0, &Default::default()).unwrap();
        let image = media.overlay.unwrap();
        present(&mut renderer, &image);
        if media.image_generation != generation {
            assert!(
                renderer.overlay_diagnosing_for_test(),
                "peer must admit before private proof completes"
            );
            let pixels = capture(&renderer);
            assert_eq!(at(&pixels, 1), [255, 0, 0, 255]);
            assert_eq!(at(&pixels, 61), [0, 255, 0, 255]);
            break;
        }
        assert!(Instant::now() < until, "peer did not admit before source replacement");
        std::thread::sleep(Duration::from_millis(1));
    }
    f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
    candidate(&mut f, 0, 4, 1, 0x00ffffff);
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let until = Instant::now() + Duration::from_secs(25);
    loop {
        renew(&scene, first, peer);
        let media = renderer
            .prepare_media(&size, 0, &Default::default())
            .expect("healthy peer cannot blank after late proof");
        present(&mut renderer, &media.overlay.unwrap());
        let host = scene.overlays().lock().unwrap();
        assert!(
            host.windows.get(first).is_some() && host.windows.get(peer).is_some(),
            "old proof closed a replacement or peer"
        );
        if host.displayed[&first].revision == 4 {
            assert_eq!(host.displayed[&peer].revision, 3);
            assert!(host.outcomes[&first.context().session()].iter().any(
                |o| o.submission.revision == 3 && o.outcome == PresentationOutcome::Superseded
            ));
            assert_eq!(
                host.outcomes[&peer.context().session()]
                    .iter()
                    .filter(|o| o.submission.revision == 3
                        && o.outcome == PresentationOutcome::Presented)
                    .count(),
                1
            );
            assert!(host.reserved.is_empty());
            break;
        }
        assert!(
            Instant::now() < until,
            "healthy source did not recover after exact late-proof rejection"
        );
        drop(host);
        std::thread::sleep(Duration::from_millis(1));
    }
    let pixels = capture(&renderer);
    assert_eq!(at(&pixels, 1), [0, 255, 255, 255]);
    assert_eq!(at(&pixels, 61), [0, 255, 0, 255]);
    renderer.set_vivid_scene(crate::vivid::scene::SharedScene::for_test());
    let until = Instant::now() + Duration::from_secs(10);
    while renderer.overlay_allocations_for_test() != 0 {
        assert!(Instant::now() < until, "late-proof exports did not retire");
        std::thread::sleep(Duration::from_millis(1));
    }
    eprintln!(
        "Late shared-device proof: original hostile revision three superseded, healthy revision four stays open/presents cyan, admitted same-ID peer three remains green with one exact outcome"
    );
}
