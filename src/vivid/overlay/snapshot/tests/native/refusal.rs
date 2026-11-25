//! Actual shared-device work refusal with exact same-ID source recovery.

// Rust guideline compliant 2026-10-07
use super::*;
use crate::display::text::TextSystem;
use vivid_protocol::overlay::wire::PresentationOutcome;
use vivid_protocol::overlay::{Event, WindowMode, WindowOptions};
use vivid_protocol::vector::{Brush, Canvas, Color as VectorColor, Path, Rect};

pub(super) fn candidate(f: &mut Fixture, owner: usize, revision: u64, count: usize, color: u32) {
    let surface = f.surfaces[owner];
    f.submit(surface, 2, revision);
    let mut canvas = Canvas::new();
    for _ in 0..count {
        canvas.fill(path(count), Brush::Solid(VectorColor(color))).unwrap();
    }
    let compiled = crate::display::vector::compile(
        &canvas,
        &mut TextSystem::new(Default::default()),
        &Default::default(),
    )
    .unwrap();
    let entry = f.host.scenes.get_mut(&surface.track(2).unwrap()).unwrap();
    entry.2 = Arc::new(compiled);
}

/// One full-window square, or a hostile layer for a `count`-layer stack.
///
/// A hostile layer spans the full 1024² window, so binning and tile allocation
/// still overflow and force the checked subdivision to its 64-attempt limit.
/// It is only a one-unit sliver along the diagonal, though, so each successful
/// sub-tile costs little fine rasterization, and it encodes in few bytes. Full-coverage layers made the
/// ~32 successful sub-tiles exceed the 10-second job deadline on slower GPUs,
/// which ended the job as a timeout instead of a work refusal.
pub(super) fn path(count: usize) -> Path {
    if count == 1 {
        return Path::rectangle(Rect::new(0., 0., 1024., 1024.).unwrap()).unwrap();
    }
    Path::builder()
        .move_to(0., 0.)
        .line_to(1024., 1024.)
        .line_to(1024., 1023.)
        .close()
        .build()
        .unwrap()
}

pub(super) fn present(renderer: &mut SceneRenderer, image: &vello::peniko::ImageData) {
    let mut frame = Scene::new();
    // The checked overlay is 8192². A 64² primary readback proves its imported
    // pixels without allocating a second large physical window/readback target.
    frame.draw_image(image, Affine::scale(1. / 128.));
    assert!(renderer.render(&frame, Color::WHITE).unwrap());
}

pub(super) fn at(pixels: &crate::display::renderer::ScreenshotPixels, x: usize) -> [u8; 4] {
    let offset = pixels.padded_bytes_per_row as usize + x * 4;
    pixels.bytes[offset..offset + 4].try_into().unwrap()
}

