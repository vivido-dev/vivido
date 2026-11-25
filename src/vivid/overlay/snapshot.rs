//! Explicit, bounded reservations for immutable overlay compositions.

// Rust guideline compliant 2026-10-07
use std::sync::Arc;

use vivid_protocol::identity::{SessionIdentity, SurfaceIdentity, TrackIdentity};
use vivid_protocol::overlay::wire::{PresentationOutcome, Submission};
use vivid_protocol::revision::TargetGeneration;

use super::{Drawing, Host};
use crate::vivid::scene::SharedScene;

// One displayed replacement and one pending composition. Candidates are coalesced
// by the consumer instead of growing a queue of retained compiled scenes.
const MAX_RESERVATIONS: usize = 2;

/// An immutable scene reservation, identified by its allocation rather than numeric IDs.
pub(crate) struct Snapshot {
    drawings: Vec<Drawing>,
    bindings: Vec<(TrackIdentity, Arc<()>, TargetGeneration)>,
    windows: Vec<(SurfaceIdentity, u64, u64)>,
    viewport_revision: u64,
    target_generation: u64,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("drawings", &self.drawings.len())
            .field("viewport_revision", &self.viewport_revision)
            .field("target_generation", &self.target_generation)
            .finish_non_exhaustive()
    }
}

impl Snapshot {
    fn bindings(
        &self,
    ) -> impl Iterator<Item = (SurfaceIdentity, Submission, TargetGeneration)> + '_ {
        self.drawings
            .iter()
            .zip(&self.bindings)
            .map(|(drawing, (_, _, target))| (drawing.window, drawing.submission, *target))
    }

    /// Compares exact retained content without granting admission to either snapshot.
    pub(crate) fn same_content(&self, other: &Self) -> bool {
        self.viewport_revision == other.viewport_revision
            && self.target_generation == other.target_generation
            && self.windows == other.windows
            && self.drawings.len() == other.drawings.len()
            && self.bindings.len() == other.bindings.len()
            && self.drawings.iter().zip(&other.drawings).all(|(a, b)| {
                a.window == b.window
                    && a.window_revision == b.window_revision
                    && a.revision == b.revision
                    && a.bounds == b.bounds
                    && a.scale.to_bits() == b.scale.to_bits()
                    && a.submission == b.submission
                    && Arc::ptr_eq(&a.compiled, &b.compiled)
            })
            && self
                .bindings
                .iter()
                .zip(&other.bindings)
                .all(|(a, b)| a.0 == b.0 && Arc::ptr_eq(&a.1, &b.1) && a.2 == b.2)
    }

    pub(crate) fn drawings(&self) -> &[Drawing] {
        &self.drawings
    }

    /// Matches a checked replacement that omitted exactly one refused window.
    pub(crate) fn survivors(&self, original: &Self, omitted: SurfaceIdentity) -> bool {
        self.viewport_revision == original.viewport_revision
            && self.target_generation == original.target_generation
            && self.drawings.len().checked_add(1) == Some(original.drawings.len())
            && self
                .drawings
                .iter()
                .zip(&self.bindings)
                .zip(
                    original
                        .drawings
                        .iter()
                        .zip(&original.bindings)
                        .filter(|(d, _)| d.window != omitted),
                )
                .all(|((a, ag), (b, bg))| {
                    a.window == b.window
                        && a.window_revision == b.window_revision
                        && a.revision == b.revision
                        && a.submission == b.submission
                        && a.bounds == b.bounds
                        && a.scale.to_bits() == b.scale.to_bits()
                        && Arc::ptr_eq(&a.compiled, &b.compiled)
                        && ag.0 == bg.0
                        && Arc::ptr_eq(&ag.1, &bg.1)
                        && ag.2 == bg.2
                })
    }
}

impl Host {
    /// Reserves checked peers from an original frame after exact source refusal.
    ///
    /// Newer accepted peer bytes do not replace this frame's pixels or receipt.
    /// Preserve its reservation until this derived handle exists, then abandon
    /// the failed frame. Other topology, placement, target or binding changes
    /// prevent reuse, including descendants also closed by the ordinary policy.
    pub(crate) fn reserve_survivors(
        &mut self,
        scene: &SharedScene,
        original: &Arc<Snapshot>,
        omitted: SurfaceIdentity,
    ) -> Option<Arc<Snapshot>> {
        if self.reserved.len() >= MAX_RESERVATIONS
            || !(self.reserved.iter().any(|s| Arc::ptr_eq(s, original))
                || self.admitted.upgrade().is_some_and(|s| Arc::ptr_eq(&s, original)))
            || self.windows.get(omitted).is_some()
            || !original.drawings.iter().any(|d| d.window == omitted)
        {
            return None;
        }
        let windows = self.snapshot_windows();
        if !windows.iter().eq(original.windows.iter().filter(|(id, _, _)| *id != omitted)) {
            return None;
        }
        let snapshot = Arc::new(Snapshot {
            drawings: original.drawings.iter().filter(|d| d.window != omitted).cloned().collect(),
            bindings: original
                .drawings
                .iter()
                .zip(&original.bindings)
                .filter(|(d, _)| d.window != omitted)
                .map(|(_, binding)| binding.clone())
                .collect(),
            windows,
            viewport_revision: original.viewport_revision,
            target_generation: original.target_generation,
        });
        if !self.current(scene, &snapshot) {
            return None;
        }
        self.reserved.push(snapshot.clone());
        Some(snapshot)
    }

