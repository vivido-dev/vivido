//! Committed-scene handoffs must preserve exact same-ID owner authority.

// Rust guideline compliant 2026-10-07
use super::*;

#[test]
fn retaining_advances_exact_peer_receipts_and_keeps_selected_replacement_pending() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let mut shown = f.host.prepare(&f.scene).unwrap();
    assert!(f.host.prepare_retaining(&f.scene, &shown, &[first]).is_none(), "unadmitted image");
    assert!(f.host.finish(&f.scene, &shown, true));
    let old = shown.clone();
    let compiled = f.host.displayed[&first].compiled.clone();
    f.submit(first, 2, 2);
    let diagnostic = f.host.prepare(&f.scene).unwrap();
    for revision in 2..=5 {
        f.submit(peer, 2, revision);
        let next = f.host.prepare_retaining(&f.scene, &shown, &[first]).unwrap();
        assert_eq!(f.host.reserved.len(), 2);
        assert!(f.host.prepare(&f.scene).is_none());
        assert!(f.host.prepare_retaining(&f.scene, &shown, &[first]).is_none());
        let selected = next.drawings.iter().find(|d| d.window == first).unwrap();
        assert_eq!(selected.revision, 1);
        assert!(Arc::ptr_eq(&selected.compiled, &compiled));
        assert!(f.pending(first, 2) && f.pending(peer, revision));
        assert!(f.host.finish(&f.scene, &next, true));
        assert!(f.host.diagnosing(&f.scene, &diagnostic));
        assert_eq!(f.host.displayed[&first].revision, 1);
        assert_eq!(f.host.displayed[&peer].revision, revision);
        assert!(f.pending(first, 2) && !f.pending(peer, revision));
        assert!(
            f.host.prepare_retaining(&f.scene, &old, &[first]).is_none(),
            "older admitted frame"
        );
        shown = next;
    }
    assert!(f.host.reject_drawing(&f.scene, &diagnostic, first));
    let survivors = f.host.reserve_survivors(&f.scene, &shown, first).unwrap();
    assert!(survivors.survivors(&shown, first));
    assert!(!f.host.finish(&f.scene, &diagnostic, false));
    assert!(f.host.finish(&f.scene, &survivors, true));
    assert_eq!(f.host.displayed.len(), 1);
    assert_eq!(f.host.displayed[&peer].revision, 5);
    assert_eq!(
        f.host.outcomes[&peer.context().session()].len(),
        5,
        "cached survivor does not duplicate outcomes"
    );
    assert!(
        f.host.outcomes[&first.context().session()]
            .iter()
            .any(|o| o.submission.revision == 2 && o.outcome == PresentationOutcome::Superseded)
    );
}

#[test]
fn retaining_rejects_foreign_duplicate_absent_and_changed_contexts_without_settling_peers() {
    for mutation in 0..12 {
        let mut f = Fixture::new();
        let [first, peer] = f.surfaces;
        let shown = f.host.prepare(&f.scene).unwrap();
        assert!(f.host.finish(&f.scene, &shown, true));
        f.submit(first, 2, 2);
        f.submit(peer, 2, 2);
        let track = first.track(2).unwrap();
        let mut selected = vec![first];
        match mutation {
            0 => selected.push(first),
            1 => selected[0] = first.context().surface(99).unwrap(),
            2 => f.host.update_viewport(800., 600., 2.).unwrap(),
            3 => {
                f.scene.advance_target_generation(TargetGeneration::new(2));
            },
            4 => {
                f.scene.advance_channel(track).unwrap();
            },
            5 => {
                f.scene.detach_channel(track, ChannelGeneration::ONE).unwrap();
            },
            6 => {
                f.scene.suspend_session(first.context().session());
            },
            7 => {
                f.host.lane_open(first.context().session(), 2);
            },
            8 => {
                f.track(first, 3);
                f.submit(first, 3, 3);
                f.activate(first, 3);
            },
            9 | 10 => {
                let mut options = f.host.windows.get(first).unwrap().options.clone();
                if mutation == 9 {
                    options.visible = false;
                } else {
                    options.bounds = Rect::new(30., 20., 10., 10.).unwrap();
                }
                f.host.set_window(first, 1, 1, options).unwrap();
            },
            11 => {
                let mut foreign = Fixture::new();
                assert!(
                    foreign.host.prepare_retaining(&foreign.scene, &shown, &selected).is_none()
                );
                // Even identical cloned metadata is not the admitted allocation.
                let clone = Arc::new(super::super::Snapshot {
                    drawings: shown.drawings.clone(),
                    bindings: shown.bindings.clone(),
                    windows: shown.windows.clone(),
                    viewport_revision: shown.viewport_revision,
                    target_generation: shown.target_generation,
                });
                assert!(f.host.prepare_retaining(&f.scene, &clone, &selected).is_none());
                continue;
            },
            _ => unreachable!(),
        }
        assert!(
            f.host.prepare_retaining(&f.scene, &shown, &selected).is_none(),
            "mutation {mutation}"
        );
        assert!(f.pending(peer, 2));
        assert_eq!(f.host.displayed[&peer].revision, 1);
    }
}
