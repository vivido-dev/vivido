//! Exact committed scenes for independently checked peer compositions.

// Rust guideline compliant 2026-10-07
use super::*;

fn same(a: &Drawing, b: &Drawing) -> bool {
    a.window == b.window
        && a.window_revision == b.window_revision
        && a.revision == b.revision
        && a.submission == b.submission
        && a.bounds == b.bounds
        && a.scale.to_bits() == b.scale.to_bits()
        && Arc::ptr_eq(&a.compiled, &b.compiled)
}

impl Host {
    /// Reserves fresh peers while selected windows retain exact committed content.
    ///
    /// `previous` must still describe this host's complete latest admitted image.
    /// Selected windows retain original resources and submissions; their newer
    /// accepted replacements remain pending. Placement, viewport, target, lane,
    /// source incarnation, channel and retirement guards must remain current.
    /// The ordinary two-reservation limit is unchanged: one diagnostic frame
    /// and one peer output. Actual imported-image sampling still controls finish.
    pub(crate) fn prepare_retaining(
        &mut self,
        scene: &SharedScene,
        previous: &Arc<Snapshot>,
        retained: &[SurfaceIdentity],
    ) -> Option<Arc<Snapshot>> {
        if self.reserved.len() >= MAX_RESERVATIONS
            || !self.admitted.upgrade().is_some_and(|s| Arc::ptr_eq(&s, previous))
            || !self.current(scene, previous)
            || previous.drawings.len() != self.displayed.len()
            || previous.drawings.iter().any(|drawing| {
                self.displayed.get(&drawing.window).is_none_or(|shown| !same(drawing, shown))
            })
            || retained.len() > previous.drawings.len()
            || retained.iter().enumerate().any(|(index, window)| {
                retained[..index].contains(window)
                    || !previous.drawings.iter().any(|drawing| drawing.window == *window)
            })
        {
            return None;
        }
        let mut candidate = self.candidate(scene);
        if retained.iter().any(|window| !candidate.drawings.iter().any(|d| d.window == *window)) {
            return None;
        }
        for (drawing, binding) in candidate.drawings.iter_mut().zip(&mut candidate.bindings) {
            if retained.contains(&drawing.window) {
                let (committed, guard) = previous
                    .drawings
                    .iter()
                    .zip(&previous.bindings)
                    .find(|(old, _)| old.window == drawing.window)?;
                *drawing = committed.clone();
                *binding = guard.clone();
            }
        }
        let candidate = Arc::new(candidate);
        if !self.current(scene, &candidate) {
            return None;
        }
        self.reserved.push(candidate.clone());
        Some(candidate)
    }
}
