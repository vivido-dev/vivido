//! Exact-source refusal fences; these model cases do not claim GPU attribution.

// Rust guideline compliant 2026-10-07
use super::*;

#[test]
fn isolated_refusal_closes_only_the_exact_owner_and_ordinary_descendants() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let child = first.context().surface(2).unwrap();
    let mut options = WindowOptions::new(Rect::new(0., 0., 5., 5.).unwrap(), WindowMode::Floating);
    options.parent = Some(first);
    f.host.set_window(child, 1, 0, options).unwrap();
    f.host.surfaces.insert(child);
    let failed = f.host.prepare(&f.scene).unwrap();
    let peer_compiled =
        failed.drawings().iter().find(|d| d.window == peer).unwrap().compiled.clone();
    let first_token =
        f.host.assets[&(first.track(2).unwrap(), ChannelGeneration::ONE, 1)].0.clone();
    // Peer placement may advance without invalidating the proof against first.
    let mut peer_options = f.host.windows.get(peer).unwrap().options.clone();
    peer_options.bounds = Rect::new(50., 0., 10., 10.).unwrap();
    f.host.set_window(peer, 1, 1, peer_options).unwrap();
    assert!(f.host.reject_drawing(&f.scene, &failed, first));
    assert!(!f.host.reject_drawing(&f.scene, &failed, first));
    assert!(f.host.windows.get(first).is_none());
    assert!(f.host.windows.get(child).is_none());
    assert!(f.host.windows.get(peer).is_some());
    assert!(f.host.scenes.contains_key(&peer.track(2).unwrap()));
    assert!(f.pending(peer, 1));
    assert!(!f.host.outcomes.contains_key(&peer.context().session()));
    assert_eq!(f.host.outcomes[&first.context().session()].len(), 1);
    assert_eq!(
        f.host.outcomes[&first.context().session()][0].outcome,
        PresentationOutcome::Superseded
    );
    let mut dismissed = Vec::new();
    while let Some(event) = f.host.windows.take_event(first.context().session()) {
        if let vivid_protocol::overlay::Event::Dismissed(reason) = event.event {
            dismissed.push((event.window, reason));
        }
    }
    assert_eq!(
        dismissed,
        vec![
            (child, vivid_protocol::overlay::DismissReason::ParentClosed),
            (first, vivid_protocol::overlay::DismissReason::Closed),
        ]
    );
    assert!(!f.host.finish(&f.scene, &failed, true));
    drop(failed);
    assert_eq!(first_token.strong_count(), 0);
    let fresh = f.host.prepare(&f.scene).unwrap();
    assert_eq!(fresh.drawings().len(), 1);
    assert!(Arc::ptr_eq(&fresh.drawings()[0].compiled, &peer_compiled));
    assert!(f.host.finish(&f.scene, &fresh, true));
    assert_eq!(f.host.outcomes[&peer.context().session()].len(), 1);
}

#[test]
fn late_refusal_cannot_close_newer_displayed_or_rebound_content() {
    for mutation in 0..10 {
        let mut f = Fixture::new();
        let [first, peer] = f.surfaces;
        let failed = f.host.prepare(&f.scene).unwrap();
        match mutation {
            0 => {
                f.submit(first, 2, 2);
            },
            1 => {
                assert!(f.host.finish(&f.scene, &failed, true));
            },
            2 => {
                f.scene.advance_channel(first.track(2).unwrap()).unwrap();
            },
            3 => {
                f.scene.suspend_session(first.context().session());
            },
            4 => {
                let mut options = f.host.windows.get(first).unwrap().options.clone();
                options.visible = false;
                f.host.set_window(first, 1, 1, options).unwrap();
            },
            5 => {
                f.host.update_viewport(800., 600., 2.).unwrap();
            },
            6 => {
                f.scene.advance_target_generation(TargetGeneration::new(2));
            },
            7 => {
                f.track(first, 3);
                f.submit(first, 3, 2);
                f.activate(first, 3);
            },
            8 => {
                let options = f.host.windows.get(first).unwrap().options.clone();
                f.host.remove_surface(first);
                f.host.set_window(first, 1, 0, options).unwrap();
                f.submit(first, 2, 2);
            },
            9 => {
                f.scene
                    .admit_media(first.track(2).unwrap(), ChannelGeneration::ONE, 1, 2, 2, true, 3)
                    .unwrap();
            },
            _ => unreachable!(),
        }
        assert!(!f.host.reject_drawing(&f.scene, &failed, first), "mutation {mutation}");
        assert!(f.host.windows.get(first).is_some(), "mutation {mutation}");
        assert!(f.host.windows.get(peer).is_some());
        assert!(f.host.scenes.contains_key(&peer.track(2).unwrap()));
        assert!(!Host::default().reject_drawing(&f.scene, &failed, peer));
    }
}

