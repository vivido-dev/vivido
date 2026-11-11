//! Bounded reconciliation pages built from the terminal presenter's owned state.

// Rust guideline compliant 2026-10-07
use super::*;
use vivid_protocol::session_status::{Kind, PageError, Query, Summary};

pub(super) fn query(
    shared: &ServiceShared,
    session: &SessionRuntime,
    object: u64,
    request: u64,
    value: &Value,
) -> Result<messages::PayloadMap, ControlError> {
    let query = Query::decode(object, value)
        .map_err(|_| ControlError::bad_message("invalid session status query"))?;
    let contexts = lock(&session.contexts)
        .values()
        .map(|context| Summary {
            kind: Kind::Context,
            context_id: context.identity.context_id(),
            surface_id: 0,
            object_id: 0,
            revision: context.revision.get(),
            generation: 0,
            state: context.lifecycle as u64,
            detail: 0,
        })
        .collect();
    let disabled = {
        let grant = lock(&session.grant);
        grant.active().is_none() || grant.watchdog_expired(clock::now())
    };
    let snapshot = shared
        .scene
        .session_status(session.identity, contexts, session.resume_generation, disabled)
        .map_err(ControlError::state)?;
    let page = vivid_protocol::session_status::paginate(
        &snapshot,
        session.identity,
        &query,
        request,
        session.control_body_limit,
    )
    .map_err(|error| match error {
        PageError::InvalidCursor | PageError::RevisionChanged => {
            ControlError::precondition("session status cursor is foreign or stale")
        },
        PageError::BodyLimit => {
            ControlError::limit("session status exceeds negotiated control body")
        },
        PageError::InvalidSnapshot => ControlError::state("invalid session status snapshot"),
    })?;
    page.payload().map_err(|_| ControlError::state("invalid session status page"))
}

pub(super) fn mutation(kind: u16) -> bool {
    matches!(
        kind,
        messages::CREATE_CONTEXT
            | messages::REVOKE_CONTEXT
            | messages::CREATE_SESSION_LEASE
            | messages::REVOKE_SESSION_LEASE
            | messages::CREATE_SURFACE
            | messages::UPDATE_SURFACE
            | messages::DESTROY_SURFACE
            | messages::CREATE_TRACK
            | messages::DESTROY_TRACK
            | messages::ADVANCE_CHANNEL
            | messages::ACTIVATE_TRACK
            | messages::BEGIN_TXN
            | messages::CREATE_NODE
            | messages::UPDATE_NODE
            | messages::DELETE_NODE
            | messages::COMMIT_TXN
            | messages::ABORT_TXN
            | messages::SET_AUDIO_GAIN
            | messages::PLAY
            | messages::PAUSE
            | messages::FLUSH
            | messages::DRAIN
            | messages::SET_OVERLAY_WINDOW
            | messages::OVERLAY_ACTION
    )
}
