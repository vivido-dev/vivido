//! Session leases: delegated authority for one child logical session.
//!
//! Security §6 inverts who creates the secret. The controller generates a 32-byte activation
//! secret and sends the presenter only `SHA-256("VIVID-LEASE-1" || lease_id || secret)`, so a lost
//! `SESSION_LEASE_READY` can never lose the only copy of a credential — the reply contains no
//! secret at all, and an exact retry returns the same non-secret outcome.
//!
//! The state machine itself lives in `vivid_protocol::lease::LeaseMachine`, which owns the
//! activation-retry rules that make a lost `WELCOME` recoverable without minting a second session.
//! This module is the presenter's storage and lookup around it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use vivid_protocol::auth::{self, Secret32};
use vivid_protocol::cbor::Value;
use vivid_protocol::identity::SessionIdentity;
use vivid_protocol::lease::{CleanupPolicy, LeaseMachine, LeaseState, SessionLeaseDefinition};
use vivid_protocol::messages::PayloadMap;
use vivid_protocol::resource::ResourceContract;

/// Complete identity of a lease: the session that issued it, its owning context, and its local ID.
///
/// Core §2.1 makes this the teardown key. Two sessions deliberately reusing context and lease
/// numbers must not be able to reach each other's leases, and an activation that names
/// `(context, lease)` is disambiguated by which stored verifier its secret actually matches.
pub(crate) type LeaseKey = (SessionIdentity, u64, u64);

/// Reason bits on `SESSION_LEASE_CHANGED`, security §8.
pub(crate) mod reason {
    pub(crate) const CLEAN_CLOSE: u64 = 1 << 1;
    pub(crate) const UNCLEAN_LOSS: u64 = 1 << 2;
    pub(crate) const EXPLICIT_REVOKE: u64 = 1 << 4;
    pub(crate) const PARENT_CLEANUP: u64 = 1 << 5;
    pub(crate) const RESUMED: u64 = 1 << 3;
    pub(crate) const ACTIVATION_EXPIRY: u64 = 1 << 6;
    pub(crate) const GRACE_EXPIRY: u64 = 1 << 7;
}

/// One issued lease.
pub(crate) struct Lease {
    pub(crate) definition: SessionLeaseDefinition,
    pub(crate) machine: LeaseMachine,
    /// Capacity reserved from the owning context, released on final cleanup.
    pub(crate) contract: ResourceContract,
    /// Operation classes the child session inherits.
    pub(crate) classes: u64,
    /// When an unactivated lease stops being usable.
    activation_deadline: Instant,
    /// When a suspended lease's grace runs out.
    pub(crate) grace_deadline: Option<Instant>,
    /// The child logical session, once one exists.
    pub(crate) child: Option<SessionIdentity>,
    /// The suspended session's resume key, which is what a resume proof is verified against.
    ///
    /// Held only while suspended. Security §7.2 derives the next generation's keys from it and
    /// erases it once the new `WELCOME` confirmation is committed.
    resume_key: Option<Secret32>,
}

impl Lease {
    pub(crate) fn new(
        definition: SessionLeaseDefinition,
        contract: ResourceContract,
        classes: u64,
        now: Instant,
    ) -> Self {
        let machine =
            LeaseMachine::new(definition.cleanup_policy, definition.requested_disconnect_grace_us);
        let activation_deadline =
            now.checked_add(Duration::from_micros(definition.activation_timeout_us)).unwrap_or(now);
        Self {
            definition,
            machine,
            contract,
            classes,
            activation_deadline,
            grace_deadline: None,
            child: None,
            resume_key: None,
        }
    }

    /// Does this activation secret belong to this lease?
    ///
    /// Compared in constant time after an exact-length check, per security §2.
    pub(crate) fn accepts(&self, lease_id: u64, secret: &Secret32) -> bool {
        auth::verify_activation_secret(lease_id, secret, &self.definition.activation_verifier)
    }

    /// Does an unclean loss suspend this lease rather than close it?
    pub(crate) fn suspends_on_unclean_loss(&self) -> bool {
        self.definition.cleanup_policy == CleanupPolicy::SuspendOnUncleanLoss
            && self.definition.requested_disconnect_grace_us > 0
    }

    /// Move to `SUSPENDED`, starting the grace and retaining the resume key.
    pub(crate) fn suspend(&mut self, resume_key: Secret32, now: Instant) -> bool {
        if self.machine.confirm_transport_lost(false).is_err() {
            return false;
        }
        if self.machine.state() != LeaseState::Suspended {
            return false;
        }
        self.resume_key = Some(resume_key);
        self.grace_deadline =
            now.checked_add(Duration::from_micros(self.definition.requested_disconnect_grace_us));
        true
    }

    /// The resume key a proof is checked against, while suspended.
    pub(crate) fn resume_key(&self) -> Option<&Secret32> {
        self.resume_key.as_ref()
    }

