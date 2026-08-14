//! Presenter-owned file-drop bindings, source handles, and bulk transfer connections.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use sha2::{Digest, Sha256};
use vivid_protocol::file_drop::{
    self, AcceptFileDrop, AdvanceFileTransfer, CancelFileDrop, FileDropBinding,
    FileDropBindingState, FileDropGrant, FileDropOffer, FileDropState, FileDropStatus,
    FileDropTuple, FileFinish, FileResult, FileResultCode, FileTransferAccepted, FileTransferOpen,
    MaximumFileData, QueryFileDrop,
};
use vivid_protocol::identity::{SessionIdentity, SurfaceIdentity};
use vivid_protocol::revision::{
    FileDropGrantGeneration, FileTransferGeneration, SurfaceGeneration,
};
use vivid_protocol::wire::ConnectionKind;
use vivid_protocol::{auth, messages, registry};

use super::{
    PendingConnection, Reader, ServiceShared, SessionRuntime, Writer, lock, protocol_error,
};
use crate::vivid::transport::ReadShutdown;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BindingKey {
    session: SessionIdentity,
    context_id: u64,
    surface_id: u64,
}

#[derive(Debug)]
struct BindingEntry {
    binding: FileDropBinding,
    grant: FileDropGrant,
    activation: u64,
    consent: ConsentState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsentState {
    Unconfirmed,
    ConfirmOnNextDrop,
    Confirmed,
}

struct FileSource {
    file: File,
    length: u64,
    suggested_name: String,
    cancelled: AtomicBool,
    shutdown: Mutex<Option<ReadShutdown>>,
}

impl std::fmt::Debug for FileSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileSource")
            .field("length", &self.length)
            .field("suggested_name", &self.suggested_name)
            .finish_non_exhaustive()
    }
}

impl FileSource {
    fn install_shutdown(&self, shutdown: ReadShutdown) {
        if self.cancelled.load(Ordering::Acquire) {
            shutdown.stop();
            return;
        }
        *lock(&self.shutdown) = Some(shutdown);
        if self.cancelled.load(Ordering::Acquire)
            && let Some(shutdown) = lock(&self.shutdown).take()
        {
            shutdown.stop();
        }
    }

    fn clear_shutdown(&self) {
        lock(&self.shutdown).take();
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(shutdown) = lock(&self.shutdown).take() {
            shutdown.stop();
        }
    }

