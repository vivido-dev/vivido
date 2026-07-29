//! Scheduler for emitting events at a specific time in the future.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::context::ContextId;
use crate::event::Event;
use crate::runtime::RuntimeProxy;

/// ID uniquely identifying a timer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TimerId {
    topic: Topic,
    context_id: ContextId,
}

impl TimerId {
    pub fn new(topic: Topic, context_id: ContextId) -> Self {
        Self { topic, context_id }
    }
}

/// Available timer topics.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Topic {
    SelectionScrolling,
    DelayedSearch,
    BlinkCursor,
    BlinkTimeout,
    Frame,
    VividResizeSettled,
    #[cfg(unix)]
    ScreenshotReadback,
    #[cfg(unix)]
    Automation,
}

/// Event scheduled to be emitted at a specific time.
pub struct Timer {
    pub deadline: Instant,
    pub event: Event,
    pub id: TimerId,

    interval: Option<Duration>,
}

/// Scheduler tracking all pending timers.
pub struct Scheduler {
    timers: VecDeque<Timer>,
    event_proxy: RuntimeProxy,
}

impl Scheduler {
    pub fn new(event_proxy: RuntimeProxy) -> Self {
        Self { timers: VecDeque::new(), event_proxy }
    }

    /// Process all pending timers.
    ///
    /// If there are still timers pending after all ready events have been processed, the closest
    /// pending deadline will be returned.
    pub fn update(&mut self) -> Option<Instant> {
        let now = Instant::now();

        while !self.timers.is_empty() && self.timers[0].deadline <= now {
            if let Some(timer) = self.timers.pop_front() {
                // Automatically repeat the event.
                if let Some(interval) = timer.interval {
                    self.schedule(timer.event.clone(), interval, true, timer.id);
                }

                let _ = self.event_proxy.send_event(timer.event);
            }
        }

        self.timers.front().map(|timer| timer.deadline)
    }

    /// Schedule a new event.
    pub fn schedule(&mut self, event: Event, interval: Duration, repeat: bool, timer_id: TimerId) {
        let deadline = Instant::now() + interval;

        // Get insert position in the schedule.
        let index = self
            .timers
            .iter()
            .position(|timer| timer.deadline > deadline)
            .unwrap_or(self.timers.len());

        // Set the automatic event repeat rate.
        let interval = if repeat { Some(interval) } else { None };

        self.timers.insert(index, Timer { interval, deadline, event, id: timer_id });
    }

    /// Cancel a scheduled event.
    pub fn unschedule(&mut self, id: TimerId) -> Option<Timer> {
        let index = self.timers.iter().position(|timer| timer.id == id)?;
        self.timers.remove(index)
    }

    /// Check if a timer is already scheduled.
    pub fn scheduled(&mut self, id: TimerId) -> bool {
        self.timers.iter().any(|timer| timer.id == id)
    }

    /// Remove all timers scheduled for a window.
    ///
    /// This must be called when a window is removed to ensure that timers on intervals do not
    /// stick around forever and cause a memory leak.
    pub fn unschedule_window(&mut self, context_id: ContextId) {
        self.timers.retain(|timer| timer.id.context_id != context_id);
    }
}

#[cfg(test)]
mod tests {
    use super::{Scheduler, TimerId, Topic};
    use crate::context::ContextId;
    use crate::event::{Event, EventType};
    use crate::runtime::RuntimeProxy;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn timers_are_ordered_and_removed_by_internal_context() {
        let (sender, _receiver) = mpsc::channel();
        let mut scheduler = Scheduler::new(RuntimeProxy::Headless(sender));
        let first = ContextId::new(1);
        let second = ContextId::new(2);

        scheduler.schedule(
            Event::new(EventType::Frame, first),
            Duration::from_secs(2),
            false,
            TimerId::new(Topic::Frame, first),
        );
        scheduler.schedule(
            Event::new(EventType::Frame, second),
            Duration::from_secs(1),
            false,
            TimerId::new(Topic::Frame, second),
        );

        assert_eq!(scheduler.timers.front().unwrap().id.context_id, second);
        scheduler.unschedule_window(second);
        assert_eq!(scheduler.timers.front().unwrap().id.context_id, first);
    }
}
