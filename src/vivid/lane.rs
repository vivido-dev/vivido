//! The authenticated interactive lane.
//!
//! Core §7 gives input its own connection so that a saturated bulk track cannot delay revocation:
//! desktop §10 requires proving that holding bulk flow at zero still lets reset, revoke, and the
//! watchdog complete. The lane carries only `desktop-input-v1` records, `PING`, `PONG`, and
//! `ERROR`, and losing it revokes input without touching the control session, surfaces, or tracks.
//!
//! This module owns the admission rules, which are subtle enough to be worth testing on their own.

use vivid_protocol::messages;

/// Records the interactive lane will carry, core §7.
pub(crate) fn carries(record_type: u16) -> bool {
    matches!(
        record_type,
        messages::SET_INPUT_BINDING
            | messages::KEY_INPUT
            | messages::POINTER_MOTION
            | messages::POINTER_BUTTON
            | messages::POINTER_AXIS
            | messages::PING
            | messages::PONG
            | messages::ERROR
    )
}

/// What to do with a `LANE_OPEN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// A generation that has not been opened before.
    Accept,
    /// An exact retry after confirmed loss, which receives the same logical acceptance because no
    /// input was ever admitted on the transport it replaces.
    Replay,
    /// Another transport is already live for this generation.
    Busy,
    /// A stale generation, or a retry of one whose transport already admitted input. The producer
    /// has to obtain a new lane generation through control, and input stays revoked until a fresh
    /// binding.
    Refused,
}

/// The one interactive transport a session may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LaneState {
    generation: u64,
    nonce: [u8; 16],
    live: bool,
    /// Whether any ordinary input event was admitted on this generation's transport.
    admitted_input: bool,
}

impl LaneState {
    pub(crate) fn live(&self) -> bool {
        self.live
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Note that an ordinary input event was admitted, which makes this generation unrepeatable.
    pub(crate) fn note_input(&mut self) {
        self.admitted_input = true;
    }
}

/// Decide a `LANE_OPEN` against whatever transport the session already had.
///
/// On `Accept` or `Replay` the slot is updated to the admitted transport.
pub(crate) fn admit(slot: &mut Option<LaneState>, generation: u64, nonce: [u8; 16]) -> Admission {
    if generation == 0 {
        return Admission::Refused;
    }
    let Some(current) = slot else {
        *slot = Some(LaneState { generation, nonce, live: true, admitted_input: false });
        return Admission::Accept;
    };
    if generation == current.generation {
        if current.nonce != nonce {
            // A different client nonce for the same generation is a different open, and core §7
            // permits only one transport per generation.
            return Admission::Refused;
        }
        if current.live {
            return Admission::Busy;
        }
        if current.admitted_input {
            return Admission::Refused;
        }
        current.live = true;
        return Admission::Replay;
    }
    if generation < current.generation {
        return Admission::Refused;
    }
    *slot = Some(LaneState { generation, nonce, live: true, admitted_input: false });
    Admission::Accept
}

/// Mark the transport for `generation` lost, leaving a later generation untouched.
pub(crate) fn confirm_lost(slot: &mut Option<LaneState>, generation: u64) {
    if let Some(current) = slot
        && current.generation == generation
    {
        current.live = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 16] = [0xa; 16];
    const B: [u8; 16] = [0xb; 16];

    #[test]
    fn the_first_open_is_accepted() {
        let mut slot = None;
        assert_eq!(admit(&mut slot, 1, A), Admission::Accept);
        assert!(slot.unwrap().live());
    }

    #[test]
    fn a_zero_generation_is_refused() {
        let mut slot = None;
        assert_eq!(admit(&mut slot, 0, A), Admission::Refused);
        assert!(slot.is_none());
    }

    #[test]
    fn a_duplicate_open_while_live_is_busy() {
        let mut slot = None;
        admit(&mut slot, 1, A);
        assert_eq!(admit(&mut slot, 1, A), Admission::Busy);
    }

    #[test]
    fn an_exact_retry_after_loss_replays_when_no_input_was_admitted() {
        let mut slot = None;
        admit(&mut slot, 1, A);
        confirm_lost(&mut slot, 1);
        assert_eq!(admit(&mut slot, 1, A), Admission::Replay);
        assert!(slot.unwrap().live(), "the replacement transport is live");
    }

    #[test]
    fn a_retry_after_input_was_admitted_is_refused() {
        // Core §7: once an input event was accepted the generation cannot be reopened, so the
        // producer must reconcile through control and bind again.
        let mut slot = None;
        admit(&mut slot, 1, A);
        slot.as_mut().unwrap().note_input();
        confirm_lost(&mut slot, 1);
        assert_eq!(admit(&mut slot, 1, A), Admission::Refused);
    }

    #[test]
    fn a_different_nonce_for_the_same_generation_is_refused() {
        let mut slot = None;
        admit(&mut slot, 1, A);
        confirm_lost(&mut slot, 1);
        assert_eq!(admit(&mut slot, 1, B), Admission::Refused);
    }

    #[test]
    fn a_greater_generation_replaces_and_clears_the_input_flag() {
        let mut slot = None;
        admit(&mut slot, 1, A);
        slot.as_mut().unwrap().note_input();
        assert_eq!(admit(&mut slot, 2, B), Admission::Accept);
        let state = slot.unwrap();
        assert_eq!(state.generation, 2);
        assert!(state.live());
    }

    #[test]
    fn a_lower_generation_is_refused() {
        let mut slot = None;
        admit(&mut slot, 4, A);
        assert_eq!(admit(&mut slot, 3, B), Admission::Refused);
        assert_eq!(slot.unwrap().generation, 4);
    }

    #[test]
    fn losing_an_old_generation_does_not_disturb_the_current_one() {
        let mut slot = None;
        admit(&mut slot, 1, A);
        admit(&mut slot, 2, B);
        confirm_lost(&mut slot, 1);
        assert!(slot.unwrap().live(), "generation two is still the live transport");
    }

    #[test]
    fn the_lane_carries_only_input_and_liveness_records() {
        assert!(carries(messages::SET_INPUT_BINDING));
        assert!(carries(messages::KEY_INPUT));
        assert!(carries(messages::POINTER_AXIS));
        assert!(carries(messages::PING));
        assert!(!carries(messages::CREATE_SURFACE));
        assert!(!carries(messages::VIDEO_PACKET));
        assert!(!carries(messages::CREATE_SESSION_LEASE));
    }
}
