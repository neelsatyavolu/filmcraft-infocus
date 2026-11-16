//! The session's side of [`TeamLink::poll`](super::TeamLink::poll): what each request outcome and
//! each teammate's update does to the session, and the per-frame [`Session::poll_team`] that
//! drives it.

use filmcraft_project::ItemId;
use serde_json::Value;
use web_time::Instant;

use super::{HEARTBEAT_EVERY, LOCK_EXPIRY, LockInfo, RETRY_AFTER, catch_up, send_deletes, send_sequences, send_shared, still_attached, tell};
use crate::Session;

/// Why the Drive refused or could not do a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// Someone else holds the lock.
    HeldBy { holder: String },
    /// The lock expired or was force-released.
    LockLost,
    /// The base revision is not the server's (`rev`).
    Stale { rev: u64 },
    /// A deleted shared item is still used by a sequence on the server.
    ItemInUse { item: ItemId },
    /// The project's files were changed outside InFocus Cut.
    FilesChanged,
    /// This app is too old for the project.
    UpdateRequired,
    /// The Drive could not be reached (the request is tried again).
    Network(String),
}

/// What the link reports back from [`TeamLink::poll`](super::TeamLink::poll).
#[derive(Clone, Debug, PartialEq)]
pub enum LinkEvent {
    LockTaken {
        seq: ItemId,
    },
    LockRefused {
        seq: ItemId,
        holder: String,
    },
    HeartbeatOk {
        seq: ItemId,
    },
    HeartbeatFailed {
        seq: ItemId,
        error: LinkError,
    },
    SequencePut {
        seq: ItemId,
        rev: u64,
    },
    SequencePutFailed {
        seq: ItemId,
        error: LinkError,
    },
    /// This member's delete of `seq` was done; `rev` is its last revision.
    SequenceDeleted {
        seq: ItemId,
        rev: u64,
    },
    SequenceDeleteFailed {
        seq: ItemId,
        error: LinkError,
    },
    SharedPosted {
        rev: u64,
    },
    SharedPostFailed {
        error: LinkError,
    },
    /// A teammate saved a sequence (the link fetched its part).
    SequenceUpdated {
        seq: ItemId,
        rev: u64,
        by: String,
        json: Value,
    },
    /// A teammate deleted a sequence (its lock goes with it).
    SequenceRemoved {
        seq: ItemId,
        rev: u64,
        by: String,
    },
    /// The shared part changed (the link fetched it).
    SharedUpdated {
        rev: u64,
        shared: Value,
    },
    /// A lock was taken (`Some(holder)`) or released (`None`).
    LockChanged {
        seq: ItemId,
        holder: Option<String>,
    },
}

impl Session {
    /// Run the team side (call regularly; `poll_persistence` does): accept edits made outside
    /// `execute`, apply what the link reports, renew locks every 20 s (a lock without a
    /// successful heartbeat for 120 s is lost), and send due pushes and releases.
    pub fn poll_team(&mut self, now: Instant) {
        if !still_attached(self) {
            return;
        }
        catch_up(self, now);
        let events = self.team.as_mut().map(|t| t.link.poll()).unwrap_or_default();
        for ev in events {
            self.on_link_event(ev, now);
        }
        let mut lost = Vec::new();
        if let Some(t) = self.team.as_mut() {
            for (id, lock) in t.held.iter_mut() {
                if now.saturating_duration_since(lock.last_ok) >= LOCK_EXPIRY {
                    lost.push(*id);
                } else if now.saturating_duration_since(lock.last_sent) >= HEARTBEAT_EVERY {
                    lock.last_sent = now;
                    t.link.heartbeat(*id);
                }
            }
        }
        for id in lost {
            self.lock_lost(id);
        }
        send_sequences(self, now);
        send_shared(self, now);
        send_deletes(self, now);
        if let Some(t) = self.team.as_mut() {
            let done: Vec<ItemId> = t.releasing.iter().filter(|id| !t.dirty.contains_key(id) && !t.in_flight.contains(id)).copied().collect();
            for id in done {
                t.releasing.remove(&id);
                t.held.remove(&id);
                t.link.release(id);
            }
        }
    }

