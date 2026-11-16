//! Reservation regressions with two authenticated owners reusing local IDs.

// Rust guideline compliant 2026-10-07
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use vivid_protocol::identity::{PresenterInstanceId, SessionIdentity, SurfaceIdentity};
use vivid_protocol::messages::LaneClass;
use vivid_protocol::overlay::wire::{PresentationOutcome, Submission, WindowAddress};
use vivid_protocol::overlay::{WindowMode, WindowOptions};
use vivid_protocol::revision::{ChannelGeneration, TargetGeneration};
use vivid_protocol::surface::{CoordinateModel, SurfaceDefinition, SurfaceDescriptor, SurfaceRole};
use vivid_protocol::track::{
    KindConfiguration, TrackConfiguration, TrackMode, VectorConfiguration,
};
use vivid_protocol::vector::{Brush, Canvas, Color, Command, HitRole, Path, Rect};

use super::super::{Host, compile};
use crate::display::text::TextSystem;
use crate::vivid::scene::SharedScene;
use crate::vivid::target::DisplayGeometry;

struct Fixture {
    scene: SharedScene,
    host: Host,
    surfaces: [SurfaceIdentity; 2],
}

impl Fixture {
    fn new() -> Self {
        let scene = SharedScene::for_test();
        let mut host = Host::default();
        host.update_viewport(800., 600., 1.).unwrap();
        let surfaces = [1, 2].map(|serial| {
            let owner = SessionIdentity::new(PresenterInstanceId([61; 16]), serial).unwrap();
            let surface = owner.context(1).unwrap().surface(1).unwrap();
            scene.register_session(owner, TargetGeneration::ONE).unwrap();
            scene
                .create_surface(
                    surface,
                    SurfaceDefinition {
                        context_id: 1,
                        surface_id: 1,
                        semantic_profile: vivid_protocol::registry::TERMINAL_CONTENT.into(),
                        coordinate_model: CoordinateModel::TerminalContentCells,
                        logical_width: 10,
                        logical_height: 10,
                        scale_numerator: 1,
                        scale_denominator: 1,
                        rotation: 0,
                        descriptor: SurfaceDescriptor {
                            role: SurfaceRole::Figure,
                            title: String::new(),
                            semantic_content_revision: 0,
                            semantic_availability: 0,
                            locator_hint: String::new(),
                        },
                        policy: 0,
                        profile_parameters: vec![],
                    },
                )
                .unwrap();
            let options = WindowOptions::new(
                Rect::new(20. * serial as f64, 0., 10., 10.).unwrap(),
                WindowMode::Floating,
            );
            host.set_window(surface, 1, 0, options).unwrap();
            host.surfaces.insert(surface);
            host.lane_open(owner, 1);
            surface
        });
        let mut fixture = Self { scene, host, surfaces };
        for surface in surfaces {
            fixture.track(surface, 2);
            fixture.submit(surface, 2, 1);
            fixture.activate(surface, 2);
        }
        fixture
    }

    fn track(&self, surface: SurfaceIdentity, id: u64) {
        let track = surface.track(id).unwrap();
        self.scene
            .create_track(
                track,
                TrackConfiguration {
                    direction: Default::default(),
                    context_id: 1,
                    surface_id: 1,
                    track_id: id,
                    slot: vivid_sdk::SLOT_VECTOR,
                    mode: TrackMode::Live,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 1036,
                    maximum_rate_millihertz: 60_000,
                    maximum_encoded_bits_per_second: 497_280,
                    maximum_records_per_second: 60,
                    maximum_inflight_body_bytes: 8192,
                    target_latency_us: 0,
                    maximum_latency_us: 0,
                    retained_pixel_charge: 100,
                    kind: KindConfiguration::VectorScene(VectorConfiguration {
                        width: 10,
                        height: 10,
                        maximum_scene_bytes: 1024,
                    }),
                },
            )
            .unwrap();
        self.scene.accept_channel(track, ChannelGeneration::ONE, 8192, 64).unwrap();
    }

    fn activate(&mut self, surface: SurfaceIdentity, track: u64) {
        let revision = self.scene.surface_status(surface).unwrap().revision;
        self.scene
            .activate_tracks(
                surface,
                revision,
                &[(
                    vivid_sdk::SLOT_VECTOR,
                    track,
                    ChannelGeneration::ONE,
                    vivid_sdk::MILESTONE_OUTPUT_READY,
                )],
            )
            .unwrap();
        self.host.sync_active(&self.scene, surface);
    }

