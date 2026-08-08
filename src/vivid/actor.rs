//! Session egress and the bounded pending-operation set.
//!
//! Core §5.1 requires both endpoints to service control traffic continuously, and requires a long
//! operation to become bounded pending state rather than blocking the parser: `PING`, input
//! revocation, channel-local flow updates, track recovery, and authority revocation all have to
//! keep working while something else is outstanding.
//!
//! Vivido used to satisfy that by spawning a thread per outstanding operation and writing replies
//! inline on the reader thread. That has two defects. A thread per registered wait is a thread per
//! wait — polling every two milliseconds — and, more seriously, a peer that stops reading its
//! control replies stalls the reader mid-write, so nothing further can even be parsed. Neither is
//! survivable once an input watchdog has to fire inside 250 ms.
//!
//! So a session is three parts: a reader that only parses and enqueues, an actor that owns mutable
//! state and applies mutations in receive order, and an egress that owns the socket. Replies may
//! complete out of order and are correlated by request ID.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use vivid_protocol::identity::TrackIdentity;
use vivid_protocol::messages;
use vivid_protocol::revision::ChannelGeneration;

use crate::vivid::audio::AudioOutput;
use crate::vivid::scene::{SharedScene, TrackWaitEvaluation};
use crate::vivid::transport::{ReadShutdown, Writer};

/// How often the actor re-evaluates outstanding operations when no record arrives.
pub(crate) const TICK: Duration = Duration::from_millis(2);

/// Records the egress will hold for a peer that has stopped reading.
///
/// Bounded, and overflow closes the session. A presenter that buffers without limit for an
/// unresponsive producer has simply moved the failure somewhere less observable.
pub(crate) const EGRESS_CAPACITY: usize = 256;

/// Parsed records the reader may run ahead by while the actor is busy.
pub(crate) const INGRESS_CAPACITY: usize = 64;

struct EgressQueue {
    records: VecDeque<(u16, u64, Vec<u8>)>,
    closed: bool,
}

/// Owns a connection's writer so no thread that must stay responsive blocks on a peer.
///
/// Every write a presenter originates goes through one of these. The control egress is owned by
/// the session actor; the lane egress is owned by the lane's reader thread. Threads that may never
/// block on a socket — the PTY parser, the winit UI thread, a track channel, another session's
/// actor — only ever queue here.
pub(crate) struct Egress {
    queue: Mutex<EgressQueue>,
    ready: Condvar,
    overflowed: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
    /// Unblocks the reader sharing this connection once the egress has given up on the peer.
    ///
    /// Only the lane sets one. The control session's actor already owns its reader's shutdown and
    /// stops it after the final reply has drained, which an egress-side close would pre-empt.
    shutdown: Mutex<Option<ReadShutdown>>,
}

impl Egress {
    pub(crate) fn start(writer: Arc<Writer>, name: &'static str) -> Arc<Self> {
        let egress = Arc::new(Self {
            queue: Mutex::new(EgressQueue { records: VecDeque::new(), closed: false }),
            ready: Condvar::new(),
            overflowed: AtomicBool::new(false),
            worker: Mutex::new(None),
            shutdown: Mutex::new(None),
        });
        let worker = {
            let egress = egress.clone();
            thread::Builder::new().name(name.into()).spawn(move || egress.run(writer)).ok()
        };
        *egress.worker.lock().expect("egress worker") = worker;
        egress
    }

    /// Have this egress unblock `shutdown`'s reader when it *gives up* on the peer — an overflow
    /// or a failed write, never an orderly close.
    ///
    /// Without this a session whose queue overflowed from an outside thread would stop accepting
    /// records but stay registered, holding its connection slot and its scene state, until its
    /// peer happened to close. Overflow is meant to end the session, so it has to be able to.
    pub(crate) fn set_shutdown(&self, shutdown: ReadShutdown) {
        let closed = self.queue.lock().expect("egress queue").closed;
        if closed {
            // The egress already gave up; honour the handle immediately rather than storing it
            // somewhere nothing will read it again.
            shutdown.stop();
            return;
        }
        *self.shutdown.lock().expect("egress shutdown") = Some(shutdown);
    }

