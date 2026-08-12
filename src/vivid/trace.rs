//! Bounded, metadata-only automation trace for the Vivid presenter.

use std::collections::VecDeque;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vivid_protocol::identity::TrackIdentity;

pub const MAX_EVENTS: usize = 4096;
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_QUERY_EVENTS: u16 = 512;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceCategory {
    Connection,
    Lifecycle,
    Flow,
    Playback,
    Recovery,
    Decode,
    Render,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraceTrackIdentity {
    pub session_id: u64,
    pub context_id: u64,
    pub surface_id: u64,
    pub track_id: u64,
}

impl From<TrackIdentity> for TraceTrackIdentity {
    fn from(identity: TrackIdentity) -> Self {
        Self {
            session_id: identity.surface.context.session.session_id,
            context_id: identity.surface.context.context_id,
            surface_id: identity.surface.surface_id,
            track_id: identity.track_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceEvent {
    pub sequence: u64,
    pub process_id: u32,
    pub process_instance_id: String,
    pub startup_wall_clock_unix_us: u64,
    pub monotonic_us: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_sequence: Option<u64>,
    pub category: TraceCategory,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<TraceTrackIdentity>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraceFilter {
    pub session_id: Option<u64>,
    pub context_id: Option<u64>,
    pub surface_id: Option<u64>,
    pub track_id: Option<u64>,
    pub category: Option<TraceCategory>,
    pub recovery_only: bool,
}

impl TraceFilter {
    fn matches(self, event: &TraceEvent) -> bool {
        if self.recovery_only && event.category != TraceCategory::Recovery {
            return false;
        }
        if self.category.is_some_and(|category| category != event.category) {
            return false;
        }
        let identity = event.track;
        self.session_id.is_none_or(|value| identity.map(|id| id.session_id) == Some(value))
            && self.context_id.is_none_or(|value| identity.map(|id| id.context_id) == Some(value))
            && self.surface_id.is_none_or(|value| identity.map(|id| id.surface_id) == Some(value))
            && self.track_id.is_none_or(|value| identity.map(|id| id.track_id) == Some(value))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraceGap {
    pub requested_sequence: u64,
    pub oldest_sequence: u64,
    pub current_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceBatch {
    pub schema_version: u32,
    pub instance_id: String,
    pub started_unix_us: u64,
    pub captured_unix_us: u64,
    pub captured_monotonic_us: u64,
    pub oldest_sequence: u64,
    pub current_sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap: Option<TraceGap>,
    pub events: Vec<TraceEvent>,
}

struct StoredEvent {
    encoded_bytes: usize,
    event: TraceEvent,
}

pub struct TraceJournal {
    started: Instant,
    started_unix_us: u64,
    instance_id: String,
    next_sequence: u64,
    encoded_bytes: usize,
    events: VecDeque<StoredEvent>,
    maximum_events: usize,
    maximum_bytes: usize,
}

impl Default for TraceJournal {
    fn default() -> Self {
        Self::with_limits(MAX_EVENTS, MAX_BYTES)
    }
}

impl TraceJournal {
    fn with_limits(maximum_events: usize, maximum_bytes: usize) -> Self {
        let mut random = [0_u8; 16];
        if getrandom::fill(&mut random).is_err() {
            random[..4].copy_from_slice(&std::process::id().to_be_bytes());
        }
        Self {
            started: Instant::now(),
            started_unix_us: unix_us(),
            instance_id: random.iter().map(|byte| format!("{byte:02x}")).collect(),
            next_sequence: 0,
            encoded_bytes: 0,
            events: VecDeque::new(),
            maximum_events,
            maximum_bytes,
        }
    }

    pub fn push(
        &mut self,
        category: TraceCategory,
        event: impl Into<String>,
        track: Option<TrackIdentity>,
        data: Value,
    ) {
        self.next_sequence = self.next_sequence.saturating_add(1);
        let recovery_sequence = (category == TraceCategory::Recovery).then_some(self.next_sequence);
        let event = TraceEvent {
            sequence: self.next_sequence,
            process_id: std::process::id(),
            process_instance_id: self.instance_id.clone(),
            startup_wall_clock_unix_us: self.started_unix_us,
            monotonic_us: elapsed_us(self.started),
            recovery_sequence,
            category,
            event: event.into(),
            track: track.map(Into::into),
            data,
        };
        let encoded_bytes = serde_json::to_vec(&event).map_or(0, |bytes| bytes.len());
        if encoded_bytes > self.maximum_bytes {
            return;
        }
        self.encoded_bytes = self.encoded_bytes.saturating_add(encoded_bytes);
        self.events.push_back(StoredEvent { encoded_bytes, event });
        while self.events.len() > self.maximum_events || self.encoded_bytes > self.maximum_bytes {
            if let Some(removed) = self.events.pop_front() {
                self.encoded_bytes = self.encoded_bytes.saturating_sub(removed.encoded_bytes);
            } else {
                break;
            }
        }
    }

    pub fn query(
        &self,
        after_sequence: Option<u64>,
        limit: u16,
        filter: TraceFilter,
    ) -> TraceBatch {
        let oldest_sequence = self
            .events
            .front()
            .map_or(self.next_sequence.saturating_add(1), |stored| stored.event.sequence);
        let requested = after_sequence.unwrap_or(oldest_sequence.saturating_sub(1));
        let gap = (after_sequence.is_some() && requested < oldest_sequence.saturating_sub(1))
            .then_some(TraceGap {
                requested_sequence: requested,
                oldest_sequence,
                current_sequence: self.next_sequence,
            });
        let events = self
            .events
            .iter()
            .filter(|stored| stored.event.sequence > requested)
            .filter(|stored| filter.matches(&stored.event))
            .take(usize::from(limit.min(MAX_QUERY_EVENTS)))
            .map(|stored| stored.event.clone())
            .collect();
        TraceBatch {
            schema_version: 1,
            instance_id: self.instance_id.clone(),
            started_unix_us: self.started_unix_us,
            captured_unix_us: unix_us(),
            captured_monotonic_us: elapsed_us(self.started),
            oldest_sequence,
            current_sequence: self.next_sequence,
            gap,
            events,
        }
    }
}

fn elapsed_us(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_micros()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_journal_reports_eviction_and_filters_recovery() {
        let mut journal = TraceJournal::with_limits(2, 64 * 1024);
        journal.push(TraceCategory::Flow, "flow", None, Value::Null);
        journal.push(TraceCategory::Recovery, "requested", None, Value::Null);
        journal.push(TraceCategory::Recovery, "recovered", None, Value::Null);

        let batch = journal.query(
            Some(0),
            16,
            TraceFilter { recovery_only: true, ..TraceFilter::default() },
        );
        assert!(batch.gap.is_some());
        assert_eq!(batch.events.len(), 2);
        assert!(batch.events.iter().all(|event| event.category == TraceCategory::Recovery));
    }
}