    fn submit(&mut self, surface: SurfaceIdentity, id: u64, revision: u64) -> Weak<()> {
        let track = surface.track(id).unwrap();
        let status = self.scene.track_status(track).unwrap();
        let generation = status.state.channel_generation;
        let epoch = status.state.media_epoch.max(1);
        let sequence = status.last_media_record_sequence.max(1).checked_add(1).unwrap();
        self.scene.admit_media(track, generation, 1, epoch, revision, true, sequence).unwrap();
        self.scene.mark_output_ready(track, generation).unwrap();
        let resource = Arc::new(());
        let weak = Arc::downgrade(&resource);
        let mut canvas = Canvas::new();
        let rect = Rect::new(0., 0., 10., 10.).unwrap();
        let color = if surface == self.surfaces[1] {
            0x0000ffff
        } else if revision == 1 {
            0xff0000ff
        } else {
            0x00ff00ff
        };
        canvas.fill(Path::rectangle(rect).unwrap(), Brush::Solid(Color(color))).unwrap();
        canvas
            .push(Command::Hit {
                id: 1,
                path: Path::rectangle(rect).unwrap(),
                role: HitRole::Input,
                cursor: None,
            })
            .unwrap();
        let mut compiled =
            compile(&canvas, &mut TextSystem::new(Default::default()), &BTreeMap::new()).unwrap();
        compiled.retain_resource(resource).unwrap();
        let submission = Submission {
            address: WindowAddress { context_id: 1, surface_id: 1, generation: 1 },
            track_id: id,
            channel_generation: generation.get(),
            epoch,
            revision,
        };
        self.host.assets.insert((track, generation, revision), (weak.clone(), 4));
        self.host.scenes.insert(track, (generation, revision, Arc::new(compiled)));
        self.host.submissions.insert(track, submission);
        self.host.pending.insert((surface.context().session(), submission));
        self.host.sync_active(&self.scene, surface);
        weak
    }

    fn pending(&self, surface: SurfaceIdentity, revision: u64) -> bool {
        self.host.pending.iter().any(|(owner, submission)| {
            *owner == surface.context().session() && submission.revision == revision
        })
    }
}

#[test]
fn suspended_hidden_windows_need_fresh_output_before_input_returns() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let owner = first.context().session();
    let hidden = first.context().surface(2).unwrap();
    let mut options =
        WindowOptions::new(Rect::new(0., 20., 10., 10.).unwrap(), WindowMode::Floating);
    options.visible = false;
    f.host.set_window(hidden, 1, 0, options.clone()).unwrap();
    let snapshot = f.host.prepare(&f.scene).unwrap();
    assert!(f.host.finish(&f.scene, &snapshot, true));
    let first_displayed = f.host.displayed[&first].clone();
    let peer_displayed = f.host.displayed[&peer].clone();
    f.host.accepted.insert(first, 1);
    f.host.suspend_owner(owner);
    assert_eq!(f.host.accepted[&first], 1);
    assert!(f.host.windows.request_focus(first).is_err());
    f.host.lane_open(owner, 2);
    // Model a newly admitted final composition, then reveal a retained hidden window.
    f.host.displayed.insert(first, first_displayed.clone());
    f.host.refresh_hover();
    f.host.windows.request_focus(first).unwrap();
    options.visible = true;
    f.host.set_window(hidden, 1, 1, options).unwrap();
    assert!(f.host.windows.request_focus(first).is_err());
    assert!(f.host.windows.request_focus(hidden).is_err());
    assert!(Arc::ptr_eq(&f.host.displayed[&peer].compiled, &peer_displayed.compiled));
    f.host.windows.request_focus(peer).unwrap();
    let mut hidden_displayed = first_displayed;
    hidden_displayed.window = hidden;
    f.host.displayed.insert(hidden, hidden_displayed);
    f.host.refresh_hover();
    f.host.windows.request_focus(hidden).unwrap();
    assert!(!f.host.awaiting_fresh_input.contains(&hidden));
}

#[test]
fn snapshots_preserve_original_receipts_and_bound_retained_candidates() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let old = f.host.prepare(&f.scene).unwrap();
    let old_compiled = old.drawings()[0].compiled.clone();
    let resource = f.submit(first, 2, 2);
    assert!(f.pending(first, 1), "the old reservation still owns revision one");
    let new = f.host.prepare(&f.scene).unwrap();
    assert!(f.host.prepare(&f.scene).is_none(), "two reservations are the bound");
    assert!(f.host.finish(&f.scene, &old, true));
    assert_eq!(f.host.displayed[&first].revision, 1);
    assert!(Arc::ptr_eq(&f.host.displayed[&first].compiled, &old_compiled));
    assert!(f.pending(first, 2), "presenting the old image cannot ack the replacement");
    assert!(!f.pending(peer, 1));
    assert!(f.host.admits(&f.scene, &new));
    assert!(!f.host.finish(&f.scene, &old, true), "a receipt can settle only once");
    assert_eq!(f.host.reserved.len(), 1);
    assert!(!f.host.finish(&f.scene, &new, false));
    assert!(f.pending(first, 2), "abandoning a current candidate permits retry");
    let retry = f.host.prepare(&f.scene).unwrap();
    assert!(f.host.finish(&f.scene, &retry, true));
    assert_eq!(f.host.displayed[&first].revision, 2);
    assert!(resource.strong_count() > 0, "displayed assets stay charged");
    assert_eq!(f.host.outcomes[&first.context().session()].len(), 2);
}