    /// An egress with no worker, so queue admission can be tested without racing a drain.
    #[cfg(test)]
    fn detached() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(EgressQueue { records: VecDeque::new(), closed: false }),
            ready: Condvar::new(),
            overflowed: AtomicBool::new(false),
            worker: Mutex::new(None),
            shutdown: Mutex::new(None),
        })
    }

    /// Queue one record. Returns false when the session must close, because the peer is not
    /// draining its replies or the egress has already shut down.
    pub(crate) fn send(&self, record_type: u16, object_id: u64, body: Vec<u8>) -> bool {
        let mut queue = self.queue.lock().expect("egress queue");
        if queue.closed {
            return false;
        }
        if queue.records.len() >= EGRESS_CAPACITY {
            queue.closed = true;
            self.overflowed.store(true, Ordering::Release);
            self.ready.notify_all();
            drop(queue);
            self.stop_reader();
            return false;
        }
        queue.records.push_back((record_type, object_id, body));
        self.ready.notify_all();
        true
    }

    /// True when the session ended because the peer stopped reading.
    pub(crate) fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Acquire)
    }

    /// Stop accepting records and let the worker drain what is already queued.
    pub(crate) fn close(&self) {
        let mut queue = self.queue.lock().expect("egress queue");
        queue.closed = true;
        self.ready.notify_all();
    }

    pub(crate) fn join(&self) {
        let worker = self.worker.lock().expect("egress worker").take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }

    /// Wake whatever reader shares this connection, so it stops waiting for a peer we have
    /// stopped answering. A no-op for the control egress, which has no handle.
    fn stop_reader(&self) {
        if let Some(shutdown) = self.shutdown.lock().expect("egress shutdown").take() {
            shutdown.stop();
        }
    }

    fn run(self: Arc<Self>, writer: Arc<Writer>) {
        loop {
            let next = {
                let mut queue = self.queue.lock().expect("egress queue");
                loop {
                    if let Some(record) = queue.records.pop_front() {
                        break Some(record);
                    }
                    if queue.closed {
                        break None;
                    }
                    queue = self.ready.wait(queue).expect("egress queue");
                }
            };
            let Some((record_type, object_id, body)) = next else {
                // A normal close, so the reader is left alone: the control session's actor stops
                // it itself once the final reply — notably `GOODBYE`'s `OK` — has drained, and
                // pre-empting that would cut the connection before the peer saw it.
                return;
            };
            if writer.write_record(record_type, object_id, &body).is_err() {
                self.close();
                self.stop_reader();
                return;
            }
        }
    }
}

/// One operation the session owes a reply for.
pub(crate) enum Pending {
    /// `WAIT_TRACK`: a bounded, one-shot condition on a track's current channel generation.
    TrackWait {
        request_id: u64,
        object_id: u64,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        condition: u64,
        value: Option<u64>,
        deadline: Instant,
    },
    /// `DRAIN`: audio completes only after EOS, decoder flush, and queued device samples.
    AudioDrain {
        request_id: u64,
        object_id: u64,
        identity: TrackIdentity,
        generation: ChannelGeneration,
        output: Arc<AudioOutput>,
    },
}

impl Pending {
    fn is_wait(&self) -> bool {
        matches!(self, Self::TrackWait { .. })
    }

    fn request_id(&self) -> u64 {
        match self {
            Self::TrackWait { request_id, .. } | Self::AudioDrain { request_id, .. } => *request_id,
        }
    }
}

/// Why a pending entry could not be registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    /// The registered-wait contract is exhausted.
    Waits,
    /// The pending-request contract is exhausted.
    Requests,
}

/// The session's outstanding operations, bounded by the effective resource contract.
pub(crate) struct PendingSet {
    entries: Vec<Pending>,
    maximum_waits: usize,
    maximum_requests: usize,
}

