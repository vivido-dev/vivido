//! OSC 9;4 progress bar drawn across the top of the terminal.
//!
//! Programs such as `winget`, systemd, and agent CLIs report progress with ConEmu's
//! `OSC 9;4;<state>[;<percent>]`. [`ProgressBar`] folds those reports into per-window state and
//! projects it into a [`ProgressVisual`] once per frame: a determinate fill that eases toward the
//! reported percent, or an indeterminate segment bouncing over a faint track.
//!
//! A report goes stale after [`STALE_AFTER`] without another one, so a tool that crashed
//! mid-operation does not leave a bar behind forever. The owning window schedules a timer for
//! [`ProgressBar::deadline`] and calls [`ProgressBar::expire`] when it fires.
//!
//! Outside the surface, [`ProgressBar::current`] is what a window, taskbar button, Dock tile, or
//! an embedding host's tab mirrors, and [`most_urgent`] folds several surfaces into one.

use std::time::{Duration, Instant};

use crate::osc_notification::{ProgressReport, ProgressState};

/// Bar thickness in logical pixels.
pub const BAR_HEIGHT: f32 = 2.;
/// How long a report stays visible without a follow-up report.
pub const STALE_AFTER: Duration = Duration::from_secs(15);
/// How long the determinate fill takes to ease to a new percent.
const FILL_ANIMATION: Duration = Duration::from_millis(200);
/// One sweep of the indeterminate segment from one edge to the other.
const BOUNCE_SWEEP: Duration = Duration::from_millis(1200);
/// Width of the indeterminate segment as a fraction of the bar.
pub const SEGMENT_FRACTION: f32 = 0.25;
/// Opacity of the track the indeterminate segment runs over.
pub const TRACK_ALPHA: f32 = 0.3;

/// The colour role of a determinate bar.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProgressTone {
    Normal,
    Paused,
    Error,
}

/// What a frame draws for the current progress state.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ProgressVisual {
    /// Filled from the left edge to `fraction` (`0..=1`) of the width.
    Fill { tone: ProgressTone, fraction: f32 },
    /// A [`SEGMENT_FRACTION`]-wide segment starting at `offset` (`0..=1 - SEGMENT_FRACTION`).
    Bounce { offset: f32 },
}

/// The reported progress of one surface, for mirroring outside it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub kind: ProgressKind,
    /// Percent complete in `0..=100`; `None` only for [`ProgressKind::Indeterminate`].
    pub percent: Option<u8>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProgressKind {
    Normal,
    Indeterminate,
    Paused,
    Error,
}

impl ProgressKind {
    /// How strongly a surface in this state asks for attention when states are combined.
    fn urgency(self) -> u8 {
        match self {
            Self::Indeterminate => 0,
            Self::Normal => 1,
            Self::Paused => 2,
            Self::Error => 3,
        }
    }
}

/// Fold several surfaces' progress into the one a shared indicator shows.
///
/// An error outranks a pause, which outranks running work, and a known percent outranks an
/// indeterminate one. Among equals the least complete surface wins, since the combined work is
/// not done until it is.
pub fn most_urgent(progress: impl IntoIterator<Item = Progress>) -> Option<Progress> {
    progress.into_iter().max_by_key(|progress| {
        (progress.kind.urgency(), std::cmp::Reverse(progress.percent.unwrap_or(0)))
    })
}

/// Short text for a badge that cannot draw a bar, such as the macOS Dock tile's.
pub fn badge_label(progress: Progress) -> String {
    let percent = progress.percent.unwrap_or(0);
    match progress.kind {
        ProgressKind::Normal => format!("{percent}%"),
        ProgressKind::Indeterminate => String::from("…"),
        ProgressKind::Paused => format!("{percent}% ‖"),
        ProgressKind::Error => String::from("!"),
    }
}

#[derive(Clone, Copy, Debug)]
struct Active {
    /// Never [`ProgressState::Remove`]; removal clears the bar instead.
    state: ProgressState,
    /// The percent the last report resolved to, if the state has one.
    percent: Option<u8>,
    /// Fill fraction the current ease started from.
    from: f32,
    /// When the fill fraction or the indeterminate sweep last (re)started.
    changed_at: Instant,
    /// When the last report arrived, for staleness.
    updated_at: Instant,
}

/// Progress state for one terminal surface.
#[derive(Debug, Default)]
pub struct ProgressBar {
    active: Option<Active>,
}