    fn ensure_active(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(io::Error::new(io::ErrorKind::Interrupted, "file drop was cancelled"))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
struct OfferEntry {
    binding: FileDropTuple,
    source: Option<Arc<FileSource>>,
    deadline: Instant,
    transfer: Option<u64>,
    terminal: Option<FileResult>,
    cancelled: bool,
    terminal_deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct TransferEntry {
    drop_id: u64,
    context_id: u64,
    surface_id: u64,
    generation: FileTransferGeneration,
    committed_offset: u64,
    maximum_record_body: u32,
    maximum_body_bytes: u64,
    maximum_records: u64,
    active: bool,
    deadline: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalDropDisposition {
    NoBinding,
    ConsentRequired(&'static str),
    Offered,
    Rejected(&'static str),
}

pub(crate) fn local_paste_text(path: &Path) -> String {
    let path: String = path.to_string_lossy().into();
    path + " "
}

#[derive(Debug, Default)]
pub(crate) struct FileDropManager {
    bindings: HashMap<BindingKey, BindingEntry>,
    offers: HashMap<(SessionIdentity, u64), OfferEntry>,
    transfers: HashMap<(SessionIdentity, u64), TransferEntry>,
    binding_epochs: HashMap<BindingKey, (FileDropBinding, FileDropGrant)>,
    next_drop_id: u64,
    next_activation: u64,
    grant_generations: HashMap<SessionIdentity, FileDropGrantGeneration>,
    cancellations: Vec<(SessionIdentity, CancelFileDrop)>,
}

impl FileDropManager {
    pub(crate) fn set_binding(
        &mut self,
        session: SessionIdentity,
        binding: FileDropBinding,
        allow: bool,
    ) -> Result<FileDropGrant, &'static str> {
        binding.validate(binding.surface_id).map_err(|_| "invalid file-drop binding")?;
        let key =
            BindingKey { session, context_id: binding.context_id, surface_id: binding.surface_id };
        if let Some((previous, reply)) = self.binding_epochs.get(&key) {
            if binding.producer_epoch < previous.producer_epoch {
                return Err("stale file-drop binding epoch");
            }
            if binding.producer_epoch == previous.producer_epoch {
                return (binding == *previous)
                    .then_some(*reply)
                    .ok_or("file-drop epoch was reused with different fields");
            }
        }

        if binding.disabled() {
            self.remove_binding(key);
            for entry in self.bindings.values_mut() {
                entry.consent = ConsentState::Unconfirmed;
            }
            let reply = FileDropGrant {
                producer_epoch: binding.producer_epoch,
                grant_generation: FileDropGrantGeneration::ZERO,
                context_id: binding.context_id,
                surface_id: binding.surface_id,
                surface_generation: binding.surface_generation,
                state: FileDropBindingState::Disabled,
                destination: None,
                maximum_file_bytes: 0,
                maximum_pending_offers: 0,
                maximum_active_transfers: 0,
                maximum_record_body: 0,
                acceptance_timeout_us: 0,
                idle_timeout_us: 0,
                reason: 0,
            };
            self.binding_epochs.insert(key, (binding, reply));
            return Ok(reply);
        }

        let generation = self
            .grant_generations
            .entry(session)
            .or_insert(FileDropGrantGeneration::ZERO)
            .advance()
            .map_err(|_| "file-drop grant generation exhausted")?;
        self.grant_generations.insert(session, generation);
        let state =
            if allow { FileDropBindingState::Enabled } else { FileDropBindingState::Denied };
        let reply = FileDropGrant {
            producer_epoch: binding.producer_epoch,
            grant_generation: generation,
            context_id: binding.context_id,
            surface_id: binding.surface_id,
            surface_generation: binding.surface_generation,
            state,
            destination: binding.destination,
            maximum_file_bytes: binding.maximum_file_bytes,
            maximum_pending_offers: binding.maximum_pending_offers,
            maximum_active_transfers: binding.maximum_active_transfers,
            maximum_record_body: binding.maximum_record_body,
            acceptance_timeout_us: binding.acceptance_timeout_us,
            idle_timeout_us: binding.idle_timeout_us,
            reason: (!allow) as u64,
        };
        if allow {
            for entry in self.bindings.values_mut() {
                entry.consent = ConsentState::Unconfirmed;
            }
            self.next_activation = self
                .next_activation
                .checked_add(1)
                .ok_or("file-drop activation order exhausted")?;
            self.bindings.insert(
                key,
                BindingEntry {
                    binding: binding.clone(),
                    grant: reply,
                    activation: self.next_activation,
                    consent: ConsentState::Unconfirmed,
                },
            );
        }
        self.binding_epochs.insert(key, (binding, reply));
        Ok(reply)
    }

    pub(crate) fn hover_label(
        &self,
        hit: Option<(SurfaceIdentity, SurfaceGeneration)>,
    ) -> Option<&'static str> {
        let binding = self.effective_binding(hit)?;
        match binding.binding.destination {
            Some(file_drop::FileDropDestination::ShellCwd) => Some("Copy to remote shell"),
            Some(file_drop::FileDropDestination::DesktopFolder) => Some("Copy to remote desktop"),
            None => None,
        }
    }

    pub(crate) fn offer_local_file(
        &mut self,
        path: &Path,
        hit: Option<(SurfaceIdentity, SurfaceGeneration)>,
    ) -> (LocalDropDisposition, Option<(SessionIdentity, FileDropOffer)>) {
        self.expire();
        let Some(key) = self.effective_binding_key(hit) else {
            return (LocalDropDisposition::NoBinding, None);
        };
        let source = match open_source(path) {
            Ok(source) => Arc::new(source),
            Err(_) => {
                return (LocalDropDisposition::Rejected("Only regular files can be copied"), None);
            },
        };
        let entry = self.bindings.get_mut(&key).expect("effective binding is live");
        if source.length > entry.grant.maximum_file_bytes {
            return (
                LocalDropDisposition::Rejected("The dropped file exceeds the remote limit"),
                None,
            );
        }
        match entry.consent {
            ConsentState::Unconfirmed => {
                entry.consent = ConsentState::ConfirmOnNextDrop;
                return (LocalDropDisposition::ConsentRequired(self.hover_label_for(key)), None);
            },
            ConsentState::ConfirmOnNextDrop => entry.consent = ConsentState::Confirmed,
            ConsentState::Confirmed => {},
        }
        let pending = self
            .offers
            .iter()
            .filter(|((session, _), offer)| {
                *session == key.session
                    && offer.binding.context_id == key.context_id
                    && offer.binding.surface_id == key.surface_id
                    && offer.transfer.is_none()
                    && offer.terminal.is_none()
                    && !offer.cancelled
            })
            .count() as u64;
        if pending >= entry.grant.maximum_pending_offers {
            return (LocalDropDisposition::Rejected("Too many file drops are pending"), None);
        }
        self.next_drop_id = match self.next_drop_id.checked_add(1) {
            Some(id) if id != 0 => id,
            _ => return (LocalDropDisposition::Rejected("File-drop ID space is exhausted"), None),
        };
        let tuple = FileDropTuple {
            producer_epoch: entry.binding.producer_epoch,
            grant_generation: entry.grant.grant_generation,
            context_id: entry.binding.context_id,
            surface_id: entry.binding.surface_id,
            surface_generation: entry.binding.surface_generation,
            drop_id: self.next_drop_id,
        };
        let offer = FileDropOffer {
            binding: tuple,
            suggested_name: source.suggested_name.clone(),
            declared_length: source.length,
        };
        let deadline = Instant::now()
            .checked_add(std::time::Duration::from_micros(entry.grant.acceptance_timeout_us))
            .unwrap_or_else(Instant::now);
        self.offers.insert(
            (key.session, self.next_drop_id),
            OfferEntry {
                binding: tuple,
                source: Some(source),
                deadline,
                transfer: None,
                terminal: None,
                cancelled: false,
                terminal_deadline: None,
            },
        );
        (LocalDropDisposition::Offered, Some((key.session, offer)))
    }

    pub(crate) fn accept(
        &mut self,
        session: SessionIdentity,
        acceptance: AcceptFileDrop,
    ) -> Result<vivid_protocol::file_drop::FileDropAccepted, &'static str> {
        self.expire();
        let offer = self
            .offers
            .get(&(session, acceptance.binding.drop_id))
            .ok_or("file-drop offer is absent")?;
        if offer.binding != acceptance.binding {
            return Err("file-drop acceptance has a stale binding identity");
        }
        if offer.cancelled || offer.terminal.is_some() {
            return Err("file-drop offer is already terminal");
        }
        if let Some(transfer_id) = offer.transfer {
            if transfer_id == acceptance.transfer_id {
                let idle_timeout_us = self
                    .bindings
                    .get(&BindingKey {
                        session,
                        context_id: acceptance.binding.context_id,
                        surface_id: acceptance.binding.surface_id,
                    })
                    .ok_or("file-drop binding is no longer live")?
                    .grant
                    .idle_timeout_us;
                return Ok(vivid_protocol::file_drop::FileDropAccepted {
                    drop_id: acceptance.binding.drop_id,
                    transfer_id,
                    transfer_generation: acceptance.transfer_generation,
                    open_timeout_us: idle_timeout_us,
                });
            }
            return Err("file-drop offer was already accepted");
        }
        let binding = self
            .bindings
            .get(&BindingKey {
                session,
                context_id: acceptance.binding.context_id,
                surface_id: acceptance.binding.surface_id,
            })
            .ok_or("file-drop binding is no longer live")?;
        let active = self.transfers.iter().filter(|((owner, _), value)| {
            *owner == session
                && value.context_id == acceptance.binding.context_id
                && value.surface_id == acceptance.binding.surface_id
                && self
                    .offers
                    .get(&(session, value.drop_id))
                    .is_some_and(|offer| !offer.cancelled && offer.terminal.is_none())
        });
        if active.count() as u64 >= binding.grant.maximum_active_transfers {
            return Err("file-drop active-transfer limit reached");
        }
        if acceptance.maximum_record_body > binding.grant.maximum_record_body {
            return Err("file-drop record limit exceeds the binding");
        }
        if self.transfers.contains_key(&(session, acceptance.transfer_id)) {
            return Err("file-transfer ID is already live");
        }
        self.offers
            .get_mut(&(session, acceptance.binding.drop_id))
            .expect("validated file-drop offer remains live")
            .transfer = Some(acceptance.transfer_id);
        self.transfers.insert(
            (session, acceptance.transfer_id),
            TransferEntry {
                drop_id: acceptance.binding.drop_id,
                context_id: acceptance.binding.context_id,
                surface_id: acceptance.binding.surface_id,
                generation: acceptance.transfer_generation,
                committed_offset: 0,
                maximum_record_body: acceptance.maximum_record_body,
                maximum_body_bytes: acceptance.initial_maximum_body_bytes,
                maximum_records: acceptance.initial_maximum_records,
                active: false,
                deadline: Instant::now()
                    .checked_add(std::time::Duration::from_micros(binding.grant.idle_timeout_us))
                    .unwrap_or_else(Instant::now),
            },
        );
        Ok(vivid_protocol::file_drop::FileDropAccepted {
            drop_id: acceptance.binding.drop_id,
            transfer_id: acceptance.transfer_id,
            transfer_generation: acceptance.transfer_generation,
            open_timeout_us: binding.grant.idle_timeout_us,
        })
    }

    pub(crate) fn advance(
        &mut self,
        session: SessionIdentity,
        advance: AdvanceFileTransfer,
    ) -> Result<vivid_protocol::file_drop::FileTransferAdvanced, &'static str> {
        let transfer = self
            .transfers
            .get_mut(&(session, advance.transfer_id))
            .ok_or("file transfer is absent")?;
        if transfer.context_id != advance.context_id
            || transfer.surface_id != advance.surface_id
            || transfer.drop_id != advance.drop_id
            || transfer.generation != advance.expected_generation
            || advance.new_generation
                != transfer
                    .generation
                    .advance()
                    .map_err(|_| "file-transfer generation exhausted")?
            || advance.committed_offset
                > self.offers[&(session, transfer.drop_id)]
                    .source
                    .as_ref()
                    .ok_or("file-drop offer is terminal")?
                    .length
        {
            return Err("file-transfer advance has stale or impossible state");
        }
        transfer.generation = advance.new_generation;
        transfer.committed_offset = advance.committed_offset;
        transfer.maximum_body_bytes = advance.maximum_body_bytes;
        transfer.maximum_records = advance.maximum_records;
        transfer.active = false;
        let idle_timeout_us = self
            .bindings
            .get(&BindingKey {
                session,
                context_id: transfer.context_id,
                surface_id: transfer.surface_id,
            })
            .ok_or("file-drop binding is no longer live")?
            .grant
            .idle_timeout_us;
        transfer.deadline = Instant::now()
            .checked_add(std::time::Duration::from_micros(idle_timeout_us))
            .unwrap_or_else(Instant::now);
        Ok(vivid_protocol::file_drop::FileTransferAdvanced {
            transfer_id: advance.transfer_id,
            generation: advance.new_generation,
            committed_offset: advance.committed_offset,
            open_timeout_us: idle_timeout_us,
        })
    }

    pub(crate) fn cancel(
        &mut self,
        session: SessionIdentity,
        cancellation: CancelFileDrop,
    ) -> Result<(), &'static str> {
        let offer = self
            .offers
            .get_mut(&(session, cancellation.binding.drop_id))
            .ok_or("file-drop offer is absent")?;
        if offer.binding != cancellation.binding {
            return Err("file-drop cancellation has a stale binding identity");
        }
        if let Some(transfer) = offer.transfer
            && let Some(transfer) = self.transfers.get_mut(&(session, transfer))
        {
            transfer.active = false;
        }
        cancel_offer_source(offer);
        offer.cancelled = true;
        offer.terminal_deadline = Instant::now().checked_add(std::time::Duration::from_micros(
            file_drop::FILE_DROP_RESULT_RETENTION_US,
        ));
        Ok(())
    }

    pub(crate) fn status(
        &mut self,
        session: SessionIdentity,
        query: QueryFileDrop,
    ) -> Result<FileDropStatus, &'static str> {
        self.expire();
        let offer =
            self.offers.get(&(session, query.drop_id)).ok_or("file-drop offer is absent")?;
        if let Some(result) = &offer.terminal {
            return Ok(FileDropStatus {
                drop_id: query.drop_id,
                state: if matches!(
                    result.result,
                    FileResultCode::Committed | FileResultCode::AlreadyCommitted
                ) {
                    FileDropState::Committed
                } else if result.result == FileResultCode::Cancelled {
                    FileDropState::Cancelled
                } else {
                    FileDropState::Failed
                },
                transfer_id: result.transfer_id,
                generation: result.transfer_generation,
                committed_offset: result.committed_length,
                result: Some(result.result),
                final_name: result.final_name.clone(),
            });
        }
        if offer.cancelled {
            let transfer = offer.transfer.and_then(|id| self.transfers.get(&(session, id)));
            return Ok(FileDropStatus {
                drop_id: query.drop_id,
                state: FileDropState::Cancelled,
                transfer_id: offer.transfer.unwrap_or(0),
                generation: transfer.map_or(FileTransferGeneration::ZERO, |value| value.generation),
                committed_offset: transfer.map_or(0, |value| value.committed_offset),
                result: Some(FileResultCode::Cancelled),
                final_name: String::new(),
            });
        }
        let transfer = offer.transfer.and_then(|id| self.transfers.get(&(session, id)));
        Ok(FileDropStatus {
            drop_id: query.drop_id,
            state: transfer.map_or(FileDropState::Offered, |transfer| {
                if transfer.active { FileDropState::Transferring } else { FileDropState::Accepted }
            }),
            transfer_id: transfer.map_or(0, |_| offer.transfer.unwrap_or(0)),
            generation: transfer.map_or(FileTransferGeneration::ZERO, |value| value.generation),
            committed_offset: transfer.map_or(0, |value| value.committed_offset),
            result: None,
            final_name: String::new(),
        })
    }

