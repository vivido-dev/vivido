//! Event-loop-owned state for agent automation.

use std::collections::{HashSet, VecDeque};
use std::process::ExitStatus;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};

use crate::polling::ipc::{IpcConnection, IpcError, MAX_SUBSCRIPTIONS, SubscriptionEventEnvelope};

pub const TRANSCRIPT_CAPACITY: usize = 1024 * 1024;
pub const EVENT_REPLAY_BYTES: usize = 4 * 1024 * 1024;
pub const EVENT_REPLAY_COUNT: usize = 4096;
pub const SCREEN_HISTORY_COUNT: usize = 1024;
pub const OUTPUT_EVENT_CHUNK: usize = 64 * 1024;

pub struct SubscriptionRequest {
    pub target: Option<u64>,
    pub all_windows: bool,
    pub kinds: HashSet<String>,
    pub since_event: Option<u64>,
    pub current_sequences: Value,
}

/// A sanitized, byte-exact rolling PTY transcript with monotonic offsets.
#[derive(Debug)]
pub struct Transcript {
    bytes: VecDeque<u8>,
    oldest_offset: u64,
    end_offset: u64,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            bytes: VecDeque::with_capacity(TRANSCRIPT_CAPACITY),
            oldest_offset: 0,
            end_offset: 0,
        }
    }
}

impl Transcript {
    /// Append bytes after the authenticated Vivid marker scanner has removed marker envelopes.
    pub fn append(&mut self, bytes: &[u8]) -> (u64, u64) {
        let start = self.end_offset;
        self.end_offset = self.end_offset.saturating_add(bytes.len() as u64);
        if bytes.len() >= TRANSCRIPT_CAPACITY {
            self.bytes.clear();
            self.bytes.extend(&bytes[bytes.len() - TRANSCRIPT_CAPACITY..]);
        } else {
            let overflow =
                self.bytes.len().saturating_add(bytes.len()).saturating_sub(TRANSCRIPT_CAPACITY);
            self.bytes.drain(..overflow);
            self.bytes.extend(bytes);
        }
        self.oldest_offset = self.end_offset.saturating_sub(self.bytes.len() as u64);
        (start, self.end_offset)
    }

    pub fn oldest_offset(&self) -> u64 {
        self.oldest_offset
    }

    pub fn end_offset(&self) -> u64 {
        self.end_offset
    }

    /// Read retained bytes starting at an absolute offset.
    pub fn range(&self, start: u64, max_bytes: usize) -> Result<Vec<u8>, IpcError> {
        if start < self.oldest_offset {
            return Err(IpcError::new(
                "sequence_gap",
                format!(
                    "transcript offset {start} was evicted; oldest retained offset is {}",
                    self.oldest_offset
                ),
            )
            .with_data(json!({
                "requested_offset": start,
                "oldest_offset": self.oldest_offset,
                "end_offset": self.end_offset,
            })));
        }
        let relative = usize::try_from(start.saturating_sub(self.oldest_offset))
            .unwrap_or(usize::MAX)
            .min(self.bytes.len());
        Ok(self.bytes.iter().skip(relative).take(max_bytes).copied().collect())
    }

    /// Read the newest bytes, or bytes after an explicit absolute offset.
    pub fn snapshot(
        &self,
        after_offset: Option<u64>,
        max_bytes: usize,
    ) -> Result<TranscriptSnapshot, IpcError> {
        let start = match after_offset {
            Some(offset) => offset,
            None => self.end_offset.saturating_sub(max_bytes as u64).max(self.oldest_offset),
        };
        let data = self.range(start, max_bytes)?;
        let returned_end = start.saturating_add(data.len() as u64);
        Ok(TranscriptSnapshot {
            oldest_offset: self.oldest_offset,
            start_offset: start,
            end_offset: self.end_offset,
            returned_end_offset: returned_end,
            truncated: start > self.oldest_offset || returned_end < self.end_offset,
            data,
        })
    }
}

#[derive(Debug)]
pub struct TranscriptSnapshot {
    pub oldest_offset: u64,
    pub start_offset: u64,
    pub end_offset: u64,
    pub returned_end_offset: u64,
    pub truncated: bool,
    pub data: Vec<u8>,
}