    /// Closes only a still-current drawing after an isolated checked work failure.
    ///
    /// A mixed-frame failure grants no attribution. The caller must prove this
    /// drawing fails by itself; this method grants no presentation receipt.
    /// Foreign, displayed, replaced, resumed or otherwise stale work is ignored.
    pub(crate) fn reject_drawing(
        &mut self,
        scene: &SharedScene,
        snapshot: &Arc<Snapshot>,
        window: SurfaceIdentity,
    ) -> bool {
        if !self.reserved.iter().any(|s| Arc::ptr_eq(s, snapshot))
            || snapshot.viewport_revision != self.viewport_revision
        {
            return false;
        }
        let Some((drawing, (track, guard, target))) = snapshot
            .drawings
            .iter()
            .zip(&snapshot.bindings)
            .find(|(drawing, _)| drawing.window == window)
        else {
            return false;
        };
        let owner = window.context().session();
        let Some(current) = self.windows.get(window) else { return false };
        if !snapshot.windows.contains(&(window, current.generation, current.revision))
            || current.scene_revision >= drawing.revision
            || self.lanes.get(&owner).is_none_or(|(_, until)| std::time::Instant::now() >= *until)
            || !self.reservation_guards.get(track).is_some_and(|g| Arc::ptr_eq(g, guard))
            || self.active.get(&window) != Some(&drawing.submission)
            || self.accepted.get(&window).is_some_and(|r| *r > drawing.revision)
            || !self.scenes.get(track).is_some_and(|(generation, revision, compiled)| {
                generation.get() == drawing.submission.channel_generation
                    && *revision == drawing.revision
                    && Arc::ptr_eq(compiled, &drawing.compiled)
            })
        {
            return false;
        }
        // Keep channel/epoch/target authority stable through the host mutation,
        // without acquiring another scene lock or doing GPU work under it.
        let Some(admission) = scene
            .admit_overlay(snapshot.target_generation, [(window, drawing.submission, *target)])
        else {
            return false;
        };
        let before: Vec<_> = self.windows.stacked().map(|w| w.identity).collect();
        self.windows.close(window, vivid_protocol::overlay::DismissReason::Closed);
        let closed: Vec<_> =
            before.into_iter().filter(|id| self.windows.get(*id).is_none()).collect();
        for id in closed {
            self.layouts.retain(|(surface, _), _| *surface != id);
            if self.editor.is_some_and(|(surface, _)| surface == id) {
                self.editor = None;
            }
            self.retire_presentation(id);
            self.accepted.remove(&id);
            self.awaiting_fresh_input.remove(&id);
            self.surfaces.remove(&id);
        }
        self.refresh_hover();
        self.retire_stale_semantics();
        if let Some(actor) = self.actors.get(&owner).and_then(std::sync::Weak::upgrade) {
            actor.wake_actor();
        }
        drop(admission);
        true
    }

    fn snapshot_windows(&self) -> Vec<(SurfaceIdentity, u64, u64)> {
        self.windows.visible().map(|w| (w.identity, w.generation, w.revision)).collect()
    }

    fn reserved_submission(&self, owner: SessionIdentity, submission: Submission) -> bool {
        self.reserved.iter().any(|snapshot| {
            snapshot
                .drawings
                .iter()
                .any(|d| d.window.context().session() == owner && d.submission == submission)
        })
    }

    pub(super) fn supersede(&mut self, owner: SessionIdentity, submission: Submission) {
        if !self.reserved_submission(owner, submission) {
            self.resolve(owner, submission, PresentationOutcome::Superseded);
        }
    }

    /// Reserves a snapshot without replacing any earlier pending composition.
    ///
    /// Returns `None` at the two-reservation limit. The caller must finish or
    /// abandon each reservation explicitly; dropping its copy cannot silently
    /// resolve a receipt whose GPU work may still be pending.
    pub(crate) fn prepare(&mut self, scene: &SharedScene) -> Option<Arc<Snapshot>> {
        if self.reserved.len() >= MAX_RESERVATIONS {
            return None;
        }
        let snapshot = Arc::new(self.candidate(scene));
        self.reserved.push(snapshot.clone());
        Some(snapshot)
    }