    fn begin_transfer(
        &mut self,
        session: SessionIdentity,
        open: &FileTransferOpen,
    ) -> Result<Arc<FileSource>, &'static str> {
        self.expire();
        let transfer = self
            .transfers
            .get_mut(&(session, open.transfer_id))
            .ok_or("file transfer is absent")?;
        if transfer.context_id != open.context_id
            || transfer.surface_id != open.surface_id
            || transfer.drop_id != open.drop_id
            || transfer.generation != open.transfer_generation
            || transfer.committed_offset != open.resume_offset
            || transfer.maximum_record_body != open.maximum_record_body
            || transfer.maximum_body_bytes != open.maximum_body_bytes
            || transfer.maximum_records != open.maximum_records
            || transfer.active
        {
            return Err("file-transfer open does not match the accepted generation");
        }
        let offer = self.offers.get(&(session, open.drop_id)).ok_or("file-drop offer is absent")?;
        if offer.binding.producer_epoch != open.producer_epoch
            || offer.binding.grant_generation != open.grant_generation
            || offer.binding.surface_generation != open.surface_generation
        {
            return Err("file-transfer open has a stale complete binding identity");
        }
        if offer.cancelled || offer.terminal.is_some() {
            return Err("file-drop offer is terminal");
        }
        transfer.active = true;
        offer.source.clone().ok_or("file-drop source is no longer available")
    }

    fn binding_idle_timeout(
        &self,
        session: SessionIdentity,
        open: &FileTransferOpen,
    ) -> Option<u64> {
        let transfer = self.transfers.get(&(session, open.transfer_id))?;
        self.bindings
            .get(&BindingKey {
                session,
                context_id: transfer.context_id,
                surface_id: transfer.surface_id,
            })
            .map(|binding| binding.grant.idle_timeout_us)
    }

    fn finish_transfer(&mut self, session: SessionIdentity, result: FileResult) {
        if let Some(transfer) = self.transfers.get_mut(&(session, result.transfer_id)) {
            transfer.active = false;
            transfer.committed_offset = result.committed_length;
            if let Some(offer) = self.offers.get_mut(&(session, transfer.drop_id)) {
                offer.source = None;
                offer.terminal = Some(result);
                offer.terminal_deadline = Instant::now().checked_add(
                    std::time::Duration::from_micros(file_drop::FILE_DROP_RESULT_RETENTION_US),
                );
            }
        }
    }

    fn connection_lost(&mut self, session: SessionIdentity, transfer_id: u64) {
        if let Some(transfer) = self.transfers.get_mut(&(session, transfer_id)) {
            transfer.active = false;
            if let Some(binding) = self.bindings.get(&BindingKey {
                session,
                context_id: transfer.context_id,
                surface_id: transfer.surface_id,
            }) {
                transfer.deadline = Instant::now()
                    .checked_add(std::time::Duration::from_micros(binding.grant.idle_timeout_us))
                    .unwrap_or_else(Instant::now);
            }
        }
    }

    pub(crate) fn next_deadline(&self, session: SessionIdentity) -> Option<Instant> {
        self.offers
            .iter()
            .filter(|((owner, _), _)| *owner == session)
            .filter_map(|(_, offer)| {
                if let Some(deadline) = offer.terminal_deadline {
                    Some(deadline)
                } else if let Some(transfer_id) = offer.transfer {
                    self.transfers
                        .get(&(session, transfer_id))
                        .filter(|transfer| !transfer.active)
                        .map(|transfer| transfer.deadline)
                } else {
                    Some(offer.deadline)
                }
            })
            .min()
    }

    pub(crate) fn service_timeouts(&mut self, session: SessionIdentity) -> Vec<CancelFileDrop> {
        self.expire();
        let mut matched = Vec::new();
        self.cancellations.retain(|(owner, cancellation)| {
            if *owner == session {
                matched.push(*cancellation);
                false
            } else {
                true
            }
        });
        matched
    }

    pub(crate) fn remove_session(&mut self, session: SessionIdentity) {
        self.remove_session_bindings(session);
        self.binding_epochs.retain(|key, _| key.session != session);
        self.grant_generations.remove(&session);
        self.cancellations.retain(|(owner, _)| *owner != session);
    }

    pub(crate) fn remove_contexts(
        &mut self,
        session: SessionIdentity,
        contexts: &std::collections::HashSet<u64>,
    ) {
        self.bindings
            .retain(|key, _| key.session != session || !contexts.contains(&key.context_id));
        self.binding_epochs
            .retain(|key, _| key.session != session || !contexts.contains(&key.context_id));
        self.offers.retain(|(owner, _), offer| {
            let remove = *owner == session && contexts.contains(&offer.binding.context_id);
            if remove {
                cancel_offer_source(offer);
            }
            !remove
        });
        self.transfers.retain(|(owner, _), transfer| {
            *owner != session || !contexts.contains(&transfer.context_id)
        });
        self.cancellations.retain(|(owner, cancellation)| {
            *owner != session || !contexts.contains(&cancellation.binding.context_id)
        });
    }

    pub(crate) fn remove_surface(
        &mut self,
        session: SessionIdentity,
        context_id: u64,
        surface_id: u64,
    ) {
        let key = BindingKey { session, context_id, surface_id };
        self.bindings.remove(&key);
        self.binding_epochs.remove(&key);
        self.offers.retain(|(owner, _), offer| {
            let remove = *owner == session
                && offer.binding.context_id == context_id
                && offer.binding.surface_id == surface_id;
            if remove {
                cancel_offer_source(offer);
            }
            !remove
        });
        self.transfers.retain(|(owner, _), transfer| {
            *owner != session
                || transfer.context_id != context_id
                || transfer.surface_id != surface_id
        });
        self.cancellations.retain(|(owner, cancellation)| {
            *owner != session
                || cancellation.binding.context_id != context_id
                || cancellation.binding.surface_id != surface_id
        });
    }

    fn remove_session_bindings(&mut self, session: SessionIdentity) {
        self.bindings.retain(|key, _| key.session != session);
        self.offers.retain(|(owner, _), offer| {
            let remove = *owner == session;
            if remove {
                cancel_offer_source(offer);
            }
            !remove
        });
        self.transfers.retain(|(owner, _), _| *owner != session);
    }

    fn remove_binding(&mut self, key: BindingKey) {
        self.bindings.remove(&key);
        self.offers.retain(|(owner, _), offer| {
            let remove = *owner == key.session
                && offer.binding.context_id == key.context_id
                && offer.binding.surface_id == key.surface_id;
            if remove {
                cancel_offer_source(offer);
            }
            !remove
        });
        self.transfers.retain(|(owner, _), transfer| {
            *owner != key.session
                || transfer.context_id != key.context_id
                || transfer.surface_id != key.surface_id
        });
    }

    fn effective_binding(
        &self,
        hit: Option<(SurfaceIdentity, SurfaceGeneration)>,
    ) -> Option<&BindingEntry> {
        let key = self.effective_binding_key(hit)?;
        self.bindings.get(&key)
    }

    fn effective_binding_key(
        &self,
        hit: Option<(SurfaceIdentity, SurfaceGeneration)>,
    ) -> Option<BindingKey> {
        if let Some((surface, generation)) = hit {
            let key = BindingKey {
                session: surface.context.session,
                context_id: surface.context.context_id,
                surface_id: surface.surface_id,
            };
            if self.bindings.get(&key).is_some_and(|entry| {
                entry.binding.surface_generation == generation
                    && entry.grant.state == FileDropBindingState::Enabled
            }) {
                return Some(key);
            }
        }
        self.bindings
            .iter()
            .filter(|(_, entry)| entry.binding.surface_id == 0)
            .max_by_key(|(_, entry)| entry.activation)
            .map(|(key, _)| *key)
    }

    fn hover_label_for(&self, key: BindingKey) -> &'static str {
        match self.bindings[&key].binding.destination {
            Some(file_drop::FileDropDestination::ShellCwd) => "Copy to remote shell",
            Some(file_drop::FileDropDestination::DesktopFolder) => "Copy to remote desktop",
            None => "Copy file",
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let terminal_expired = self
            .offers
            .iter()
            .filter_map(|(key, offer)| {
                offer.terminal_deadline.is_some_and(|deadline| deadline <= now).then_some(*key)
            })
            .collect::<Vec<_>>();
        for key in terminal_expired {
            if let Some(mut offer) = self.offers.remove(&key) {
                cancel_offer_source(&mut offer);
                if let Some(transfer_id) = offer.transfer {
                    self.transfers.remove(&(key.0, transfer_id));
                }
            }
        }
        let timed_out = self
            .offers
            .iter()
            .filter_map(|(key, offer)| {
                (!offer.cancelled
                    && offer.terminal.is_none()
                    && ((offer.deadline <= now && offer.transfer.is_none())
                        || offer.transfer.is_some_and(|transfer_id| {
                            self.transfers.get(&(key.0, transfer_id)).is_some_and(|transfer| {
                                !transfer.active && transfer.deadline <= now
                            })
                        })))
                .then_some(*key)
            })
            .collect::<Vec<_>>();
        for key in timed_out {
            if let Some(offer) = self.offers.get_mut(&key) {
                if let Some(transfer_id) = offer.transfer
                    && let Some(transfer) = self.transfers.get_mut(&(key.0, transfer_id))
                {
                    transfer.active = false;
                }
                cancel_offer_source(offer);
                offer.cancelled = true;
                offer.terminal_deadline = now.checked_add(std::time::Duration::from_micros(
                    file_drop::FILE_DROP_RESULT_RETENTION_US,
                ));
                self.cancellations
                    .push((key.0, CancelFileDrop { binding: offer.binding, reason: 1 }));
            }
        }
    }
}