#[test]
fn checked_survivors_require_the_original_peer_content_and_binding() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let failed = f.host.prepare(&f.scene).unwrap();
    assert!(f.host.reject_drawing(&f.scene, &failed, first));
    assert!(!f.host.finish(&f.scene, &failed, false));
    let fresh = f.host.prepare(&f.scene).unwrap();
    assert!(fresh.survivors(&failed, first));
    assert!(!f.host.finish(&f.scene, &fresh, false));
    f.submit(peer, 2, 2);
    let next = f.host.prepare(&f.scene).unwrap();
    assert!(!next.survivors(&failed, first));
    assert!(!f.pending(peer, 1), "a newer candidate supersedes the abandoned original");
    assert!(f.pending(peer, 2));
}

#[test]
fn derived_survivor_keeps_original_receipt_while_a_newer_peer_waits() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let failed = f.host.prepare(&f.scene).unwrap();
    f.submit(peer, 2, 2);
    assert!(f.host.reject_drawing(&f.scene, &failed, first));
    let surviving = f.host.reserve_survivors(&f.scene, &failed, first).unwrap();
    assert!(surviving.survivors(&failed, first));
    assert!(!f.host.finish(&f.scene, &failed, false));
    assert!(f.pending(peer, 1));
    assert!(f.pending(peer, 2));
    assert!(f.host.finish(&f.scene, &surviving, true));
    assert_eq!(f.host.displayed[&peer].revision, 1);
    assert!(f.pending(peer, 2));
    assert_eq!(f.host.outcomes[&peer.context().session()].len(), 1);
    assert_eq!(
        f.host.outcomes[&peer.context().session()][0].outcome,
        PresentationOutcome::Presented
    );
    let next = f.host.prepare(&f.scene).unwrap();
    assert!(f.host.finish(&f.scene, &next, true));
    assert_eq!(f.host.displayed[&peer].revision, 2);
}

#[test]
fn derived_survivor_cannot_rebind_placement_target_or_channel() {
    for mutation in 0..5 {
        let mut f = Fixture::new();
        let [first, peer] = f.surfaces;
        let failed = f.host.prepare(&f.scene).unwrap();
        assert!(f.host.reject_drawing(&f.scene, &failed, first));
        match mutation {
            0 => {
                let mut options = f.host.windows.get(peer).unwrap().options.clone();
                options.bounds = Rect::new(50., 0., 10., 10.).unwrap();
                f.host.set_window(peer, 1, 1, options).unwrap();
            },
            1 => {
                f.scene.advance_target_generation(TargetGeneration::new(2));
            },
            2 => {
                f.scene.advance_channel(peer.track(2).unwrap()).unwrap();
            },
            3 => {
                let mut options = f.host.windows.get(peer).unwrap().options.clone();
                options.visible = false;
                f.host.set_window(peer, 1, 1, options).unwrap();
            },
            4 => {
                f.host.update_viewport(800., 600., 2.).unwrap();
            },
            _ => unreachable!(),
        }
        assert!(
            f.host.reserve_survivors(&f.scene, &failed, first).is_none(),
            "mutation {mutation}"
        );
        assert!(Host::default().reserve_survivors(&f.scene, &failed, first).is_none());
        assert!(f.pending(peer, 1));
        assert!(!f.host.outcomes.contains_key(&peer.context().session()));
    }
}