impl ProgressBar {
    /// Fold one report into the bar.
    ///
    /// Returns whether the reported state or percent changed. Tools re-send the same report as a
    /// keepalive; those refresh staleness but are not a change worth announcing.
    pub(crate) fn apply(&mut self, report: ProgressReport, now: Instant) -> bool {
        let previous = self.active;
        let Some(state) = (report.state != ProgressState::Remove).then_some(report.state) else {
            return self.active.take().is_some();
        };

        // Error and pause keep the last known percent when they carry none, and fill the whole
        // bar when there never was one, so the colour is still visible.
        let percent = match state {
            ProgressState::Normal => Some(report.percent.unwrap_or(0)),
            ProgressState::Error | ProgressState::Paused => {
                report.percent.or_else(|| previous.and_then(|active| active.percent)).or(Some(100))
            },
            ProgressState::Indeterminate => None,
            ProgressState::Remove => unreachable!("removal returned above"),
        };

        let changed =
            previous.is_none_or(|active| (active.state, active.percent) != (state, percent));
        let restarts = previous.is_none_or(|active| {
            active.percent != percent
                || (state == ProgressState::Indeterminate)
                    != (active.state == ProgressState::Indeterminate)
        });
        let (from, changed_at) = match previous {
            Some(active) if !restarts => (active.from, active.changed_at),
            // Ease from wherever the fill currently is; a bar appearing grows from the left.
            _ => (self.fill_fraction_at(now).unwrap_or(0.), now),
        };

        self.active = Some(Active { state, percent, from, changed_at, updated_at: now });
        changed
    }

    /// Remove the bar, returning whether one was showing.
    pub fn clear(&mut self) -> bool {
        self.active.take().is_some()
    }

    /// Remove a bar whose last report is older than [`STALE_AFTER`], returning whether it did.
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.active.is_some_and(|active| is_stale(&active, now)) {
            self.active = None;
            return true;
        }
        false
    }

    /// When the current report goes stale.
    pub fn deadline(&self) -> Option<Instant> {
        self.active.map(|active| active.updated_at + STALE_AFTER)
    }

    /// The visual a frame drawn at `now` shows.
    pub fn visual_at(&self, now: Instant) -> Option<ProgressVisual> {
        let active = self.active.filter(|active| !is_stale(active, now))?;
        let tone = match active.state {
            ProgressState::Indeterminate => {
                return Some(ProgressVisual::Bounce { offset: bounce_offset(&active, now) });
            },
            ProgressState::Error => ProgressTone::Error,
            ProgressState::Paused => ProgressTone::Paused,
            ProgressState::Normal | ProgressState::Remove => ProgressTone::Normal,
        };
        Some(ProgressVisual::Fill { tone, fraction: fill_fraction(&active, now) })
    }

    /// Whether frames drawn after `now` differ from one drawn at `now`.
    pub fn is_animating_at(&self, now: Instant) -> bool {
        self.active.is_some_and(|active| {
            !is_stale(&active, now)
                && (active.state == ProgressState::Indeterminate
                    || now.saturating_duration_since(active.changed_at) < FILL_ANIMATION)
        })
    }

    /// The progress a frame drawn at `now` shows, for mirroring outside the surface.
    pub fn current(&self, now: Instant) -> Option<Progress> {
        let active = self.active.filter(|active| !is_stale(active, now))?;
        let kind = match active.state {
            ProgressState::Normal | ProgressState::Remove => ProgressKind::Normal,
            ProgressState::Indeterminate => ProgressKind::Indeterminate,
            ProgressState::Paused => ProgressKind::Paused,
            ProgressState::Error => ProgressKind::Error,
        };
        Some(Progress { kind, percent: active.percent })
    }

    /// The reported state for automation: `state` is `none` when no bar is showing.
    ///
    /// A stale report reads as `none` even before its expiry timer has fired, matching what is
    /// drawn.
    pub fn automation_json(&self) -> serde_json::Value {
        let (state, percent) = match self.active.filter(|active| !is_stale(active, Instant::now()))
        {
            None => ("none", None),
            Some(active) => (state_name(active.state), active.percent),
        };
        serde_json::json!({"state": state, "percent": percent})
    }

    fn fill_fraction_at(&self, now: Instant) -> Option<f32> {
        self.active
            .filter(|active| active.state != ProgressState::Indeterminate && !is_stale(active, now))
            .map(|active| fill_fraction(&active, now))
    }
}

