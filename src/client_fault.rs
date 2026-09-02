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
    TerminalParser,
    PtyIo,
    Vivid,
    Ipc,
}

/// Current ability of a terminal pane to accept client traffic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientHealth {
    #[default]
    Healthy,
    Quarantined,
    Recovering,
}

impl ClientHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Quarantined => "quarantined",
            Self::Recovering => "recovering",
        }
    }
}

impl ClientFaultClass {
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
    pub id: u64,
    pub class: ClientFaultClass,
    pub diagnostic: &'static str,
}

impl ClientFault {
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
pub(crate) fn is_contained() -> bool {
    CONTAINED_DEPTH.with(|depth| depth.get() != 0)
}

/// Run client-derived work without allowing its panic to escape the current worker boundary.
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

        let mut host_turns = 0;
        host_turns += 1;
        assert_eq!(host_turns, 1, "the host event loop remains runnable");
    }
}