impl PendingSet {
    pub(crate) fn new(maximum_waits: u64, maximum_requests: u64) -> Self {
        Self {
            entries: Vec::new(),
            maximum_waits: usize::try_from(maximum_waits).unwrap_or(usize::MAX),
            maximum_requests: usize::try_from(maximum_requests).unwrap_or(usize::MAX),
        }
    }

    pub(crate) fn register(&mut self, entry: Pending) -> Result<(), AdmissionError> {
        if self.entries.len() >= self.maximum_requests {
            return Err(AdmissionError::Requests);
        }
        if entry.is_wait()
            && self.entries.iter().filter(|item| item.is_wait()).count() >= self.maximum_waits
        {
            return Err(AdmissionError::Waits);
        }
        self.entries.push(entry);
        Ok(())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve whatever is ready, writing replies through the egress.
    ///
    /// `cancelled` holds request IDs a `CANCEL_WAIT` named; a cancel that arrives after the wait
    /// already completed is harmless, which is what core §10 requires.
    pub(crate) fn service(
        &mut self,
        scene: &SharedScene,
        egress: &Egress,
        cancelled: &mut std::collections::HashSet<u64>,
        now: Instant,
    ) {
        let mut resolved = Vec::new();
        self.entries.retain(|entry| {
            if cancelled.remove(&entry.request_id()) {
                resolved.push((
                    messages::ERROR,
                    object_id_of(entry),
                    cancelled_body(entry.request_id()),
                ));
                return false;
            }
            match entry {
                Pending::TrackWait {
                    request_id,
                    object_id,
                    identity,
                    generation,
                    condition,
                    value,
                    deadline,
                } => match scene.evaluate_track_wait(*identity, *generation, *condition, *value) {
                    TrackWaitEvaluation::Satisfied(satisfied) => {
                        resolved.push((
                            messages::WAIT_SATISFIED,
                            *object_id,
                            crate::vivid::wait_satisfied_body(
                                *request_id,
                                *identity,
                                *condition,
                                satisfied,
                            ),
                        ));
                        false
                    },
                    TrackWaitEvaluation::NotFound => {
                        resolved.push((
                            messages::ERROR,
                            *object_id,
                            error_body(
                                *request_id,
                                messages::ERROR_NOT_FOUND,
                                "track does not exist",
                            ),
                        ));
                        false
                    },
                    TrackWaitEvaluation::Lost => {
                        resolved.push((
                            messages::ERROR,
                            *object_id,
                            error_body(
                                *request_id,
                                messages::ERROR_BAD_STATE,
                                "track was lost while waiting",
                            ),
                        ));
                        false
                    },
                    TrackWaitEvaluation::StaleGeneration => {
                        resolved.push((
                            messages::ERROR,
                            *object_id,
                            error_body(
                                *request_id,
                                messages::ERROR_STALE_CHANNEL_GENERATION,
                                "track wait names a stale channel generation",
                            ),
                        ));
                        false
                    },
                    TrackWaitEvaluation::NotVisible => {
                        resolved.push((
                            messages::ERROR,
                            *object_id,
                            error_body(
                                *request_id,
                                messages::ERROR_NOT_VISIBLE,
                                "track has no eligible visible placement",
                            ),
                        ));
                        false
                    },
                    TrackWaitEvaluation::Pending if now < *deadline => true,
                    TrackWaitEvaluation::Pending => {
                        resolved.push((
                            messages::ERROR,
                            *object_id,
                            error_body(
                                *request_id,
                                messages::ERROR_TIMEOUT,
                                "track wait timed out",
                            ),
                        ));
                        false
                    },
                },
                Pending::AudioDrain { request_id, object_id, identity, generation, output } => {
                    match output.poll_drained() {
                        None => true,
                        Some(Ok(())) => {
                            let _ = scene.mark_buffered_ended(*identity, *generation);
                            resolved.push((messages::OK, *object_id, messages::ok(*request_id)));
                            false
                        },
                        Some(Err(error)) => {
                            resolved.push((
                                messages::ERROR,
                                *object_id,
                                error_body_owned(
                                    *request_id,
                                    messages::ERROR_DEVICE_LOST,
                                    error.to_string(),
                                ),
                            ));
                            false
                        },
                    }
                },
            }
        });
        for (record_type, object_id, body) in resolved {
            if !egress.send(record_type, object_id, body) {
                return;
            }
        }
    }
}

fn object_id_of(entry: &Pending) -> u64 {
    match entry {
        Pending::TrackWait { object_id, .. } | Pending::AudioDrain { object_id, .. } => *object_id,
    }
}

fn cancelled_body(request_id: u64) -> Vec<u8> {
    error_body(request_id, messages::ERROR_CANCELLED, "track wait was cancelled")
}

fn error_body(request_id: u64, code: u64, diagnostic: &'static str) -> Vec<u8> {
    error_body_owned(request_id, code, diagnostic.to_owned())
}

fn error_body_owned(request_id: u64, code: u64, diagnostic: String) -> Vec<u8> {
    crate::vivid::protocol_error(request_id, code, false, diagnostic)
        .unwrap_or_else(|_| messages::ok(request_id))
}

/// A reply the actor produced but could not deliver.
pub(crate) fn deliver(egress: &Egress, reply: (u16, u64, Vec<u8>)) -> io::Result<()> {
    if egress.send(reply.0, reply.1, reply.2) {
        return Ok(());
    }
    Err(io::Error::new(io::ErrorKind::BrokenPipe, "control egress closed"))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use vivid_protocol::identity::{PresenterInstanceId, SessionIdentity};

    use super::*;

    fn track() -> TrackIdentity {
        let session = SessionIdentity::new(PresenterInstanceId([1; 16]), 1).unwrap();
        session.context(1).unwrap().surface(1).unwrap().track(1).unwrap()
    }

    fn wait(request_id: u64) -> Pending {
        Pending::TrackWait {
            request_id,
            object_id: 1,
            identity: track(),
            generation: ChannelGeneration::ONE,
            condition: 2,
            value: Some(1),
            deadline: Instant::now() + Duration::from_secs(1),
        }
    }

    #[test]
    fn registered_waits_are_bounded_by_the_contract() {
        let mut pending = PendingSet::new(2, 16);
        assert!(pending.register(wait(1)).is_ok());
        assert!(pending.register(wait(2)).is_ok());
        assert_eq!(pending.register(wait(3)), Err(AdmissionError::Waits));
    }

    #[test]
    fn total_pending_requests_are_bounded_independently_of_waits() {
        // The pending-request bound is the tighter of the two here, so it is what refuses.
        let mut pending = PendingSet::new(16, 2);
        assert!(pending.register(wait(1)).is_ok());
        assert!(pending.register(wait(2)).is_ok());
        assert_eq!(pending.register(wait(3)), Err(AdmissionError::Requests));
    }

    #[test]
    fn an_empty_set_needs_no_servicing() {
        let pending = PendingSet::new(4, 4);
        assert!(pending.is_empty());
    }

    #[test]
    fn egress_overflow_closes_the_session_rather_than_buffering() {
        // A peer that stops draining must not make the presenter grow without bound. With no
        // worker attached nothing drains, so admission is exactly the queue bound.
        let egress = Egress::detached();
        let mut accepted = 0;
        while egress.send(messages::PONG, 0, Vec::new()) {
            accepted += 1;
            assert!(accepted <= EGRESS_CAPACITY, "the egress grew past its bound");
        }
        assert_eq!(accepted, EGRESS_CAPACITY);
        assert!(egress.overflowed(), "overflow must be reported, not silent");
        assert!(!egress.send(messages::PONG, 0, Vec::new()), "an overflowed egress stays closed");
    }

    #[test]
    fn a_closed_egress_refuses_further_records() {
        let egress = Egress::detached();
        assert!(egress.send(messages::PONG, 0, Vec::new()));
        egress.close();
        assert!(!egress.send(messages::PONG, 0, Vec::new()));
        assert!(!egress.overflowed(), "a clean close is not an overflow");
    }
}