pub(super) fn handle_connection(
    reader: &mut Reader,
    shared: &Arc<ServiceShared>,
    pending: &PendingConnection,
) -> io::Result<()> {
    let writer = reader.writer(ConnectionKind::FileTransfer)?;
    let first = reader.read_record(ConnectionKind::FileTransfer)?;
    if first.record_type != registry::record::FILE_TRANSFER_OPEN || first.sequence != 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected FILE_TRANSFER_OPEN"));
    }
    let open = FileTransferOpen::decode(&first.body)?;
    if first.object_id != open.transfer_id {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file-transfer object mismatch"));
    }
    let session =
        lock(&shared.registry).sessions.get(&open.session_id).cloned().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "file-transfer session is absent")
        })?;
    authenticate_open(&session, &open, &writer)?;
    let shutdown = reader.shutdown_handle()?;
    let source =
        lock(&shared.file_drops).begin_transfer(session.identity, &open).map_err(|message| {
            reject_open(&writer, open.transfer_id, messages::ERROR_PRECONDITION_FAILED, message)
        })?;
    source.install_shutdown(shutdown);
    pending.authenticated(reader)?;
    let idle_timeout = lock(&shared.file_drops)
        .binding_idle_timeout(session.identity, &open)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file-drop binding is absent"))?;
    reader.set_record_idle_timeout(std::time::Duration::from_micros(idle_timeout))?;
    reader.set_maximum(64 * 1024)?;
    writer.write_record(
        registry::record::FILE_TRANSFER_ACCEPTED,
        open.transfer_id,
        &FileTransferAccepted {
            transfer_id: open.transfer_id,
            transfer_generation: open.transfer_generation,
            resume_offset: open.resume_offset,
        }
        .encode()?,
    )?;
    let result = stream_source(reader, &writer, &open, &source);
    source.clear_shutdown();
    match result {
        Ok(result) => lock(&shared.file_drops).finish_transfer(session.identity, result),
        Err(error) => {
            lock(&shared.file_drops).connection_lost(session.identity, open.transfer_id);
            wake_actor(&session);
            return Err(error);
        },
    }
    wake_actor(&session);
    Ok(())
}