#[test]
#[ignore = "requires native GPU; actual 64-attempt shared-device refusal and nonblank peer replacement"]
fn shared_device_work_refusal_preserves_peer_pixels_and_exact_original_outcomes() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    assert_eq!(first.surface_id(), peer.surface_id());
    assert_eq!(first.context().context_id(), peer.context().context_id());
    assert_ne!(first.context().session(), peer.context().session());
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
    let (original, generation) = ready(&mut renderer, &size, None);
    present(&mut renderer, &original);
    let pixels = capture(&renderer);
    assert_eq!(at(&pixels, 1), [255, 0, 0, 255]);
    assert_eq!(at(&pixels, 61), [0, 0, 255, 255]);
    let before = renderer.overlay_metrics();
    f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
    candidate(&mut f, 0, 3, 2048, 0xff000080);
    candidate(&mut f, 1, 3, 1, 0x00ff00ff);
    let peer_compiled = f.host.scenes[&peer.track(2).unwrap()].2.clone();
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let until = Instant::now() + Duration::from_secs(20);
    let mut retained_frames = 0;
    let fresh = loop {
        // This finite fixture has no service actor. Model live input-lane
        // keepalives while the actual checked worker runs on its own thread.
        {
            let mut host = scene.overlays().lock().unwrap();
            host.lane_open(first.context().session(), 1);
            host.lane_open(peer.context().session(), 1);
        }
        let media = renderer
            .prepare_media(&size, 0, &Default::default())
            .expect("recovery cannot blank the healthy peer");
        let image = media.overlay.expect("checked peer image remains available");
        assert!(renderer.overlay_allocations_for_test() <= 2);
        if media.image_generation != generation {
            break image;
        }
        assert_eq!(image.data.id(), original.data.id());
        present(&mut renderer, &image);
        let pixels = capture(&renderer);
        assert_eq!(at(&pixels, 61), [0, 0, 255, 255]);
        let host = scene.overlays().lock().unwrap();
        assert!(host.windows.get(first).is_some(), "close waits for checked survivor output");
        assert_eq!(host.displayed[&peer].revision, 2);
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| *owner == peer.context().session() && s.revision == 3)
        );
        retained_frames += 1;
        assert!(Instant::now() < until, "source diagnosis did not finish");
        std::thread::sleep(Duration::from_millis(1));
    };
    {
        let host = scene.overlays().lock().unwrap();
        assert!(host.windows.get(first).is_none());
        assert!(host.windows.get(peer).is_some());
        assert_eq!(host.displayed[&peer].revision, 2, "ready output is not yet admitted");
        assert!(
            host.pending
                .iter()
                .any(|(owner, s)| *owner == peer.context().session() && s.revision == 3)
        );
        assert!(host.outcomes[&first.context().session()].iter().any(|o|
            o.submission.revision==3 && o.outcome==PresentationOutcome::Superseded));
    }
    present(&mut renderer, &fresh);
    let pixels = capture(&renderer);
    eprintln!(
        "Refusal replacement: original_id={}, fresh_id={}, size={}x{}, peer_presented={}, row={:?}",
        original.data.id(),
        fresh.data.id(),
        fresh.width,
        fresh.height,
        scene.overlays().lock().unwrap().displayed.get(&peer).map_or(0, |d| d.revision),
        (58..64).map(|x| at(&pixels, x)).collect::<Vec<_>>()
    );
    assert_eq!(at(&pixels, 1), [255; 4], "the refused source is absent");
    assert_eq!(at(&pixels, 61), [0, 255, 0, 255]);
    let after = renderer.overlay_metrics();
    assert!(
        (128..=255).contains(&(after.render_passes - before.render_passes)),
        "actual mixed/isolated refusal and replacement stay within their checked bounds"
    );
    let mut host = scene.overlays().lock().unwrap();
    assert_eq!(host.displayed[&peer].revision, 3);
    assert!(Arc::ptr_eq(&host.displayed[&peer].compiled, &peer_compiled));
    assert_eq!(
        host.outcomes[&peer.context().session()]
            .iter()
            .filter(|o| o.submission.revision == 3 && o.outcome == PresentationOutcome::Presented)
            .count(),
        1
    );
    host.windows.set_pane_focus(true);
    host.windows.request_focus(peer).unwrap();
    assert!(host.keyboard(Event::Text("survivor Ω".into()), false));
    let mut delivered = false;
    while let Some(event) = host.windows.take_event(peer.context().session()) {
        if let Event::Text(text) = event.event {
            assert_eq!(text, "survivor Ω");
            assert_eq!(event.window, peer);
            assert_eq!(event.scene_revision, 3);
            delivered = true;
        }
    }
    assert!(delivered);
    drop(host);
    let generation =
        renderer.prepare_media(&size, 0, &Default::default()).unwrap().image_generation;
    f.host = std::mem::take(&mut *scene.overlays().lock().unwrap());
    candidate(&mut f, 1, 4, 1, 0xff00ffff);
    *scene.overlays().lock().unwrap() = std::mem::take(&mut f.host);
    let (next, _) = ready(&mut renderer, &size, Some(generation));
    present(&mut renderer, &next);
    let pixels = capture(&renderer);
    assert_eq!(at(&pixels, 1), [255; 4]);
    assert_eq!(at(&pixels, 61), [255, 0, 255, 255]);
    assert_eq!(scene.overlays().lock().unwrap().displayed[&peer].revision, 4);
    assert_eq!(renderer.overlay_allocations_for_test(), 1);
    eprintln!(
        "Shared-device native refusal: retained_primary_frames={retained_frames}, checked_attempts={}, pool={}, original peer revision/input=3, red removed and green peer admitted without an absent overlay",
        after.render_passes - before.render_passes,
        renderer.overlay_allocations_for_test()
    );
}