    /// Clear the grace and the retained key once the session is live again.
    pub(crate) fn resumed(&mut self) {
        self.grace_deadline = None;
        self.resume_key = None;
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        match self.machine.state() {
            // An unactivated lease stops being usable at its activation deadline.
            LeaseState::Issued => now >= self.activation_deadline,
            // A suspended one stops at the end of its grace.
            LeaseState::Suspended => self.grace_deadline.is_some_and(|deadline| now >= deadline),
            _ => false,
        }
    }

    /// Payload for `SESSION_LEASE_READY`. Security §6.2: no secret appears here.
    pub(crate) fn ready_payload(&self, context_id: u64, lease_id: u64) -> PayloadMap {
        vec![
            (0, Value::Unsigned(context_id)),
            (1, Value::Unsigned(lease_id)),
            (2, Value::Unsigned(self.machine.state() as u64)),
            (3, Value::Unsigned(self.definition.activation_timeout_us)),
            (4, Value::Unsigned(self.definition.requested_disconnect_grace_us)),
            (5, Value::Unsigned(self.definition.cleanup_policy as u64)),
            (
                6,
                Value::Array(
                    self.definition
                        .permitted_profiles
                        .iter()
                        .map(|profile| Value::Text(profile.clone()))
                        .collect(),
                ),
            ),
            (7, self.contract.to_value()),
            (8, Value::Unsigned(self.machine.revision())),
        ]
    }

    /// Payload for the actionable `SESSION_LEASE_CHANGED`, security §8.
    pub(crate) fn changed_payload(
        &self,
        context_id: u64,
        lease_id: u64,
        reason: u64,
        now: Instant,
    ) -> PayloadMap {
        let remaining = match (self.machine.state(), self.grace_deadline) {
            (LeaseState::Suspended, Some(deadline)) => deadline
                .checked_duration_since(now)
                .map(|left| left.as_micros().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0),
            _ => 0,
        };
        vec![
            (0, Value::Unsigned(context_id)),
            (1, Value::Unsigned(lease_id)),
            (2, Value::Unsigned(self.machine.state() as u64)),
            (3, Value::Unsigned(self.machine.revision())),
            (4, Value::Unsigned(self.machine.resume_generation().get())),
            (5, Value::Unsigned(reason)),
            (6, Value::Unsigned(remaining)),
        ]
    }
}

/// Every lease this presenter has issued, keyed by complete identity.
#[derive(Default)]
pub(crate) struct LeaseTable {
    leases: HashMap<LeaseKey, Lease>,
}

impl LeaseTable {
    pub(crate) fn insert(&mut self, key: LeaseKey, lease: Lease) {
        self.leases.insert(key, lease);
    }

    pub(crate) fn contains(&self, key: &LeaseKey) -> bool {
        self.leases.contains_key(key)
    }

    pub(crate) fn get_mut(&mut self, key: &LeaseKey) -> Option<&mut Lease> {
        self.leases.get_mut(key)
    }

    pub(crate) fn remove(&mut self, key: &LeaseKey) -> Option<Lease> {
        self.leases.remove(key)
    }

    /// Find the lease an activation names.
    ///
    /// `(context, lease)` alone is ambiguous because both are session-scoped and producers
    /// habitually start at one, so the secret is what disambiguates. Every candidate is checked so
    /// a caller cannot learn which contexts exist by timing.
    pub(crate) fn find_activation(
        &self,
        context_id: u64,
        lease_id: u64,
        secret: &Secret32,
    ) -> Option<LeaseKey> {
        let mut found = None;
        for (key, lease) in &self.leases {
            if key.1 != context_id || key.2 != lease_id {
                continue;
            }
            if lease.accepts(lease_id, secret) && found.is_none() {
                found = Some(*key);
            }
        }
        found
    }

    /// Find a suspended lease a resume names.
    ///
    /// The proof is verified by the caller against the lease's retained resume key; this only
    /// narrows by complete identity plus the suspended session the resume claims.
    pub(crate) fn find_resume(
        &self,
        context_id: u64,
        lease_id: u64,
        session_id: u64,
    ) -> Option<LeaseKey> {
        self.leases
            .iter()
            .find(|(key, lease)| {
                key.1 == context_id
                    && key.2 == lease_id
                    && lease.machine.state() == LeaseState::Suspended
                    && lease.child.is_some_and(|child| child.session_id == session_id)
            })
            .map(|(key, _)| *key)
    }

    /// Every lease issued by one session, for parent cleanup.
    pub(crate) fn issued_by(&self, issuer: SessionIdentity) -> Vec<LeaseKey> {
        self.leases.keys().filter(|key| key.0 == issuer).copied().collect()
    }

    /// Leases whose activation or grace deadline has passed.
    pub(crate) fn expired(&self, now: Instant) -> Vec<LeaseKey> {
        self.leases.iter().filter(|(_, lease)| lease.expired(now)).map(|(key, _)| *key).collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.leases.len()
    }
}