fn wake_actor(session: &SessionRuntime) {
    if let Some(ingress) = lock(&session.actor_ingress).as_ref() {
        let _ = ingress.try_send(super::ActorMessage::Wake);
    }
}

fn authenticate_open(
    session: &SessionRuntime,
    open: &FileTransferOpen,
    writer: &Writer,
) -> io::Result<()> {
    if !session.supports(registry::FILE_DROP) {
        return Err(reject_open(
            writer,
            open.transfer_id,
            messages::ERROR_UNSUPPORTED_PROFILE,
            "file-drop-v1 was not negotiated",
        ));
    }
    let expected = auth::file_transfer_tag(
        session.channel_key.expose(),
        open.session_id,
        open.context_id,
        open.surface_id,
        open.producer_epoch.get(),
        open.grant_generation.get(),
        open.surface_generation.get(),
        open.drop_id,
        open.transfer_id,
        open.transfer_generation.get(),
        open.resume_offset,
        open.maximum_record_body,
        open.maximum_body_bytes,
        open.maximum_records,
        &open.client_nonce,
    );
    if !auth::verify_tag(&expected, &open.authentication_tag) {
        return Err(reject_open(
            writer,
            open.transfer_id,
            messages::ERROR_AUTH_FAILED,
            "file-transfer authentication failed",
        ));
    }
    Ok(())
}

