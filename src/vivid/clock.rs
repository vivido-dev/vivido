//! The native reading of the protocol's caller-supplied monotonic clock.
//!
//! `vivid_protocol` never calls a clock itself so that the input watchdog and the lease grace run
//! the same code in a browser presenter driven by `performance.now()`. Vivido supplies the native
//! half: one process-wide origin, and every `Monotonic` measured from it.

use std::sync::LazyLock;
use std::time::Instant;

use vivid_protocol::time::Monotonic;

/// The origin every `Monotonic` in this process is measured from.
///
/// A single origin is what makes two readings comparable — protocol deadlines are computed by
/// subtraction, so a per-call origin would make every elapsed interval zero.
static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

/// The current monotonic time.
pub(crate) fn now() -> Monotonic {
    from_instant(Instant::now())
}

/// Read an `Instant` this process already holds against the same origin.
pub(crate) fn from_instant(instant: Instant) -> Monotonic {
    // `saturating_duration_since` rather than subtraction: an `Instant` captured before the origin
    // was first forced is legitimate, and reads as the origin rather than panicking.
    Monotonic::from_micros(instant.saturating_duration_since(*ORIGIN).as_micros() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_share_one_origin_and_advance() {
        let first = now();
        let second = now();
        assert!(second >= first, "the clock never goes backwards");
        assert!(
            second.saturating_elapsed_since(first) < 1_000_000,
            "two adjacent readings are close, which proves they share an origin"
        );
    }

    #[test]
    fn an_instant_from_before_the_origin_reads_as_zero() {
        // Forcing the origin here rather than relying on test order.
        let origin = *ORIGIN;
        let earlier = origin.checked_sub(std::time::Duration::from_secs(1)).unwrap_or(origin);
        assert_eq!(from_instant(earlier), Monotonic::ZERO);
    }

    #[test]
    fn elapsed_micros_match_the_underlying_instants() {
        // Force the lazy origin before capturing instants: a first-read capture here would land
        // after `base` and make every interval read as near zero.
        let _ = *ORIGIN;
        let base = Instant::now();
        let later = base + std::time::Duration::from_millis(250);
        assert_eq!(from_instant(later).saturating_elapsed_since(from_instant(base)), 250_000);
    }
}