    fn candidate(&mut self, scene: &SharedScene) -> Snapshot {
        let candidates = self.drawing(scene);
        let mut drawings = Vec::with_capacity(candidates.len());
        let mut bindings = Vec::with_capacity(candidates.len());
        for drawing in candidates {
            if self
                .lanes
                .get(&drawing.window.context().session())
                .is_none_or(|(_, deadline)| std::time::Instant::now() >= *deadline)
            {
                continue;
            }
            let Some(target) = scene.overlay_binding(drawing.window, drawing.submission) else {
                continue;
            };
            let Ok(track) = drawing.window.track(drawing.submission.track_id) else {
                continue;
            };
            let guard = self.reservation_guards.entry(track).or_default().clone();
            bindings.push((track, guard, target));
            drawings.push(drawing);
        }
        Snapshot {
            drawings,
            bindings,
            windows: self.snapshot_windows(),
            viewport_revision: self.viewport_revision,
            target_generation: scene.target().generation(),
        }
    }

    fn context_host(&self, snapshot: &Arc<Snapshot>) -> bool {
        snapshot.viewport_revision == self.viewport_revision
            && self
                .windows
                .visible()
                .map(|w| (w.identity, w.generation, w.revision))
                .eq(snapshot.windows.iter().copied())
            && snapshot.drawings.iter().zip(&snapshot.bindings).all(
                |(drawing, (track, guard, _))| {
                    self.lanes
                        .get(&drawing.window.context().session())
                        .is_some_and(|(_, deadline)| std::time::Instant::now() < *deadline)
                        && self.reservation_guards.get(track).is_some_and(|g| Arc::ptr_eq(g, guard))
                        && self.active.get(&drawing.window).is_some_and(|current| {
                            current.address == drawing.submission.address
                                && current.track_id == drawing.submission.track_id
                                && current.channel_generation
                                    == drawing.submission.channel_generation
                                && current.epoch == drawing.submission.epoch
                        })
                },
            )
    }

    fn current_host(&self, snapshot: &Arc<Snapshot>) -> bool {
        self.context_host(snapshot)
            && snapshot.drawings.iter().all(|drawing| {
                self.windows
                    .get(drawing.window)
                    .is_some_and(|w| w.scene_revision <= drawing.revision)
            })
    }

    /// Keeps diagnostic authority while exact peers advance their displayed scenes.
    pub(crate) fn diagnosing(&self, scene: &SharedScene, snapshot: &Arc<Snapshot>) -> bool {
        self.reserved.iter().any(|s| Arc::ptr_eq(s, snapshot))
            && self.context_host(snapshot)
            && scene.admit_overlay(snapshot.target_generation, snapshot.bindings()).is_some()
    }

    /// Checks whether an immutable displayed image still has authority and placement.
    pub(crate) fn current(&self, scene: &SharedScene, snapshot: &Arc<Snapshot>) -> bool {
        self.current_host(snapshot)
            && scene.admit_overlay(snapshot.target_generation, snapshot.bindings()).is_some()
    }

    /// Checks authority and placement before admitting a reserved GPU result.
    pub(crate) fn admits(&self, scene: &SharedScene, snapshot: &Arc<Snapshot>) -> bool {
        self.reserved.iter().any(|current| Arc::ptr_eq(current, snapshot))
            && self.current(scene, snapshot)
    }

    /// Finishes only this reservation; another pending snapshot remains untouched.
    ///
    /// Returns whether the entire snapshot was admitted. A stale mixed image
    /// cannot publish any owner's input or receipt. Unchanged current candidates
    /// remain pending for a fresh composition.
    pub(crate) fn finish(
        &mut self,
        scene: &SharedScene,
        snapshot: &Arc<Snapshot>,
        presented: bool,
    ) -> bool {
        let Some(index) = self.reserved.iter().position(|s| Arc::ptr_eq(s, snapshot)) else {
            return false;
        };
        // The host lock is already held by the caller. Keep the scene lock only
        // through this short publication, so channel retirement cannot interleave
        // between validating different owners and resolving their receipts.
        let admission = (presented && self.current_host(snapshot))
            .then(|| scene.admit_overlay(snapshot.target_generation, snapshot.bindings()))
            .flatten();
        let admitted = admission.is_some();
        self.reserved.remove(index);
        if admitted {
            // Input describes the exact composed image, including omitted retired
            // sources, rather than accumulating hit trees from earlier images.
            self.displayed.clear();
            self.admitted = Arc::downgrade(snapshot);
        }
        for drawing in &snapshot.drawings {
            let surface = drawing.window;
            if admitted {
                if self.windows.get(surface).is_some_and(|w| w.scene_revision < drawing.revision) {
                    let _ = self.windows.publish_scene(
                        surface,
                        drawing.submission.address.generation,
                        drawing.revision,
                    );
                }
                self.resolve(
                    surface.context().session(),
                    drawing.submission,
                    PresentationOutcome::Presented,
                );
                self.displayed.insert(surface, drawing.clone());
            }
            if !self
                .submissions
                .iter()
                .any(|(id, s)| id.surface() == surface && *s == drawing.submission)
            {
                self.supersede(surface.context().session(), drawing.submission);
            }
        }
        drop(admission);
        self.assets.retain(|_, (token, _)| token.strong_count() != 0);
        self.refresh_hover();
        self.retire_stale_semantics();
        admitted
    }
}

mod retained;

#[cfg(test)]
mod tests;
