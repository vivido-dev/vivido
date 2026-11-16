//! Coordinate resume proofs with the preceding control runtime's retirement.

// Rust guideline compliant 2026-10-07
use super::*;

pub(super) fn retire(
    shared: &ServiceShared,
    hello: &Hello,
    preface: &[u8; 16],
    deadline: Instant,
) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(ErrorKind::TimedOut, "control handshake deadline expired"));
    }
    let HelloAuthentication::Resume { context_id, lease_id, session_id, resume_generation, .. } =
        &hello.authentication
    else {
        return Ok(());
    };
    let mut registry = lock(&shared.registry);
    let Some(old) = registry
        .sessions
        .get(session_id)
        .filter(|session| {
            session.resume_generation == *resume_generation
                && session.lease.is_some_and(|key| key.1 == *context_id && key.2 == *lease_id)
        })
        .cloned()
    else {
        return Ok(());
    };
    if !hello.verify_resume(&auth::ResumeKey::new(*old.resume_key.expose()), preface)? {
        // Invalid proofs receive ordinary authentication rejection without parking.
        return Ok(());
    }
    while registry.sessions.get(session_id).is_some_and(|current| Arc::ptr_eq(current, &old)) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(ErrorKind::TimedOut, "resume awaited control retirement"));
        }
        #[cfg(test)]
        shared.waiting_resumes.fetch_add(1, Ordering::AcqRel);
        let (next, _) = shared
            .retired
            .wait_timeout(registry, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry = next;
        #[cfg(test)]
        shared.waiting_resumes.fetch_sub(1, Ordering::AcqRel);
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(ErrorKind::TimedOut, "resume awaited control retirement"));
    }
    // This reserves no authority. Ordinary establishment still checks suspended
    // state, grace, generation, the complete proof, and the original profile set.
    Ok(())
}