fn is_stale(active: &Active, now: Instant) -> bool {
    now.saturating_duration_since(active.updated_at) >= STALE_AFTER
}

fn fill_fraction(active: &Active, now: Instant) -> f32 {
    let target = f32::from(active.percent.unwrap_or(0)) / 100.;
    let elapsed = now.saturating_duration_since(active.changed_at).as_secs_f32();
    let t = (elapsed / FILL_ANIMATION.as_secs_f32()).min(1.);
    // Ease-out: fast start, gentle landing on the reported value.
    let eased = 1. - (1. - t) * (1. - t);
    active.from + (target - active.from) * eased
}

fn bounce_offset(active: &Active, now: Instant) -> f32 {
    let sweep = BOUNCE_SWEEP.as_secs_f32();
    let elapsed = now.saturating_duration_since(active.changed_at).as_secs_f32();
    // Triangle wave over two sweeps: out, then back.
    let phase = (elapsed / sweep) % 2.;
    let t = if phase > 1. { 2. - phase } else { phase };
    // Ease-in-out so the segment slows at each edge before turning around.
    let eased = t * t * (3. - 2. * t);
    eased * (1. - SEGMENT_FRACTION)
}

fn state_name(state: ProgressState) -> &'static str {
    match state {
        ProgressState::Remove => "none",
        ProgressState::Normal => "normal",
        ProgressState::Error => "error",
        ProgressState::Indeterminate => "indeterminate",
        ProgressState::Paused => "paused",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(state: ProgressState, percent: Option<u8>) -> ProgressReport {
        ProgressReport { state, percent }
    }

    #[test]
    fn determinate_fill_eases_to_the_reported_percent() {
        let start = Instant::now();
        let mut bar = ProgressBar::default();
        assert!(bar.apply(report(ProgressState::Normal, Some(40)), start));

        let Some(ProgressVisual::Fill { tone, fraction }) = bar.visual_at(start) else {
            panic!("a determinate report fills");
        };
        assert_eq!(tone, ProgressTone::Normal);
        assert_eq!(fraction, 0., "a new bar grows from the left edge");
        assert!(bar.is_animating_at(start));

        let settled = start + FILL_ANIMATION;
        assert_eq!(
            bar.visual_at(settled),
            Some(ProgressVisual::Fill { tone: ProgressTone::Normal, fraction: 0.4 })
        );
        assert!(!bar.is_animating_at(settled), "a settled determinate bar needs no frames");
    }

    #[test]
    fn a_repeated_report_refreshes_staleness_without_restarting_the_fill() {
        let start = Instant::now();
        let mut bar = ProgressBar::default();
        bar.apply(report(ProgressState::Normal, Some(50)), start);

        let later = start + Duration::from_secs(10);
        assert!(
            !bar.apply(report(ProgressState::Normal, Some(50)), later),
            "no change to announce"
        );
        assert!(!bar.is_animating_at(later), "the same percent does not re-run the ease");
        assert_eq!(bar.deadline(), Some(later + STALE_AFTER));
        assert!(bar.visual_at(start + STALE_AFTER + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn a_new_percent_eases_from_the_current_fill() {
        let start = Instant::now();
        let mut bar = ProgressBar::default();
        bar.apply(report(ProgressState::Normal, Some(20)), start);
        let later = start + Duration::from_secs(1);
        assert!(bar.apply(report(ProgressState::Normal, Some(80)), later));
        assert_eq!(
            bar.visual_at(later),
            Some(ProgressVisual::Fill { tone: ProgressTone::Normal, fraction: 0.2 })
        );
    }

    #[test]
    fn error_and_pause_keep_the_last_percent_or_fill_the_bar() {
        let start = Instant::now();
        let settled = start + FILL_ANIMATION;
        let mut bar = ProgressBar::default();
        bar.apply(report(ProgressState::Paused, None), start);
        assert_eq!(
            bar.visual_at(settled),
            Some(ProgressVisual::Fill { tone: ProgressTone::Paused, fraction: 1. })
        );

        bar.apply(report(ProgressState::Normal, Some(30)), settled);
        let resettled = settled + FILL_ANIMATION;
        bar.apply(report(ProgressState::Error, None), resettled);
        let Some(ProgressVisual::Fill { tone, fraction }) = bar.visual_at(resettled) else {
            panic!("error with a known percent fills");
        };
        assert_eq!(tone, ProgressTone::Error);
        assert!((fraction - 0.3).abs() < 1e-6, "error keeps the fill where it was: {fraction}");
        assert!(!bar.is_animating_at(resettled), "only the colour changed");
        assert_eq!(bar.automation_json(), serde_json::json!({"state": "error", "percent": 30}));
    }

    #[test]
    fn indeterminate_bounces_between_the_edges_and_keeps_animating() {
        let start = Instant::now();
        let mut bar = ProgressBar::default();
        bar.apply(report(ProgressState::Indeterminate, None), start);

        let at = |elapsed: Duration| match bar.visual_at(start + elapsed) {
            Some(ProgressVisual::Bounce { offset }) => offset,
            visual => panic!("indeterminate draws a bouncing segment, got {visual:?}"),
        };
        assert_eq!(at(Duration::ZERO), 0.);
        assert!((at(BOUNCE_SWEEP) - (1. - SEGMENT_FRACTION)).abs() < 1e-4, "reaches the far edge");
        assert!(at(BOUNCE_SWEEP * 2) < 1e-3, "and comes back");
        assert!(bar.is_animating_at(start + Duration::from_secs(5)));
        assert_eq!(
            bar.automation_json(),
            serde_json::json!({"state": "indeterminate", "percent": null})
        );
    }

    #[test]
    fn remove_and_staleness_clear_the_bar() {
        let start = Instant::now();
        let mut bar = ProgressBar::default();
        assert!(!bar.apply(report(ProgressState::Remove, None), start), "nothing to remove");

        bar.apply(report(ProgressState::Indeterminate, None), start);
        assert!(!bar.expire(start + STALE_AFTER - Duration::from_millis(1)));
        let stale = start + STALE_AFTER;
        assert_eq!(bar.visual_at(stale), None, "a stale bar is not drawn even before the timer");
        assert!(!bar.is_animating_at(stale), "and does not keep requesting frames");
        assert!(bar.expire(stale));
        assert_eq!(bar.automation_json(), serde_json::json!({"state": "none", "percent": null}));

        bar.apply(report(ProgressState::Normal, Some(10)), stale);
        assert!(bar.apply(report(ProgressState::Remove, None), stale));
        assert_eq!(bar.visual_at(stale), None);
        assert_eq!(bar.deadline(), None);
    }

    #[test]
    fn current_mirrors_the_resolved_report_until_it_goes_stale() {
        let start = Instant::now();
        let mut bar = ProgressBar::default();
        assert_eq!(bar.current(start), None);

        bar.apply(report(ProgressState::Normal, Some(40)), start);
        bar.apply(report(ProgressState::Paused, None), start);
        assert_eq!(
            bar.current(start),
            Some(Progress { kind: ProgressKind::Paused, percent: Some(40) }),
            "the mirror shows the reported percent, not the easing fill"
        );
        assert_eq!(bar.current(start + STALE_AFTER), None);
    }

    #[test]
    fn most_urgent_prefers_errors_then_pauses_then_the_least_complete_work() {
        let progress = |kind, percent| Progress { kind, percent };
        let busy = progress(ProgressKind::Indeterminate, None);
        let early = progress(ProgressKind::Normal, Some(10));
        let late = progress(ProgressKind::Normal, Some(90));
        let paused = progress(ProgressKind::Paused, Some(95));
        let failed = progress(ProgressKind::Error, Some(100));

        assert_eq!(most_urgent([]), None);
        assert_eq!(most_urgent([busy]), Some(busy));
        assert_eq!(most_urgent([busy, late, early]), Some(early));
        assert_eq!(most_urgent([early, paused, late]), Some(paused));
        assert_eq!(most_urgent([paused, failed, busy]), Some(failed));
    }

    #[test]
    fn badge_labels_stay_short() {
        let label = |kind, percent| badge_label(Progress { kind, percent });
        assert_eq!(label(ProgressKind::Normal, Some(42)), "42%");
        assert_eq!(label(ProgressKind::Indeterminate, None), "…");
        assert_eq!(label(ProgressKind::Paused, Some(7)), "7% ‖");
        assert_eq!(label(ProgressKind::Error, Some(50)), "!");
    }
}
