//! Authenticated producer recovery while checked native output remains pending.

// Rust guideline compliant 2026-10-07
use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use vello::{Scene, kurbo::Affine, peniko::Color as GpuColor};
use vivid_protocol::{auth::Secret32, registry};
use vivid_sdk::overlay::{
    Brush, Canvas, Color, Command, Event, HitRole, OverlayWindowOptions, Path, Rect, WindowMode,
};
use vivid_sdk::{OverlaySession, ProducerAuthentication, Session};
use winit::dpi::PhysicalSize;

use super::{
    completed_overlay_image, live_sessions, lock, resumable_definition, test_config, test_geometry,
    wait_until,
};
use crate::display::{SizeInfo, renderer::SceneRenderer, window::RenderSource};
use crate::vivid::VividService;

#[test]
fn resume_wait_rejects_invalid_proofs_and_never_takes_over_a_healthy_control() {
    use vivid_protocol::messages::{Hello, HelloAuthentication};
    use vivid_protocol::wire::{ConnectionKind, encode_preface};
    let service = VividService::start_with_wake(test_geometry(), Arc::new(|_| {})).unwrap();
    let mut controller = super::connect(&service);
    let context = controller.info().root_context_id;
    let secret = Secret32::new([0x6c; 32]);
    controller
        .create_session_lease(&resumable_definition(context, 52, &secret), &Default::default())
        .unwrap();
    let mut child = super::RawClient::activate(&service, context, 52, &secret).unwrap();
    child.query_session().unwrap();
    let mut required_profiles =
        vec![registry::CORE_CONTROL.into(), registry::TERMINAL_SURFACE.into()];
    required_profiles.sort();
    let mut hello = Hello {
        producer_name: "retirement-check".into(),
        producer_version: "0".into(),
        required_profiles,
        optional_profiles: Vec::new(),
        maximum_control_body: vivid_protocol::CONTROL_MAX_RECORD_BODY,
        client_nonce: [0x3c; 32],
        authentication: HelloAuthentication::Resume {
            context_id: context,
            lease_id: 52,
            session_id: child.session_id,
            resume_generation: 0,
            attempt_id: [0x6b; 16],
            proof: [0; 32],
        },
        target_profile: registry::TERMINAL_SURFACE.into(),
        extensions: Vec::new(),
    };
    let preface = encode_preface(ConnectionKind::Control, vivid_protocol::CONTROL_MAX_RECORD_BODY);
    let sign = |hello: &mut Hello, bytes| {
        hello.authenticate_resume(&vivid_protocol::auth::ResumeKey::new(bytes), &preface).unwrap()
    };
    sign(&mut hello, [0; 32]);
    let start = Instant::now();
    super::resume_wait::retire(&service.shared, &hello, &preface, start + Duration::from_secs(1))
        .unwrap();
    assert!(start.elapsed() < Duration::from_millis(250));
    sign(&mut hello, *child.resume_key.expose());
    let start = Instant::now();
    let deadline = start + Duration::from_millis(40);
    let error =
        super::resume_wait::retire(&service.shared, &hello, &preface, deadline).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(Instant::now() >= deadline);
    assert!(start.elapsed() < Duration::from_secs(1));
    child.query_session().unwrap();
    controller.query_session().unwrap();
    let runtime = live_sessions(&service)
        .into_iter()
        .find(|s| s.identity.session_id() == child.session_id)
        .unwrap();
    assert_eq!(runtime.resume_generation, 0);
    assert!(!runtime.connections.stopped());
}

fn canvas(color: u32) -> Canvas {
    let mut canvas = Canvas::new();
    let bounds = Rect::new(0., 0., 16., 16.).unwrap();
    canvas.fill(Path::rectangle(bounds).unwrap(), Brush::Solid(Color(color))).unwrap();
    canvas
        .push(Command::Hit {
            id: 7,
            path: Path::rectangle(bounds).unwrap(),
            role: HitRole::Input,
            cursor: None,
        })
        .unwrap();
    canvas
}

#[test]
#[ignore = "requires native GPU and sockets; qualifies authenticated overlay resume after witnessed control retirement"]
fn leased_owner_resumes_pending_output_without_replacing_its_same_id_peer() {
    resume_pending(false);
}

#[test]
#[ignore = "requires native GPU and sockets; qualifies immediate authenticated resume during control retirement"]
fn immediate_resume_waits_for_retirement_while_the_peer_remains_responsive() {
    resume_pending(true);
}