#[test]
fn late_older_snapshot_cannot_roll_back_displayed_hits_or_settle_a_peer() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let old = f.host.prepare(&f.scene).unwrap();
    f.submit(first, 2, 2);
    let new = f.host.prepare(&f.scene).unwrap();
    assert!(f.host.finish(&f.scene, &new, true));
    let compiled = f.host.displayed[&first].compiled.clone();
    assert!(!f.host.finish(&f.scene, &old, true));
    assert_eq!(f.host.displayed[&first].revision, 2);
    assert!(Arc::ptr_eq(&f.host.displayed[&first].compiled, &compiled));
    let first_outcomes = &f.host.outcomes[&first.context().session()];
    assert_eq!(first_outcomes[0].submission.revision, 2);
    assert_eq!(first_outcomes[0].outcome, PresentationOutcome::Presented);
    assert_eq!(first_outcomes[1].submission.revision, 1);
    assert_eq!(first_outcomes[1].outcome, PresentationOutcome::Superseded);
    assert_eq!(f.host.outcomes[&peer.context().session()].len(), 1);
}

#[test]
fn channel_epoch_track_and_resume_changes_reject_the_entire_old_image() {
    for mutation in 0..6 {
        let mut f = Fixture::new();
        let [first, peer] = f.surfaces;
        let old = f.host.prepare(&f.scene).unwrap();
        let track = first.track(2).unwrap();
        match mutation {
            0 => {
                f.scene.advance_channel(track).unwrap();
            },
            1 => {
                f.scene.admit_media(track, ChannelGeneration::ONE, 1, 2, 2, true, 3).unwrap();
            },
            2 => {
                f.track(first, 3);
                f.submit(first, 3, 2);
                f.activate(first, 3);
                f.host.sync_active(&f.scene, first);
            },
            3 => {
                f.scene.suspend_session(first.context().session());
            },
            4 => {
                f.scene.detach_channel(track, ChannelGeneration::ONE).unwrap();
            },
            5 => {
                f.scene.lose_track(track, ChannelGeneration::ONE).unwrap();
            },
            _ => unreachable!(),
        }
        assert!(!f.host.admits(&f.scene, &old), "mutation {mutation}");
        assert!(!f.host.finish(&f.scene, &old, true), "mutation {mutation}");
        assert!(f.host.displayed.is_empty());
        assert!(f.pending(peer, 1), "stale mixed output must not settle the healthy peer");
        assert!(!f.host.outcomes.contains_key(&peer.context().session()));
        assert_eq!(f.host.windows.get(peer).unwrap().scene_revision, 0);
    }
}

#[test]
fn placement_visibility_viewport_and_target_changes_reject_old_compositions() {
    for mutation in 0..5 {
        let mut f = Fixture::new();
        let first = f.surfaces[0];
        let old = f.host.prepare(&f.scene).unwrap();
        match mutation {
            0 | 1 => {
                let window = f.host.windows.get(first).unwrap();
                let mut options = window.options.clone();
                if mutation == 0 {
                    options.visible = false;
                } else {
                    options.bounds = Rect::new(30., 20., 10., 10.).unwrap();
                }
                f.host.set_window(first, 1, 1, options).unwrap();
            },
            2 => {
                f.host.update_viewport(800., 600., 2.).unwrap();
            },
            3 => {
                f.scene.advance_target_generation(TargetGeneration::new(2));
            },
            4 => {
                f.scene.target().offer_geometry(DisplayGeometry {
                    viewport_width: 900,
                    viewport_height: 600,
                    columns: 90,
                    rows: 24,
                    cell_width: 10,
                    cell_height: 25,
                });
            },
            _ => unreachable!(),
        }
        assert!(!f.host.admits(&f.scene, &old), "mutation {mutation}");
        assert!(!f.host.finish(&f.scene, &old, true), "mutation {mutation}");
        assert!(f.host.displayed.is_empty());
        assert!(f.host.outcomes.is_empty());
    }
}

