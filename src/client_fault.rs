//! Containment metadata for failures originating at untrusted client boundaries.

use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FAULT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CONTAINED_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// The untrusted boundary at which a failure was contained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientFaultClass {
    /// Terminal output parser failure.
    TerminalParser,
    /// Pseudoterminal I/O failure.
    PtyIo,
    /// Vivid media-session failure.
    Vivid,
    /// Local automation connection failure.
    Ipc,
}

/// Current ability of a terminal pane to accept client traffic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientHealth {
    #[default]
    /// Accepting normal client traffic.
    Healthy,
    /// Client traffic stopped after an internal worker fault.
    Quarantined,
    /// Waiting for a requested client reset to complete.
    Recovering,
}

impl ClientHealth {
    /// Return the stable diagnostic spelling of this classification.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Quarantined => "quarantined",
            Self::Recovering => "recovering",
        }
    }
}

impl ClientFaultClass {
    /// Return the stable diagnostic spelling of this classification.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TerminalParser => "terminal_parser",
            Self::PtyIo => "pty_io",
            Self::Vivid => "vivid",
            Self::Ipc => "ipc",
        }
    }
}

/// Secret-free, bounded metadata for one contained client failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientFault {
    /// Identifier scoped to this value's owning context.
    pub id: u64,
    /// Effect or failure classification.
    pub class: ClientFaultClass,
    /// Bounded static diagnostic that contains no client-controlled text.
    pub diagnostic: &'static str,
}

impl ClientFault {
    /// Assign a new fault identifier to bounded static diagnostic metadata.
    pub fn new(class: ClientFaultClass, diagnostic: &'static str) -> Self {
        Self { id: NEXT_FAULT_ID.fetch_add(1, Ordering::Relaxed), class, diagnostic }
    }
}

struct BoundaryGuard;

impl BoundaryGuard {
    fn enter() -> Self {
        CONTAINED_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        Self
    }
}

impl Drop for BoundaryGuard {
    fn drop(&mut self) {
        CONTAINED_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Whether the current panic is unwinding through an explicitly contained client boundary.
#[cfg(any(windows, test))]
pub(crate) fn is_contained() -> bool {
    CONTAINED_DEPTH.with(|depth| depth.get() != 0)
}

/// Contain a panic at a worker boundary whose affected state will be discarded.
///
/// Project exception to M-PANIC-CONTINUATION: restarting an embedding host would terminate
/// unrelated owners. Each caller must stop the affected worker, revoke its resources, and
/// discard partially mutated state. This is not an exception mechanism for ordinary errors.
/// See `docs/panic-recovery.md` for the boundary inventory and recovery contract.
pub(crate) fn catch<T>(
    class: ClientFaultClass,
    diagnostic: &'static str,
    work: impl FnOnce() -> T,
) -> Result<T, ClientFault> {
    let _guard = BoundaryGuard::enter();
    panic::catch_unwind(AssertUnwindSafe(work)).map_err(|_| ClientFault::new(class, diagnostic))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_panic_becomes_bounded_fault_metadata() {
        let fault = catch(ClientFaultClass::Ipc, "IPC client handler panicked", || {
            panic!("untrusted secret text that must not escape")
        })
        .unwrap_err();

        assert_eq!(fault.class, ClientFaultClass::Ipc);
        assert_eq!(fault.diagnostic, "IPC client handler panicked");
        assert!(!is_contained());
    }

    #[test]
    fn every_worker_class_contains_its_panic_and_the_host_keeps_running() {
        for class in [
            ClientFaultClass::TerminalParser,
            ClientFaultClass::PtyIo,
            ClientFaultClass::Vivid,
            ClientFaultClass::Ipc,
        ] {
            let fault = std::thread::spawn(move || {
                catch(class, "contained worker panic", || panic!("client-controlled payload"))
            })
            .join()
            .expect("the supervised worker itself must not unwind")
            .expect_err("the client panic must become a fault");
            assert_eq!(fault.class, class);
            assert_eq!(fault.diagnostic, "contained worker panic");
        }

        // Exercise the actual host event channel after the supervised workers have failed.
        let (sink, receiver) = crate::EventSink::headless();
        std::thread::spawn(move || {
            sink.send_event(crate::Event::new(crate::EventType::HostWakeup, None)).unwrap();
        })
        .join()
        .unwrap();
        assert!(matches!(receiver.recv().unwrap().payload(), crate::EventType::HostWakeup));
    }
}