fn stream_source(
    reader: &mut Reader,
    writer: &Writer,
    open: &FileTransferOpen,
    source: &FileSource,
) -> io::Result<FileResult> {
    source.ensure_active()?;
    let mut file = source.file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    let chunk_size =
        usize::try_from(open.maximum_record_body - file_drop::FILE_DATA_PREFIX_SIZE as u32)
            .unwrap_or(64 * 1024)
            .min(1024 * 1024);
    let mut buffer = vec![0_u8; chunk_size];
    let mut hasher = Sha256::new();
    let mut offset = 0_u64;
    let mut sent_body = 0_u64;
    let mut sent_records = 0_u64;
    let mut maximum_body = open.maximum_body_bytes;
    let mut maximum_records = open.maximum_records;
    while offset < open.resume_offset {
        source.ensure_active()?;
        let remaining = usize::try_from((open.resume_offset - offset).min(chunk_size as u64))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file length exceeds usize"))?;
        file.read_exact(&mut buffer[..remaining])?;
        hasher.update(&buffer[..remaining]);
        offset = offset
            .checked_add(remaining as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file offset overflow"))?;
    }
    while offset < source.length {
        source.ensure_active()?;
        let remaining = usize::try_from((source.length - offset).min(chunk_size as u64))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file length exceeds usize"))?;
        file.read_exact(&mut buffer[..remaining])?;
        hasher.update(&buffer[..remaining]);
        let body_length = (file_drop::FILE_DATA_PREFIX_SIZE + remaining) as u64;
        while sent_body.checked_add(body_length).is_none_or(|value| value > maximum_body)
            || sent_records.checked_add(1).is_none_or(|value| value > maximum_records)
        {
            source.ensure_active()?;
            read_credit(
                reader,
                open,
                &mut maximum_body,
                &mut maximum_records,
                sent_body,
                sent_records,
            )?;
        }
        source.ensure_active()?;
        let prefix = file_drop::file_data_prefix(offset, remaining)?;
        writer.write_record_parts(
            registry::record::FILE_DATA,
            open.transfer_id,
            &[&prefix, &buffer[..remaining]],
        )?;
        sent_body = sent_body
            .checked_add(body_length)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file credit overflow"))?;
        sent_records = sent_records
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file record overflow"))?;
        offset = offset
            .checked_add(remaining as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file offset overflow"))?;
    }
    let finish = FileFinish {
        transfer_id: open.transfer_id,
        transfer_generation: open.transfer_generation,
        final_length: source.length,
        sha256: hasher.finalize().into(),
    };
    source.ensure_active()?;
    writer.write_record(registry::record::FILE_FINISH, open.transfer_id, &finish.encode()?)?;
    loop {
        source.ensure_active()?;
        let record = reader.read_record(ConnectionKind::FileTransfer)?;
        if record.object_id != open.transfer_id {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file result object mismatch"));
        }
        match record.record_type {
            registry::record::FILE_RESULT => {
                let result = FileResult::decode(&record.body)?;
                if result.transfer_id != open.transfer_id
                    || result.transfer_generation != open.transfer_generation
                    || result.committed_length > source.length
                    || (matches!(
                        result.result,
                        FileResultCode::Committed | FileResultCode::AlreadyCommitted
                    ) && result.committed_length != source.length)
                {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid FILE_RESULT"));
                }
                return Ok(result);
            },
            registry::record::MAX_FILE_DATA => {
                let maximum = MaximumFileData::decode(&record.body)?;
                validate_credit_identity(maximum, open)?;
            },
            registry::record::FILE_TRANSFER_ABORT => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "file transfer aborted",
                ));
            },
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected file-transfer result record",
                ));
            },
        }
    }
}