    fn on_link_event(&mut self, ev: LinkEvent, now: Instant) {
        let Some(t) = self.team.as_mut() else { return };
        match ev {
            LinkEvent::LockTaken { seq } => {
                t.requested.remove(&seq);
                t.holders.remove(&seq);
                t.held.insert(seq, LockInfo { last_ok: now, last_sent: now });
            }
            LinkEvent::LockRefused { seq, holder } => {
                t.requested.remove(&seq);
                let name = self.project.item(seq).map(|i| i.name.clone()).unwrap_or_default();
                if let Some(t) = self.team.as_mut() {
                    t.holders.insert(seq, holder.clone());
                }
                tell(self, format!("{holder} is editing “{name}”"));
            }
            LinkEvent::HeartbeatOk { seq } => {
                if let Some(l) = t.held.get_mut(&seq) {
                    l.last_ok = now;
                }
            }
            // tried again in 20 s; the lock is lost after 120 s without success
            LinkEvent::HeartbeatFailed { error: LinkError::Network(_), .. } => {}
            LinkEvent::HeartbeatFailed { seq, error: _ } => self.lock_lost(seq),
            LinkEvent::SequencePut { seq, rev } => {
                t.in_flight.remove(&seq);
                t.local_new.remove(&seq);
                t.revs.insert(seq, rev);
            }
            LinkEvent::SequencePutFailed { seq, error } => match error {
                LinkError::Network(_) => {
                    t.in_flight.remove(&seq);
                    t.dirty.entry(seq).or_insert(now + RETRY_AFTER);
                }
                LinkError::UpdateRequired | LinkError::FilesChanged => {
                    t.in_flight.remove(&seq);
                    t.dirty.entry(seq).or_insert(now);
                    self.sync_stopped(&error);
                }
                LinkError::LockLost | LinkError::Stale { .. } | LinkError::HeldBy { .. } | LinkError::ItemInUse { .. } => self.lock_lost(seq),
            },
            LinkEvent::SequenceDeleted { seq, rev } => {
                t.delete_in_flight.remove(&seq);
                t.deleting.remove(&seq);
                t.held.remove(&seq);
                t.holders.remove(&seq);
                t.revs.insert(seq, rev);
                if t.local_new.contains(&seq) {
                    // put back here (undo) while the delete was on its way: save it again
                    t.requested.insert(seq);
                    t.link.take_lock(seq);
                } else {
                    t.gone.insert(seq);
                }
            }
            LinkEvent::SequenceDeleteFailed { seq, error } => {
                t.delete_in_flight.remove(&seq);
                match error {
                    LinkError::Network(_) => {
                        if let Some(d) = t.deleting.get_mut(&seq) {
                            d.due = now + RETRY_AFTER;
                        }
                    }
                    LinkError::UpdateRequired | LinkError::FilesChanged => self.sync_stopped(&error),
                    LinkError::ItemInUse { .. } => {
                        t.local_new.remove(&seq);
                        if let Some(d) = t.deleting.remove(&seq) {
                            let name = d.item.name.clone();
                            self.put_back(d);
                            if let Some(t) = self.team.as_mut() {
                                t.dirty.insert(seq, now);
                            }
                            tell(self, format!("“{name}” is used in another sequence on the Drive, so it was not deleted"));
                        }
                    }
                    LinkError::LockLost | LinkError::Stale { .. } | LinkError::HeldBy { .. } => self.lock_lost(seq),
                }
            }
            LinkEvent::SequenceRemoved { seq, rev, by } => {
                let name = self.project.item(seq).map(|i| i.name.clone());
                match self.apply_remote_delete(seq, rev) {
                    Ok(()) => {
                        if let Some(name) = name.filter(|_| self.project.item(seq).is_none()) {
                            self.log.push(crate::panels::Level::Info, "team", format!("{by} deleted “{name}”"));
                        }
                    }
                    Err(e) => tell(self, format!("Could not apply {by}’s delete of sequence {}: {e}", seq.0)),
                }
            }
            LinkEvent::SharedPosted { rev } => {
                if let Some(out) = t.shared_in_flight.take() {
                    for id in out.items {
                        if self.project.item(id).is_some() {
                            t.known_shared.insert(id);
                        } else {
                            t.known_shared.remove(&id);
                        }
                    }
                }
                t.shared_rev = t.shared_rev.max(rev);
            }
            LinkEvent::SharedPostFailed { error } => {
                let out = t.shared_in_flight.take().unwrap_or_default();
                match error {
                    LinkError::Stale { rev } => {
                        // the bins are replaced whole: merge with the Drive's first, then resend
                        t.shared_wait_rev = Some(rev);
                        t.shared_pending.absorb(out);
                        t.link.refresh();
                    }
                    LinkError::ItemInUse { item } => {
                        let mut out = out;
                        out.items.remove(&item);
                        t.shared_pending.absorb(out);
                        t.link.refresh();
                        let name = self.base_name(item);
                        tell(self, format!("“{name}” is still used by a sequence on the Drive, so it was not deleted there"));
                    }
                    LinkError::UpdateRequired | LinkError::FilesChanged => {
                        t.shared_pending.absorb(out);
                        self.sync_stopped(&error);
                    }
                    LinkError::Network(_) | LinkError::LockLost | LinkError::HeldBy { .. } => {
                        t.shared_pending.absorb(out);
                        t.shared_retry_at = Some(now + RETRY_AFTER);
                    }
                }
            }
            LinkEvent::SequenceUpdated { seq, rev, by, json } => {
                if let Err(e) = self.apply_remote_sequence(seq, rev, &json) {
                    tell(self, format!("Could not apply {by}’s changes to sequence {}: {e}", seq.0));
                }
            }
            LinkEvent::SharedUpdated { rev, shared } => {
                if let Err(e) = self.apply_remote_shared(rev, &shared) {
                    tell(self, format!("Could not apply the team’s project changes: {e}"));
                }
            }
            LinkEvent::LockChanged { seq, holder: Some(h) } => {
                if h != t.me {
                    t.holders.insert(seq, h);
                    if t.held.contains_key(&seq) {
                        self.lock_lost(seq);
                    }
                }
            }
            LinkEvent::LockChanged { seq, holder: None } => {
                t.holders.remove(&seq);
            }
        }
    }

    fn base_name(&self, id: ItemId) -> String {
        self.team.as_ref().and_then(|t| t.base.item(id)).or_else(|| self.project.item(id)).map(|i| i.name.clone()).unwrap_or_else(|| format!("item {}", id.0))
    }

    fn sync_stopped(&mut self, error: &LinkError) {
        let message = match error {
            LinkError::UpdateRequired => "This team project needs a newer InFocus Cut: update the app to keep syncing".to_string(),
            _ => "The project's files on the Drive were changed outside InFocus Cut, so syncing stopped".to_string(),
        };
        if let Some(t) = self.team.as_mut() {
            t.problem = Some(message.clone());
        }
        tell(self, message);
    }
}