/// A fingerprint of the negotiated profile set.
///
/// Security §7.2 requires a resumed session's offer to equal the original exactly, and §6.4 the
/// same for an activation retry. Hashing the accepted set plus the target profile makes that one
/// comparison instead of several.
pub(crate) fn profile_fingerprint(target_profile: &str, accepted: &[String]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"vivido-profile-fingerprint-1");
    hasher.update((target_profile.len() as u64).to_be_bytes());
    hasher.update(target_profile.as_bytes());
    for profile in accepted {
        hasher.update((profile.len() as u64).to_be_bytes());
        hasher.update(profile.as_bytes());
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use vivid_protocol::identity::PresenterInstanceId;

    use super::*;

    fn secret(byte: u8) -> Secret32 {
        Secret32::new([byte; 32])
    }

    fn definition(lease_id: u64, secret: &Secret32) -> SessionLeaseDefinition {
        SessionLeaseDefinition {
            context_id: 1,
            lease_id,
            activation_verifier: auth::activation_verifier(lease_id, secret),
            activation_timeout_us: 20_000_000,
            requested_disconnect_grace_us: 10_000_000,
            cleanup_policy: CleanupPolicy::SuspendOnUncleanLoss,
            permitted_profiles: vec!["vivid-core-control-v1".into()],
            requested_contract: ResourceContract::denied(),
            client_public_key: None,
        }
    }

    fn session(id: u64) -> SessionIdentity {
        SessionIdentity::new(PresenterInstanceId([9; 16]), id).unwrap()
    }

    fn lease(lease_id: u64, secret: &Secret32) -> Lease {
        Lease::new(definition(lease_id, secret), ResourceContract::denied(), 0, Instant::now())
    }

    #[test]
    fn a_lease_accepts_only_its_own_secret() {
        let entry = lease(4, &secret(0xa1));
        assert!(entry.accepts(4, &secret(0xa1)));
        assert!(!entry.accepts(4, &secret(0xa2)), "a different secret must not match");
        assert!(!entry.accepts(5, &secret(0xa1)), "the lease ID is bound into the verifier");
    }

    #[test]
    fn two_sessions_reusing_lease_numbers_stay_separate() {
        // Core §2.1: the complete identity is the key, and the secret disambiguates an activation
        // that names only (context, lease).
        let mut table = LeaseTable::default();
        let first = secret(0x11);
        let second = secret(0x22);
        table.insert((session(1), 1, 7), lease(7, &first));
        table.insert((session(2), 1, 7), lease(7, &second));

        assert_eq!(table.find_activation(1, 7, &first), Some((session(1), 1, 7)));
        assert_eq!(table.find_activation(1, 7, &second), Some((session(2), 1, 7)));
        assert_eq!(table.find_activation(1, 7, &secret(0x33)), None);
    }

    #[test]
    fn parent_cleanup_selects_only_the_issuing_session() {
        let mut table = LeaseTable::default();
        table.insert((session(1), 1, 7), lease(7, &secret(0x11)));
        table.insert((session(1), 2, 8), lease(8, &secret(0x12)));
        table.insert((session(2), 1, 7), lease(7, &secret(0x22)));

        let mut issued = table.issued_by(session(1));
        issued.sort();
        assert_eq!(issued, vec![(session(1), 1, 7), (session(1), 2, 8)]);
        assert_eq!(table.issued_by(session(3)), Vec::new());
    }

    #[test]
    fn an_unactivated_lease_expires_at_its_activation_deadline() {
        let now = Instant::now();
        let mut definition = definition(3, &secret(0x44));
        definition.activation_timeout_us = 1;
        let entry = Lease::new(definition, ResourceContract::denied(), 0, now);
        assert!(!entry.expired(now));
        assert!(entry.expired(now + Duration::from_millis(1)));
    }

    #[test]
    fn a_ready_payload_carries_no_secret() {
        let entry = lease(4, &secret(0xa1));
        let payload = entry.ready_payload(1, 4);
        for (_, value) in &payload {
            if let Value::Bytes(bytes) = value {
                panic!("SESSION_LEASE_READY carried {} bytes of opaque data", bytes.len());
            }
        }
        assert_eq!(payload[2].1.as_u64(), Some(LeaseState::Issued as u64));
    }

    #[test]
    fn the_profile_fingerprint_distinguishes_a_changed_offer() {
        let accepted = vec!["a".to_owned(), "b".to_owned()];
        let baseline = profile_fingerprint("t", &accepted);
        assert_eq!(baseline, profile_fingerprint("t", &accepted));
        assert_ne!(baseline, profile_fingerprint("u", &accepted));
        assert_ne!(baseline, profile_fingerprint("t", &["a".to_owned()]));
        // Length-prefixed, so concatenation cannot alias.
        assert_ne!(baseline, profile_fingerprint("t", &["ab".to_owned()]));
    }
}