fn read_credit(
    reader: &mut Reader,
    open: &FileTransferOpen,
    maximum_body: &mut u64,
    maximum_records: &mut u64,
    sent_body: u64,
    sent_records: u64,
) -> io::Result<()> {
    let record = reader.read_record(ConnectionKind::FileTransfer)?;
    if record.object_id != open.transfer_id {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file credit object mismatch"));
    }
    match record.record_type {
        registry::record::MAX_FILE_DATA => {
            let maximum = MaximumFileData::decode(&record.body)?;
            validate_credit_identity(maximum, open)?;
            if maximum.maximum_body_bytes < *maximum_body
                || maximum.maximum_records < *maximum_records
                || maximum.maximum_body_bytes < sent_body
                || maximum.maximum_records < sent_records
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file credit moved backward",
                ));
            }
            *maximum_body = maximum.maximum_body_bytes;
            *maximum_records = maximum.maximum_records;
            Ok(())
        },
        registry::record::FILE_TRANSFER_ABORT => {
            Err(io::Error::new(io::ErrorKind::ConnectionAborted, "file transfer aborted"))
        },
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "expected MAX_FILE_DATA")),
    }
}

fn validate_credit_identity(maximum: MaximumFileData, open: &FileTransferOpen) -> io::Result<()> {
    if maximum.transfer_id != open.transfer_id
        || maximum.transfer_generation != open.transfer_generation
    {
        Err(io::Error::new(io::ErrorKind::InvalidData, "file credit has a stale identity"))
    } else {
        Ok(())
    }
}

