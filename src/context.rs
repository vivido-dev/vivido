//! Stable process-local identities for terminal contexts.

use std::fmt;

/// Process-local identity used to route events and timers.
///
/// Context IDs are allocated monotonically and are never reused during one Vivido process. They
/// are intentionally distinct from both native window IDs and the public IPC `window_id`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextId(u64);

impl ContextId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for ContextId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ContextId({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::ContextId;

    #[test]
    fn identity_is_independent_of_public_window_ids() {
        let first = ContextId::new(1);
        let second = ContextId::new(2);

        assert_ne!(first, second);
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
    }
}