impl TranscriptSnapshot {
    pub fn json(&self) -> Value {
        json!({
            "oldest_offset": self.oldest_offset,
            "start_offset": self.start_offset,
            "returned_end_offset": self.returned_end_offset,
            "end_offset": self.end_offset,
            "truncated": self.truncated,
            "data": base64::engine::general_purpose::STANDARD.encode(&self.data),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ScreenChange {
    pub sequence: u64,
    /// `None` means the entire viewport was invalidated.
    pub rows: Option<Vec<u16>>,
}

#[derive(Debug)]
pub struct AutomationWindowState {
    pub creation_index: u64,
    pub screen_sequence: u64,
    pub frame_sequence: u64,
    pub focus_confirmation: u64,
    pub resize_confirmation: u64,
    pub pending_focus_confirmations: u64,
    pub pending_resize_confirmations: u64,
    pub last_screen_change: Instant,
    pub screen_history: VecDeque<ScreenChange>,
    pub row_hashes: Vec<u64>,
    pub screen_metadata_hash: u64,
    pub transcript: Arc<Mutex<Transcript>>,
    pub exit_status: Option<ExitStatus>,
    pub waiters: Vec<Waiter>,
    pub pending_writes: Vec<PendingWrite>,
}

impl AutomationWindowState {
    pub fn new(creation_index: u64, transcript: Arc<Mutex<Transcript>>) -> Self {
        Self {
            creation_index,
            screen_sequence: 0,
            frame_sequence: 0,
            focus_confirmation: 0,
            resize_confirmation: 0,
            pending_focus_confirmations: 0,
            pending_resize_confirmations: 0,
            last_screen_change: Instant::now(),
            screen_history: VecDeque::with_capacity(SCREEN_HISTORY_COUNT),
            row_hashes: Vec::new(),
            screen_metadata_hash: 0,
            transcript,
            exit_status: None,
            waiters: Vec::new(),
            pending_writes: Vec::new(),
        }
    }

    pub fn record_screen_change(&mut self, rows: Option<Vec<u16>>) -> u64 {
        self.screen_sequence = self.screen_sequence.saturating_add(1);
        self.last_screen_change = Instant::now();
        self.screen_history.push_back(ScreenChange { sequence: self.screen_sequence, rows });
        while self.screen_history.len() > SCREEN_HISTORY_COUNT {
            self.screen_history.pop_front();
        }
        self.screen_sequence
    }

    pub fn record_frame(&mut self) -> u64 {
        self.frame_sequence = self.frame_sequence.saturating_add(1);
        self.frame_sequence
    }
}

#[derive(Debug)]
pub struct PendingWrite {
    pub token: u64,
    pub bytes: usize,
    pub connection: IpcConnection,
    pub request_id: u64,
    pub deadline: Instant,
}

#[derive(Debug)]
pub struct Waiter {
    pub connection: IpcConnection,
    pub request_id: u64,
    pub deadline: Instant,
    pub kind: WaitKind,
}

#[derive(Debug)]
pub enum WaitKind {
    Text {
        pattern: String,
        regex: bool,
        after_screen: Option<u64>,
    },
    Output {
        pattern: Vec<u8>,
        regex: bool,
        start_offset: u64,
    },
    ScreenChange {
        after: u64,
    },
    ScreenStable {
        quiet: Duration,
        after_screen: Option<u64>,
    },
    Frame {
        after: u64,
    },
    Exit,
    Resize {
        columns: Option<u16>,
        rows: Option<u16>,
        width: u32,
        height: u32,
        after_resize: u64,
        pty_token: Option<u64>,
        pty_complete: bool,
    },
    Focus {
        after_focus: u64,
    },
}

#[derive(Clone, Debug)]
struct StoredEvent {
    sequence: u64,
    window_id: Option<u64>,
    kind: String,
    data: Value,
    encoded_size: usize,
}

#[derive(Debug)]
struct Subscriber {
    id: u64,
    connection: IpcConnection,
    /// `None` means all windows. A targeted subscription always contains a concrete ID.
    window_id: Option<u64>,
    all_windows: bool,
    kinds: HashSet<String>,
    overflow: Option<(u64, u64)>,
    queued_events: Arc<AtomicUsize>,
}

impl Subscriber {
    fn matches(&self, event: &StoredEvent) -> bool {
        if !self.kinds.is_empty() && !self.kinds.contains(&event.kind) {
            return false;
        }
        if self.all_windows {
            return true;
        }
        event.window_id == self.window_id
    }

    fn send(&mut self, event: &StoredEvent) {
        if let Some((start, end)) = self.overflow {
            let overflow = SubscriptionEventEnvelope {
                version: 1,
                subscription_id: self.id,
                event_sequence: end,
                window_id: self.window_id,
                event: json!({
                    "type": "overflow",
                    "data": {"first_dropped_sequence": start, "last_dropped_sequence": end},
                }),
            };
            if self.connection.event(overflow, &self.queued_events).is_err() {
                self.overflow = Some((start, event.sequence));
                return;
            }
            self.overflow = None;
        }

        let envelope = SubscriptionEventEnvelope {
            version: 1,
            subscription_id: self.id,
            event_sequence: event.sequence,
            window_id: event.window_id,
            event: json!({"type": event.kind, "data": event.data}),
        };
        if self.connection.event(envelope, &self.queued_events).is_err() {
            self.overflow = Some(match self.overflow {
                Some((start, _)) => (start, event.sequence),
                None => (event.sequence, event.sequence),
            });
        }
    }
}

/// Process-global sequence, replay, and subscription state.
#[derive(Debug, Default)]
pub struct AutomationHub {
    event_sequence: u64,
    replay: VecDeque<StoredEvent>,
    replay_bytes: usize,
    subscribers: Vec<Subscriber>,
    next_subscription_id: u64,
    next_creation_index: u64,
    next_write_token: u64,
}

impl AutomationHub {
    pub fn next_creation_index(&mut self) -> u64 {
        self.next_creation_index = self.next_creation_index.saturating_add(1);
        self.next_creation_index
    }

    pub fn next_write_token(&mut self) -> u64 {
        self.next_write_token = self.next_write_token.saturating_add(1);
        self.next_write_token
    }

    pub fn event_sequence(&self) -> u64 {
        self.event_sequence
    }

    pub fn emit(&mut self, window_id: Option<u64>, kind: &str, data: Value) -> u64 {
        self.event_sequence = self.event_sequence.saturating_add(1);
        let encoded_size =
            serde_json::to_vec(&data).map_or(0, |data| data.len()) + kind.len() + 128;
        let event = StoredEvent {
            sequence: self.event_sequence,
            window_id,
            kind: kind.to_owned(),
            data,
            encoded_size,
        };
        self.replay_bytes = self.replay_bytes.saturating_add(encoded_size);
        self.replay.push_back(event.clone());
        while self.replay.len() > EVENT_REPLAY_COUNT || self.replay_bytes > EVENT_REPLAY_BYTES {
            if let Some(oldest) = self.replay.pop_front() {
                self.replay_bytes = self.replay_bytes.saturating_sub(oldest.encoded_size);
            }
        }
        for subscriber in &mut self.subscribers {
            if subscriber.matches(&event) {
                subscriber.send(&event);
            }
        }
        self.subscribers.retain(|subscriber| subscriber.connection.is_alive());
        self.event_sequence
    }

    pub fn emit_output(&mut self, window_id: u64, start: u64, bytes: &[u8]) {
        for (chunk_index, chunk) in bytes.chunks(OUTPUT_EVENT_CHUNK).enumerate() {
            let chunk_start = start.saturating_add((chunk_index * OUTPUT_EVENT_CHUNK) as u64);
            self.emit(
                Some(window_id),
                "output",
                json!({
                    "start_offset": chunk_start,
                    "end_offset": chunk_start.saturating_add(chunk.len() as u64),
                    "data": base64::engine::general_purpose::STANDARD.encode(chunk),
                }),
            );
        }
    }

    pub fn subscribe(
        &mut self,
        connection: IpcConnection,
        request_id: u64,
        request: SubscriptionRequest,
    ) -> Result<u64, IpcError> {
        if self.subscribers.len() >= MAX_SUBSCRIPTIONS {
            return Err(IpcError::new("limit_exceeded", "Vivido allows at most 32 subscriptions"));
        }
        for kind in &request.kinds {
            if !crate::polling::ipc::EVENT_KINDS.contains(&kind.as_str()) {
                return Err(IpcError::new(
                    "invalid_params",
                    format!("unknown event kind {kind:?}"),
                ));
            }
        }
        if request.since_event.is_some_and(|since| since > self.event_sequence) {
            return Err(IpcError::new(
                "invalid_params",
                "since_event is newer than the current event sequence",
            ));
        }

        self.next_subscription_id = self.next_subscription_id.saturating_add(1);
        let mut subscriber = Subscriber {
            id: self.next_subscription_id,
            connection,
            window_id: request.target,
            all_windows: request.all_windows,
            kinds: request.kinds,
            overflow: None,
            queued_events: Arc::new(AtomicUsize::new(0)),
        };

        // Queue the correlated acknowledgement before replay events so CLI clients can establish
        // the subscription without attempting to decode an event as a response.
        subscriber.connection.reply(
            request_id,
            json!({
                "subscription_id": subscriber.id,
                "event_sequence": self.event_sequence,
            }),
        );

        if let Some(since) = request.since_event {
            let oldest =
                self.replay.front().map_or(self.event_sequence.saturating_add(1), |e| e.sequence);
            if since.saturating_add(1) < oldest {
                let gap = StoredEvent {
                    sequence: self.event_sequence,
                    window_id: request.target,
                    kind: String::from("overflow"),
                    data: json!({
                        "requested_sequence": since,
                        "oldest_sequence": oldest,
                        "current_event_sequence": self.event_sequence,
                        "current_sequences": request.current_sequences,
                    }),
                    encoded_size: 0,
                };
                subscriber.send(&gap);
            } else {
                for event in self.replay.iter().filter(|event| event.sequence > since) {
                    if subscriber.matches(event) {
                        subscriber.send(event);
                    }
                }
            }
        }

        let id = subscriber.id;
        self.subscribers.push(subscriber);
        Ok(id)
    }

    pub fn unsubscribe(&mut self, connection_id: u64, subscription_id: u64) -> bool {
        let old_len = self.subscribers.len();
        self.subscribers.retain(|subscriber| {
            subscriber.connection.id() != connection_id || subscriber.id != subscription_id
        });
        old_len != self.subscribers.len()
    }

    pub fn disconnect(&mut self, connection_id: u64) {
        self.subscribers.retain(|subscriber| subscriber.connection.id() != connection_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_has_monotonic_offsets_and_eviction_gaps() {
        let mut transcript = Transcript::default();
        assert_eq!(transcript.append(b"abc"), (0, 3));
        assert_eq!(transcript.range(1, 8).unwrap(), b"bc");

        transcript.append(&vec![b'x'; TRANSCRIPT_CAPACITY + 5]);
        assert_eq!(transcript.end_offset(), TRANSCRIPT_CAPACITY as u64 + 8);
        assert_eq!(transcript.oldest_offset(), 8);
        assert_eq!(transcript.range(7, 1).unwrap_err().code, "sequence_gap");
    }

    #[test]
    fn transcript_snapshot_reports_exact_range() {
        let mut transcript = Transcript::default();
        transcript.append(b"abcdef");
        let snapshot = transcript.snapshot(None, 3).unwrap();
        assert_eq!(snapshot.start_offset, 3);
        assert_eq!(snapshot.returned_end_offset, 6);
        assert_eq!(snapshot.data, b"def");
        assert!(snapshot.truncated);
    }

    #[test]
    fn transcript_preserves_data_across_append_boundaries() {
        let mut transcript = Transcript::default();
        transcript.append(b"rea");
        transcript.append(b"dy> ");
        assert_eq!(transcript.range(0, 64).unwrap(), b"ready> ");
    }

    #[test]
    fn screen_history_is_bounded_and_monotonic() {
        let transcript = Arc::new(Mutex::new(Transcript::default()));
        let mut state = AutomationWindowState::new(1, transcript);
        for row in 0..SCREEN_HISTORY_COUNT + 10 {
            state.record_screen_change(Some(vec![row as u16]));
        }
        assert_eq!(state.screen_sequence, (SCREEN_HISTORY_COUNT + 10) as u64);
        assert_eq!(state.screen_history.len(), SCREEN_HISTORY_COUNT);
        assert_eq!(state.screen_history.front().unwrap().sequence, 11);
    }
}