#[test]
fn exact_owner_cleanup_keeps_peer_state_and_retires_only_the_old_reservation() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let token = f.host.assets[&(first.track(2).unwrap(), ChannelGeneration::ONE, 1)].0.clone();
    let old = f.host.prepare(&f.scene).unwrap();
    f.host.remove_owner(first.context().session());
    assert!(token.strong_count() > 0, "GPU snapshots retain resources until retired");
    assert!(!f.host.finish(&f.scene, &old, true));
    drop(old);
    assert_eq!(token.strong_count(), 0);
    assert!(!f.host.displayed.contains_key(&first));
    assert!(f.host.lanes.contains_key(&peer.context().session()));
    assert!(f.pending(peer, 1));
    let fresh = f.host.prepare(&f.scene).unwrap();
    assert_eq!(fresh.drawings().len(), 1);
    assert!(f.host.finish(&f.scene, &fresh, true));
    assert_eq!(f.host.displayed[&peer].revision, 1);
    assert!(!f.host.outcomes.contains_key(&first.context().session()));
    assert_eq!(f.host.outcomes[&peer.context().session()].len(), 1);
}

#[test]
fn recreated_local_ids_and_foreign_reservations_cannot_reuse_authority() {
    let mut f = Fixture::new();
    let first = f.surfaces[0];
    let old = f.host.prepare(&f.scene).unwrap();
    let options = f.host.windows.get(first).unwrap().options.clone();
    f.host.remove_surface(first);
    // Even if an internal caller reuses every numeric field, the retired track
    // guard is not the allocation captured by the original reservation.
    f.host.set_window(first, 1, 0, options).unwrap();
    f.submit(first, 2, 2);
    let new = f.host.prepare(&f.scene).unwrap();
    assert!(!f.host.admits(&f.scene, &old));
    assert!(!Host::default().finish(&f.scene, &new, true));
    assert_eq!(f.host.reserved.len(), 2);
    assert!(!f.host.finish(&f.scene, &old, true));
    assert!(f.host.finish(&f.scene, &new, true));
    assert_eq!(f.host.displayed[&first].revision, 2);
}

#[test]
fn abandoning_one_of_two_original_snapshots_preserves_the_other_receipt() {
    let mut f = Fixture::new();
    let first = f.surfaces[0];
    let old = f.host.prepare(&f.scene).unwrap();
    let duplicate = f.host.prepare(&f.scene).unwrap();
    f.submit(first, 2, 2);
    assert!(!f.host.finish(&f.scene, &old, false));
    assert!(f.pending(first, 1), "the other reservation still owns this receipt");
    assert!(!f.host.finish(&f.scene, &duplicate, false));
    assert!(!f.pending(first, 1));
    assert!(f.pending(first, 2));
    let outcomes = &f.host.outcomes[&first.context().session()];
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome, PresentationOutcome::Superseded);
}

#[test]
fn a_new_image_replaces_the_entire_hit_test_set_after_channel_retirement() {
    let mut f = Fixture::new();
    let [first, peer] = f.surfaces;
    let original = f.host.prepare(&f.scene).unwrap();
    assert!(f.host.finish(&f.scene, &original, true));
    assert_eq!(f.host.displayed.len(), 2);
    let peer_compiled = f.host.displayed[&peer].compiled.clone();
    f.scene.detach_channel(first.track(2).unwrap(), ChannelGeneration::ONE).unwrap();
    let fresh = f.host.prepare(&f.scene).unwrap();
    assert_eq!(fresh.drawings().len(), 1);
    assert!(f.host.finish(&f.scene, &fresh, true));
    assert!(!f.host.displayed.contains_key(&first));
    assert!(Arc::ptr_eq(&f.host.displayed[&peer].compiled, &peer_compiled));
    assert_eq!(f.host.outcomes[&peer.context().session()].len(), 1);
}

#[test]
fn final_admission_holds_channel_authority_until_publication_finishes() {
    use std::sync::mpsc;
    use std::time::Duration;

    let mut f = Fixture::new();
    let snapshot = f.host.prepare(&f.scene).unwrap();
    let guard = f.scene.admit_overlay(snapshot.target_generation, snapshot.bindings()).unwrap();
    let scene = f.scene.clone();
    let track = f.surfaces[0].track(2).unwrap();
    let (starting, started) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        starting.send(()).unwrap();
        scene.advance_channel(track).unwrap();
        finished.send(()).unwrap();
    });
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(matches!(
        done.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(guard);
    done.recv_timeout(Duration::from_secs(3)).unwrap();
    thread.join().unwrap();
    assert!(!f.host.finish(&f.scene, &snapshot, true));
    assert!(f.host.displayed.is_empty());
}

#[cfg(any(unix, windows))]
mod native;

mod refusal;

mod retained;