fn resume_pending(immediate: bool) {
    let _gpu = crate::display::renderer::gpu_test_lock();
    let service = VividService::start_with_wake(test_geometry(), Arc::new(|_| {})).unwrap();
    service.update_overlay_viewport(64., 32., 1.);
    let mut config = test_config(&service);
    config.required_profiles.extend(
        [
            registry::LIVE_MEDIA,
            registry::TERMINAL_OVERLAY,
            registry::VECTOR_SCENE,
            registry::OVERLAY_INPUT,
        ]
        .into_iter()
        .map(str::to_owned),
    );
    config.required_profiles.sort();
    config.required_profiles.dedup();
    config.optional_profiles.retain(|p| !config.required_profiles.contains(p));
    let mut controller = Session::connect(config).unwrap();
    let context = controller.info().root_context_id;
    let secret = Secret32::new([0x6a; 32]);
    let mut lease = resumable_definition(context, 51, &secret);
    lease.permitted_profiles = controller.info().accepted_profiles.clone();
    controller.create_session_lease(&lease, &Default::default()).unwrap();
    let mut config = test_config(&service);
    config.authentication =
        ProducerAuthentication::lease_activation_bytes(context, 51, secret).unwrap();
    let mut owner = OverlaySession::connect(config).unwrap();
    let peer = OverlaySession::connect(test_config(&service)).unwrap();
    let mut window = owner
        .create_window(OverlayWindowOptions::new(
            Rect::new(0., 0., 16., 16.).unwrap(),
            WindowMode::Floating,
        ))
        .unwrap();
    let neighbor = peer
        .create_window(OverlayWindowOptions::new(
            Rect::new(32., 0., 16., 16.).unwrap(),
            WindowMode::Floating,
        ))
        .unwrap();
    let initial = window.submit(canvas(0xff0000ff)).unwrap();
    let peer_initial = neighbor.submit(canvas(0x0000ffff)).unwrap();
    assert!(wait_until(Duration::from_secs(3), || lock(&service.shared.overlays)
        .drawing(&service.scene)
        .len()
        == 2));
    let drawings = lock(&service.shared.overlays).drawing(&service.scene);
    let first = drawings.iter().find(|d| d.bounds.origin.x.get() == 0.).unwrap().clone();
    let other = drawings.iter().find(|d| d.bounds.origin.x.get() == 32.).unwrap().clone();
    assert_ne!(first.window.context().session(), other.window.context().session());
    assert_eq!(first.submission.address, other.submission.address);
    assert_eq!(first.submission.track_id, other.submission.track_id);
    let peer_status =
        service.scene.track_status(other.window.track(other.submission.track_id).unwrap()).unwrap();
    let mut renderer =
        SceneRenderer::new(RenderSource::Offscreen, PhysicalSize::new(64, 32), true).unwrap();
    renderer.set_vivid_scene(service.scene());
    let size = SizeInfo::new(64., 32., 1., 1., 0., 0., false);
    let (image, generation) = completed_overlay_image(&mut renderer, &size, None);
    let mut frame = Scene::new();
    frame.draw_image(&image, Affine::IDENTITY);
    assert!(renderer.render(&frame, GpuColor::WHITE).unwrap());
    assert_eq!(
        initial.wait(Duration::from_secs(2)).unwrap(),
        Some(vivid_sdk::overlay::PresentationOutcome::Presented)
    );
    assert_eq!(
        peer_initial.wait(Duration::from_secs(2)).unwrap(),
        Some(vivid_sdk::overlay::PresentationOutcome::Presented)
    );
    renderer.hold_overlay_for_test(true);
    let old = window.submit(canvas(0x00ff00ff)).unwrap();
    assert!(
        wait_until(Duration::from_secs(3), || window.reconcile().unwrap().accepted_revision == 2)
    );
    let until = Instant::now() + Duration::from_secs(3);
    while !renderer.overlay_held_for_test() {
        renderer.prepare_media(&size, 0, &Default::default());
        assert!(Instant::now() < until, "overlay worker did not reach pending validation");
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(old.wait(Duration::ZERO).unwrap(), None);
    let retired = live_sessions(&service)
        .into_iter()
        .find(|s| s.identity == first.window.context().session())
        .unwrap();
    let retired_writer = lock(&retired.lane_writer).as_ref().unwrap().clone();
    if immediate {
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        *lock(&retired.retirement_gate) = Some(release.clone());
        let config = test_config(&service);
        let reconnect = thread::spawn(move || {
            owner.resume(config, &mut [&mut window]).unwrap();
            (owner, window)
        });
        assert!(wait_until(Duration::from_secs(3), || service
            .shared
            .waiting_resumes
            .load(std::sync::atomic::Ordering::Acquire)
            == 1));
        assert!(live_sessions(&service).iter().any(|s| Arc::ptr_eq(s, &retired)));
        assert_eq!(neighbor.reconcile().unwrap().presented_revision, 1);
        release.store(true, std::sync::atomic::Ordering::Release);
        (owner, window) = reconnect.join().unwrap();
    } else {
        (owner.cancel_handle().unwrap())();
        assert!(wait_until(Duration::from_secs(3), || !live_sessions(&service)
            .iter()
            .any(|s| s.identity == first.window.context().session())));
        owner.resume(test_config(&service), &mut [&mut window]).unwrap();
    }
    assert!(
        service.scene.is_registered(first.window.context().session()),
        "suspension retains the logical owner"
    );
    assert!(
        service.scene.surface_status(first.window).is_some_and(|s| s.lifecycle == 1),
        "suspension must retain leased overlay surface metadata"
    );
    assert!(retired.connections.stopped());
    assert!(neighbor.reconcile().unwrap().presented_revision == 1);
    let resumed = live_sessions(&service)
        .into_iter()
        .find(|s| s.identity == first.window.context().session())
        .unwrap();
    assert_eq!(resumed.resume_generation, 1);
    assert_eq!(window.reconcile().unwrap().presented_revision, 1);
    assert!(!matches!(
        old.wait(Duration::ZERO),
        Ok(Some(vivid_sdk::overlay::PresentationOutcome::Presented))
    ));
    assert!(window.request_focus().is_err(), "resume cannot restore input before fresh output");
    let fresh = window.submit(canvas(0x00ff00ff)).unwrap();
    let peer_fresh = neighbor.submit(canvas(0x0000ffff)).unwrap();
    assert!(wait_until(Duration::from_secs(3), || {
        let frames = lock(&service.shared.overlays).drawing(&service.scene);
        frames.iter().any(|d| {
            d.window == first.window && d.submission.channel_generation == 2 && d.revision == 3
        }) && frames.iter().any(|d| d.window == other.window && d.revision == 2)
    }));
    renderer.hold_overlay_for_test(false);
    let (image, _) = completed_overlay_image(&mut renderer, &size, Some(generation));
    let mut frame = Scene::new();
    frame.draw_image(&image, Affine::IDENTITY);
    assert!(renderer.render(&frame, GpuColor::WHITE).unwrap());
    assert_eq!(
        fresh.wait(Duration::from_secs(2)).unwrap(),
        Some(vivid_sdk::overlay::PresentationOutcome::Presented)
    );
    assert_eq!(
        peer_fresh.wait(Duration::from_secs(2)).unwrap(),
        Some(vivid_sdk::overlay::PresentationOutcome::Presented)
    );
    assert_eq!(window.reconcile().unwrap().presented_revision, 3);
    // Late cleanup from the old runtime must not revoke the replacement lane,
    // even though both runtimes use the same logical owner and lane generation.
    drop(super::LaneCleanup {
        shared: service.shared.clone(),
        session: retired,
        generation: 1,
        writer: retired_writer,
        egress: None,
    });
    assert_eq!(window.reconcile().unwrap().presented_revision, 3);
    window.request_focus().unwrap();
    assert!(service.overlay_keyboard(Event::Text("AΩ".into()), false));
    assert!(wait_until(Duration::from_secs(2), || {
        matches!(owner.wait_event(Duration::from_millis(10)).unwrap(), Some(vivid_sdk::OverlayLaneEvent::Input(event))
            if matches!(event.event, Event::Text(ref text) if text == "AΩ") && event.scene_revision == 3)
    }));
    let after =
        service.scene.track_status(other.window.track(other.submission.track_id).unwrap()).unwrap();
    assert_eq!(after.state.channel_generation, peer_status.state.channel_generation);
    assert_eq!(neighbor.reconcile().unwrap().presented_revision, 2);
    assert!(renderer.overlay_allocations_for_test() <= 2);
    let readback = renderer.begin_screenshot().unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    let pixels = loop {
        if let Some(pixels) = renderer.poll_screenshot(&readback).unwrap() {
            break pixels;
        }
        assert!(Instant::now() < until, "resumed frame readback timed out");
        thread::yield_now();
    };
    for (x, expected) in [(8, [0, 255, 0, 255]), (40, [0, 0, 255, 255]), (24, [255, 255, 255, 255])]
    {
        let offset = 8 * pixels.padded_bytes_per_row as usize + x * 4;
        assert_eq!(&pixels.bytes[offset..offset + 4], &expected);
    }
    window.close().unwrap();
    neighbor.close().unwrap();
    owner.close().unwrap();
    peer.close().unwrap();
    controller.close().unwrap();
}