fn cancel_offer_source(offer: &mut OfferEntry) {
    if let Some(source) = offer.source.take() {
        source.cancel();
    }
}

fn reject_open(writer: &Writer, object_id: u64, code: u64, diagnostic: &'static str) -> io::Error {
    if let Ok(body) = protocol_error(0, code, true, diagnostic) {
        let _ = writer.write_record(messages::ERROR, object_id, &body);
    }
    io::Error::new(io::ErrorKind::InvalidData, diagnostic)
}

fn open_source(path: &Path) -> io::Result<FileSource> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "source is not a regular file"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "source is a reparse point"));
        }
    }
    let suggested_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| file_drop::validate_suggested_name(name).is_ok())
        .unwrap_or("dropped-file")
        .to_owned();
    Ok(FileSource {
        file,
        length: metadata.len(),
        suggested_name,
        cancelled: AtomicBool::new(false),
        shutdown: Mutex::new(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::file_drop::{
        DEFAULT_ACTIVE_FILE_TRANSFERS, DEFAULT_FILE_DROP_ACCEPTANCE_US,
        DEFAULT_FILE_TRANSFER_IDLE_US, DEFAULT_PENDING_FILE_DROPS, FileDropDestination,
    };
    use vivid_protocol::identity::PresenterInstanceId;
    use vivid_protocol::revision::FileDropEpoch;

    fn session(id: u64) -> SessionIdentity {
        SessionIdentity::new(PresenterInstanceId([3; 16]), id).unwrap()
    }

    fn binding(
        context_id: u64,
        surface_id: u64,
        destination: Option<FileDropDestination>,
    ) -> FileDropBinding {
        let enabled = destination.is_some();
        FileDropBinding {
            producer_epoch: if enabled { FileDropEpoch::ONE } else { FileDropEpoch::new(2) },
            context_id,
            surface_id,
            surface_generation: if surface_id == 0 {
                SurfaceGeneration::ZERO
            } else {
                SurfaceGeneration::ONE
            },
            destination,
            maximum_file_bytes: if enabled { 1024 } else { 0 },
            maximum_pending_offers: if enabled { DEFAULT_PENDING_FILE_DROPS } else { 0 },
            maximum_active_transfers: if enabled { DEFAULT_ACTIVE_FILE_TRANSFERS } else { 0 },
            maximum_record_body: if enabled { 4096 } else { 0 },
            acceptance_timeout_us: if enabled { DEFAULT_FILE_DROP_ACCEPTANCE_US } else { 0 },
            idle_timeout_us: if enabled { DEFAULT_FILE_TRANSFER_IDLE_US } else { 0 },
        }
    }

    #[test]
    fn hit_testing_and_stack_are_owner_scoped() {
        let first = session(1);
        let second = session(2);
        let first_surface = first.context(4).unwrap().surface(8).unwrap();
        let second_surface = second.context(4).unwrap().surface(8).unwrap();
        let mut manager = FileDropManager::default();

        manager
            .set_binding(first, binding(4, 8, Some(FileDropDestination::ShellCwd)), true)
            .unwrap();
        manager
            .set_binding(first, binding(4, 0, Some(FileDropDestination::ShellCwd)), true)
            .unwrap();
        manager
            .set_binding(second, binding(4, 0, Some(FileDropDestination::DesktopFolder)), true)
            .unwrap();

        assert_eq!(
            manager.hover_label(Some((first_surface, SurfaceGeneration::ONE))),
            Some("Copy to remote shell")
        );
        assert_eq!(manager.hover_label(None), Some("Copy to remote desktop"));

        manager.set_binding(second, binding(4, 0, None), true).unwrap();
        assert_eq!(manager.hover_label(None), Some("Copy to remote shell"));

        manager
            .set_binding(second, binding(4, 8, Some(FileDropDestination::DesktopFolder)), true)
            .unwrap();
        assert_eq!(
            manager.hover_label(Some((second_surface, SurfaceGeneration::ONE))),
            Some("Copy to remote desktop")
        );
        manager.remove_session(first);
        assert_eq!(
            manager.hover_label(Some((second_surface, SurfaceGeneration::ONE))),
            Some("Copy to remote desktop")
        );
        assert_eq!(manager.hover_label(Some((first_surface, SurfaceGeneration::ONE))), None);
    }

    #[test]
    fn absent_binding_preserves_the_existing_local_paste_bytes() {
        let mut manager = FileDropManager::default();
        let path = Path::new("folder/report.txt");
        assert_eq!(manager.offer_local_file(path, None), (LocalDropDisposition::NoBinding, None));
        assert_eq!(local_paste_text(path), "folder/report.txt ");
    }

    #[test]
    fn cancelling_a_retained_source_is_seen_by_an_active_worker() {
        let source = open_source(Path::new("Cargo.toml")).unwrap();
        source.ensure_active().unwrap();
        source.cancel();
        assert_eq!(source.ensure_active().unwrap_err().kind(), io::ErrorKind::Interrupted);
    }
}
