//! Team projects (InFocus Cut): sequence locks, pushes and teammates' updates.
//!
//! The network stays out of the engine. The app gives the session a [`TeamLink`] whose methods
//! only queue requests (a background thread talks to the Drive) and whose [`TeamLink::poll`]
//! hands back what came of them, plus the teammates' updates from the change feed. The session
//! drives it from the UI thread: [`after_command`] runs after every outermost command and
//! [`Session::poll_team`] once per frame (from [`Session::poll_persistence`]).
//!
//! - An edit that changes a sequence this member does not hold the lock for is taken back whole
//!   (project and undo history as before) and fails with a reason that names the editor.
//! - Edits of held sequences are pushed 1 s after the last one; shared items, bins and
//!   project-level fields (settings, colours, LUTs, transcripts, search bins, source graphics,
//!   narrations, notes) at once.
//! - Deleting a held sequence deletes it on the Drive (after the pushes that stop other sequences
//!   nesting it); a teammate's delete removes it here.
//! - Teammates' updates are written into the project and into every undo/redo snapshot, so undo
//!   never takes back a teammate's work.
//! - A lost lock keeps unsynced edits as a new sequence "Recovered – <name>".
//!
//! A session without a team (`Session::team` is None) never reaches any of this.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use filmcraft_project::{Bin, BinEntry, BinId, ItemId, MediaRef, Project, ProjectItem};
use filmcraft_team::{ChangeSet, LocalMedia, TeamDoc};
use serde_json::Value;
use web_time::Instant;

use crate::{EngineError, Event, History, Result, Session, panels};

mod link_events;

pub use link_events::{LinkError, LinkEvent};

/// How often a held lock is renewed.
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(20);
/// A lock without a successful heartbeat for this long is gone (the server's expiry).
pub const LOCK_EXPIRY: Duration = Duration::from_secs(120);
/// A held sequence is pushed this long after its last edit.
pub const PUSH_DEBOUNCE: Duration = Duration::from_secs(1);
/// A push that failed on the network is tried again after this long.
pub const RETRY_AFTER: Duration = Duration::from_secs(5);

/// Changes to the shared part: items merged by id, deletions, the whole bin tree, and
/// project-level fields replaced whole (by their JSON key, [`filmcraft_team::SHARED_FIELDS`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SharedPush {
    pub base_rev: u64,
    pub upserts: BTreeMap<ItemId, Value>,
    pub deletes: Vec<ItemId>,
    pub bins: Option<Value>,
    pub fields: BTreeMap<String, Value>,
}

/// The app's connection to the Drive. Every request method returns at once; outcomes arrive
/// later through [`TeamLink::poll`]. The lock token stays inside the link.
pub trait TeamLink: Send {
    fn take_lock(&mut self, seq: ItemId);
    fn heartbeat(&mut self, seq: ItemId);
    fn release(&mut self, seq: ItemId);
    /// Save a held sequence; `base_rev` is the revision it was edited from.
    fn put_sequence(&mut self, seq: ItemId, base_rev: u64, json: Value);
    /// Delete a held sequence on the Drive (its lock goes with it); `base_rev` is its revision.
    fn delete_sequence(&mut self, seq: ItemId, base_rev: u64);
    fn post_shared(&mut self, push: SharedPush);
    /// Fetch the whole project again and report it as `SequenceUpdated` / `SharedUpdated`.
    fn refresh(&mut self);
    /// Outcomes of requests and teammates' updates since the last call.
    fn poll(&mut self) -> Vec<LinkEvent>;
    /// The `drive:/` path of a local media file (None: not on the Drive).
    fn media_ref(&self, _media: &MediaRef) -> Option<String> {
        None
    }
    /// This Mac's copies of a `drive:/` media file.
    fn resolve(&self, _drive_path: &str) -> LocalMedia {
        LocalMedia::default()
    }
}

/// A lock this member holds.
#[derive(Clone, Copy, Debug)]
pub struct LockInfo {
    /// The last successful heartbeat (or when the lock was taken).
    pub last_ok: Instant,
    /// The last heartbeat sent.
    pub last_sent: Instant,
}

/// What a sequence tab shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SequenceStatus {
    /// This member holds the lock; `unsynced`: edits not saved on the Drive yet.
    Editing {
        unsynced: bool,
    },
    /// The lock was asked for.
    Requested,
    ReadOnly {
        holder: String,
    },
    Free,
}

/// Item ids whose shared changes still have to be sent, whether the bins changed, and which
/// project-level fields did.
#[derive(Clone, Debug, Default)]
struct SharedOut {
    items: BTreeSet<ItemId>,
    bins: bool,
    fields: BTreeSet<String>,
}

impl SharedOut {
    fn absorb(&mut self, other: SharedOut) {
        self.items.extend(other.items);
        self.bins |= other.bins;
        self.fields.extend(other.fields);
    }
    fn is_empty(&self) -> bool {
        self.items.is_empty() && !self.bins && self.fields.is_empty()
    }
}

/// A held sequence deleted on this Mac that the Drive still has: when to send the delete, and the
/// item and its bin, to put it back if the Drive refuses.
#[derive(Clone, Debug)]
struct PendingDelete {
    due: Instant,
    item: ProjectItem,
    bin: Option<BinId>,
}

/// The team side of a session.
pub struct TeamState {
    /// This member's Drive user name (lock holder name).
    pub me: String,
    /// This member's id slot in the project.
    pub slot: u16,
    pub held: BTreeMap<ItemId, LockInfo>,
    /// Locks other members hold.
    pub holders: BTreeMap<ItemId, String>,
    /// The server revision each sequence was last synced at.
    pub revs: BTreeMap<ItemId, u64>,
    pub shared_rev: u64,
    /// Syncing stopped (old app, project files changed on the Drive): the message to show.
    pub problem: Option<String>,
    requested: BTreeSet<ItemId>,
    /// Sequences made on this Mac that the server does not have yet.
    local_new: BTreeSet<ItemId>,
    /// Held sequences with edits to push, and when.
    dirty: BTreeMap<ItemId, Instant>,
    in_flight: BTreeSet<ItemId>,
    releasing: BTreeSet<ItemId>,
    /// Held sequences deleted here whose delete has not been confirmed by the Drive.
    deleting: BTreeMap<ItemId, PendingDelete>,
    delete_in_flight: BTreeSet<ItemId>,
    /// Sequences deleted on the Drive. `revs` keeps their last revision: the Drive keeps counting,
    /// so one put back (undo) is saved from there.
    gone: BTreeSet<ItemId>,
    /// Shared items the server has.
    known_shared: BTreeSet<ItemId>,
    shared_pending: SharedOut,
    shared_in_flight: Option<SharedOut>,
    shared_retry_at: Option<Instant>,
    /// A shared post was stale: wait for the Drive's shared part at this revision first.
    shared_wait_rev: Option<u64>,
    /// The project as last accounted for (pushed, queued or received).
    base: Arc<Project>,
    epoch: u64,
    told_unshared: bool,
    link: Box<dyn TeamLink>,
}

impl TeamState {
    /// The state for a project just opened from the Drive: its sequences' revisions, the shared
    /// revision and the locks others hold.
    pub fn new(link: Box<dyn TeamLink>, me: String, slot: u16, revs: BTreeMap<ItemId, u64>, shared_rev: u64, holders: BTreeMap<ItemId, String>) -> Self {
        Self {
            me,
            slot,
            held: BTreeMap::new(),
            holders,
            revs,
            shared_rev,
            problem: None,
            requested: BTreeSet::new(),
            local_new: BTreeSet::new(),
            dirty: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            releasing: BTreeSet::new(),
            deleting: BTreeMap::new(),
            delete_in_flight: BTreeSet::new(),
            gone: BTreeSet::new(),
            known_shared: BTreeSet::new(),
            shared_pending: SharedOut::default(),
            shared_in_flight: None,
            shared_retry_at: None,
            shared_wait_rev: None,
            base: Arc::new(Project::new("")),
            epoch: 0,
            told_unshared: false,
            link,
        }
    }

    /// Edits of `seq` that are not saved on the Drive yet.
    pub fn unsynced(&self, seq: ItemId) -> bool {
        self.dirty.contains_key(&seq) || self.in_flight.contains(&seq) || self.local_new.contains(&seq)
    }

    pub fn status(&self, seq: ItemId) -> SequenceStatus {
        if self.held.contains_key(&seq) || self.local_new.contains(&seq) {
            SequenceStatus::Editing { unsynced: self.unsynced(seq) }
        } else if let Some(h) = self.holders.get(&seq) {
            SequenceStatus::ReadOnly { holder: h.clone() }
        } else if self.requested.contains(&seq) {
            SequenceStatus::Requested
        } else {
            SequenceStatus::Free
        }
    }

    fn may_edit(&self, seq: ItemId) -> bool {
        self.held.contains_key(&seq) || self.local_new.contains(&seq)
    }

    fn known_sequence(&self, seq: ItemId) -> bool {
        (self.revs.contains_key(&seq) || self.holders.contains_key(&seq)) && !self.gone.contains(&seq) && !self.deleting.contains_key(&seq)
    }
}

/// The session as it was before an outermost command, kept to take the command back.
pub(crate) struct Before {
    project: Arc<Project>,
    history: History,
    clean: bool,
}

/// Called before an outermost command: accounts for edits made outside `execute` and keeps
/// what is needed to take the command back. None without a team.
pub(crate) fn before_command(s: &mut Session) -> Option<Before> {
    s.team.as_ref()?;
    catch_up(s, Instant::now());
    Some(Before { project: s.project.clone(), history: s.history.clone(), clean: !s.is_dirty() })
}

/// Called after an outermost command: an edit of a sequence this member may not edit is taken
/// back whole (`Disabled(command, reason)`); other changes are queued for the Drive.
pub(crate) fn after_command(s: &mut Session, id: &str, before: Before) -> Result<()> {
    if !still_attached(s) || Arc::ptr_eq(&before.project, &s.project) {
        return Ok(());
    }
    let Some(team) = s.team.as_ref() else { return Ok(()) };
    let changes = filmcraft_team::classify(&before.project, &s.project);
    let plan = check(team, &before.project, &s.project, &changes);
    if let Some((_, why)) = plan.refused.first() {
        let why = why.clone();
        s.project = before.project;
        s.history = before.history;
        s.fix_state();
        s.bump();
        if before.clean {
            s.saved_revision = s.revision;
        }
        if let Some(t) = s.team.as_mut() {
            t.base = s.project.clone();
        }
        return Err(EngineError::Disabled(id.to_string(), why));
    }
    let base = before.project;
    record(s, &base, &changes, &plan, Instant::now());
    Ok(())
}

/// What a change needs: refusals (with the reason), and sequences made or dropped on this Mac.
#[derive(Default)]
struct Plan {
    refused: Vec<(Refusal, String)>,
    new_seqs: BTreeSet<ItemId>,
    dropped: BTreeSet<ItemId>,
    /// Held sequences deleted here: deleted on the Drive too.
    deleted: BTreeSet<ItemId>,
}

#[derive(Clone, Copy)]
enum Refusal {
    Sequence(ItemId),
    Delete(ItemId),
}

fn check(team: &TeamState, before: &Project, after: &Project, changes: &ChangeSet) -> Plan {
    let mut plan = Plan::default();
    let name = |id: ItemId| after.item(id).or_else(|| before.item(id)).map(|i| i.name.clone()).unwrap_or_default();
    for &seq in &changes.sequences {
        let (was, is) = (before.item(seq).is_some(), after.item(seq).is_some());
        if was && is && !team.may_edit(seq) {
            let why = match team.holders.get(&seq) {
                Some(h) => format!("{h} is editing “{}”", name(seq)),
                None => format!("Take the lock to edit “{}”", name(seq)),
            };
            plan.refused.push((Refusal::Sequence(seq), why));
        } else if !was && is {
            if team.local_new.contains(&seq) || !team.known_sequence(seq) {
                plan.new_seqs.insert(seq);
            } else {
                plan.refused.push((Refusal::Sequence(seq), format!("“{}” is a team sequence: take the lock to edit it", name(seq))));
            }
        } else if was && !is {
            if team.local_new.contains(&seq) {
                plan.dropped.insert(seq);
            } else if !team.held.contains_key(&seq) {
                let why = match team.holders.get(&seq) {
                    Some(h) => format!("{h} is editing “{}”", name(seq)),
                    None => format!("Take the lock to delete “{}”", name(seq)),
                };
                plan.refused.push((Refusal::Sequence(seq), why));
            } else if let Some(q) = after.sequences().find(|q| filmcraft_team::sequence_json(after, q.id).is_some_and(|j| references(&j, seq.0, 0))) {
                plan.refused.push((Refusal::Sequence(seq), format!("“{}” is used in “{}”: remove it from that sequence first", name(seq), q.name)));
            } else {
                plan.deleted.insert(seq);
            }
        }
    }
    for &item in &changes.shared_items {
        if before.item(item).is_none() || after.item(item).is_some() {
            continue;
        }
        if let Some(seq) = after.sequences().find(|q| filmcraft_team::sequence_json(after, q.id).is_some_and(|j| references(&j, item.0, 0))) {
            plan.refused.push((Refusal::Delete(item), format!("“{}” is used in “{}”: remove it from the sequence first", name(item), seq.name)));
        }
    }
    plan
}

/// Whether a sequence part uses `id` (a clip's `item` or a `parent`) at any depth, the same test
/// the Drive makes before it deletes a shared item.
fn references(v: &Value, id: u64, depth: usize) -> bool {
    if depth > 200 {
        return false;
    }
    match v {
        Value::Object(m) => m.iter().any(|(k, c)| ((k == "item" || k == "parent") && c.as_u64() == Some(id)) || references(c, id, depth + 1)),
        Value::Array(a) => a.iter().any(|c| references(c, id, depth + 1)),
        _ => false,
    }
}

/// Queue an accepted change: debounced pushes for sequences; shared items, bins, fields and
/// sequence deletes at once.
fn record(s: &mut Session, before: &Project, changes: &ChangeSet, plan: &Plan, now: Instant) {
    let bins_changed = changes.shared_other && serde_json::to_value(&before.root).ok() != serde_json::to_value(&s.project.root).ok();
    let fields = if changes.shared_other { filmcraft_team::changed_fields(before, &s.project) } else { BTreeSet::new() };
    let renamed = before.name != s.project.name;
    let deletes = !plan.deleted.is_empty() || changes.shared_items.iter().any(|i| before.item(*i).is_some() && s.project.item(*i).is_none());
    let Some(t) = s.team.as_mut() else { return };
    for &seq in &changes.sequences {
        if plan.deleted.contains(&seq) {
            t.dirty.remove(&seq);
            t.requested.remove(&seq);
            t.releasing.remove(&seq);
            if let Some(item) = before.item(seq) {
                t.deleting.insert(seq, PendingDelete { due: now, item: item.clone(), bin: before.root.parent_of(seq) });
            }
            continue;
        }
        if plan.dropped.contains(&seq) {
            t.local_new.remove(&seq);
            t.dirty.remove(&seq);
            t.requested.remove(&seq);
            if t.held.remove(&seq).is_some() {
                t.link.release(seq);
            }
            continue;
        }
        if plan.new_seqs.contains(&seq) {
            put_back_new(t, seq);
        }
        t.dirty.insert(seq, now + PUSH_DEBOUNCE);
    }
    t.shared_pending.items.extend(changes.shared_items.iter().copied());
    t.shared_pending.bins |= bins_changed;
    t.shared_pending.fields.extend(fields);
    t.base = s.project.clone();
    let tell = renamed && !t.told_unshared;
    t.told_unshared |= tell;
    if deletes {
        // the Drive refuses to delete an item or sequence a stored sequence still uses: save
        // sequences first
        for due in t.dirty.values_mut() {
            *due = now;
        }
        send_sequences(s, now);
    }
    send_shared(s, now);
    send_deletes(s, now);
    if tell {
        s.log.push(panels::Level::Info, "team", "The project name is kept on this Mac only: a team project is named after its folder on the Drive");
    }
}

/// A sequence that appeared here (new, or a deleted one back through undo): it is this member's
/// to save. A delete not sent yet is simply dropped (the lock is still held).
fn put_back_new(t: &mut TeamState, seq: ItemId) {
    if t.deleting.contains_key(&seq) && !t.delete_in_flight.contains(&seq) {
        t.deleting.remove(&seq);
        return;
    }
    t.gone.remove(&seq);
    if t.local_new.insert(seq) && !t.delete_in_flight.contains(&seq) {
        // (with a delete in flight, the lock is asked for once the Drive answers it)
        t.requested.insert(seq);
        t.link.take_lock(seq);
    }
}

/// Edits made outside `execute` (direct `Session::edit` calls, History panel undo): accept them
/// or take back the parts this member may not change.
fn catch_up(s: &mut Session, now: Instant) {
    let Some(t) = s.team.as_ref() else { return };
    if Arc::ptr_eq(&t.base, &s.project) {
        return;
    }
    let base = t.base.clone();
    let changes = filmcraft_team::classify(&base, &s.project);
    let plan = check(t, &base, &s.project, &changes);
    if plan.refused.is_empty() {
        record(s, &base, &changes, &plan, now);
        return;
    }
    let mut p = (*s.project).clone();
    for (r, _) in &plan.refused {
        let id = match r {
            Refusal::Sequence(id) | Refusal::Delete(id) => *id,
        };
        match base.item(id) {
            Some(it) => {
                p.items.insert(id, it.clone());
            }
            None => {
                p.items.remove(&id);
            }
        }
    }
    let why = plan.refused.first().map(|(_, w)| w.clone()).unwrap_or_default();
    s.project = Arc::new(p);
    s.fix_state();
    s.bump();
    tell(s, why);
    let changes = filmcraft_team::classify(&base, &s.project);
    let plan = s.team.as_ref().map(|t| check(t, &base, &s.project, &changes)).unwrap_or_default();
    record(s, &base, &changes, &plan, now);
}

fn tell(s: &mut Session, message: String) {
    s.log.push(panels::Level::Warning, "team", message.clone());
    s.events.push(Event::Toast { message, error: true });
}

/// False (and the team is dropped, its locks released) once another document replaced the
/// team project.
fn still_attached(s: &mut Session) -> bool {
    let Some(t) = s.team.as_ref() else { return false };
    if t.epoch == s.document_epoch {
        return true;
    }
    s.detach_team();
    false
}

/// Push held sequences whose debounce has passed.
fn send_sequences(s: &mut Session, now: Instant) {
    let Some(t) = s.team.as_mut() else { return };
    if t.problem.is_some() {
        return;
    }
    let due: Vec<ItemId> = t.dirty.iter().filter(|(id, at)| **at <= now && t.held.contains_key(id) && !t.in_flight.contains(id)).map(|(id, _)| *id).collect();
    for id in due {
        t.dirty.remove(&id);
        let Some(json) = filmcraft_team::sequence_json(&s.project, id) else { continue };
        t.in_flight.insert(id);
        let base = t.revs.get(&id).copied().unwrap_or(0);
        t.link.put_sequence(id, base, json);
    }
}

/// Send due sequence deletes once no sequence push is waiting or on its way (a push may be what
/// stops another sequence nesting the deleted one, and the Drive checks that).
fn send_deletes(s: &mut Session, now: Instant) {
    let Some(t) = s.team.as_mut() else { return };
    if t.problem.is_some() || !t.in_flight.is_empty() || t.dirty.keys().any(|id| t.held.contains_key(id)) {
        return;
    }
    let due: Vec<ItemId> =
        t.deleting.iter().filter(|(id, d)| d.due <= now && !t.delete_in_flight.contains(id) && t.held.contains_key(id)).map(|(id, _)| *id).collect();
    for id in due {
        t.delete_in_flight.insert(id);
        let base = t.revs.get(&id).copied().unwrap_or(0);
        t.link.delete_sequence(id, base);
    }
}

/// Send queued shared changes (one request at a time; the item JSON is today's).
fn send_shared(s: &mut Session, now: Instant) {
    let Some(t) = s.team.as_mut() else { return };
    if t.problem.is_some()
        || t.shared_in_flight.is_some()
        || t.shared_pending.is_empty()
        || t.shared_retry_at.is_some_and(|at| at > now)
        || t.shared_wait_rev.is_some_and(|r| r > t.shared_rev)
    {
        return;
    }
    let out = std::mem::take(&mut t.shared_pending);
    let mut mini = Project::new("");
    let mut deletes = Vec::new();
    for &id in &out.items {
        match s.project.item(id) {
            Some(it) if it.as_sequence().is_none() => {
                mini.items.insert(id, it.clone());
            }
            Some(_) => {}
            None if t.known_shared.contains(&id) => deletes.push(id),
            None => {}
        }
    }
    let link = &t.link;
    let upserts = match filmcraft_team::split(&mini, &|m| link.media_ref(m)) {
        Ok(doc) => doc
            .shared
            .get("items")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter_map(|(k, v)| Some((ItemId(k.parse().ok()?), v.clone()))).collect())
            .unwrap_or_default(),
        Err(e) => {
            let message = format!("Could not prepare shared changes for the Drive: {e}");
            t.shared_pending.absorb(out);
            t.shared_retry_at = Some(now + RETRY_AFTER);
            tell(s, message);
            return;
        }
    };
    let fields = match filmcraft_team::field_values(&s.project, &out.fields) {
        Ok(f) => f,
        Err(e) => {
            let message = format!("Could not prepare project settings for the Drive: {e}");
            t.shared_pending.absorb(out);
            t.shared_retry_at = Some(now + RETRY_AFTER);
            tell(s, message);
            return;
        }
    };
    let bins = if out.bins { serde_json::to_value(&s.project.root).ok() } else { None };
    let push = SharedPush { base_rev: t.shared_rev, upserts, deletes, bins, fields };
    if push.upserts.is_empty() && push.deletes.is_empty() && push.bins.is_none() && push.fields.is_empty() {
        return;
    }
    t.shared_in_flight = Some(out);
    t.link.post_shared(push);
}

impl Session {
    /// Make this session a team session. The project's ids come from `team.slot` from now on;
    /// the undo history is cleared (its snapshots allocate ids from the old slot).
    pub fn attach_team(&mut self, mut team: TeamState) -> Result<()> {
        let mut p = (*self.project).clone();
        p.set_id_slot(team.slot).map_err(|e| EngineError::Other(e.to_string()))?;
        self.project = Arc::new(p);
        self.history.clear();
        team.base = self.project.clone();
        team.epoch = self.document_epoch;
        team.known_shared = self.project.items.values().filter(|i| i.as_sequence().is_none()).map(|i| i.id).collect();
        self.team = Some(team);
        Ok(())
    }

    /// Leave team mode: held locks are released (unsynced edits stay in this project only).
    pub fn detach_team(&mut self) {
        if let Some(mut t) = self.team.take() {
            let held: Vec<ItemId> = t.held.keys().copied().collect();
            for id in held {
                t.link.release(id);
            }
        }
    }

    /// The sequence tab state of `seq` (None outside a team project).
    pub fn team_status(&self, seq: ItemId) -> Option<SequenceStatus> {
        self.team.as_ref().map(|t| t.status(seq))
    }

    /// Ask for the lock on `seq` (the tab's Edit button).
    pub fn team_take_lock(&mut self, seq: ItemId) -> Result<()> {
        if self.project.sequence(seq).is_none() {
            return Err(EngineError::Other(format!("item {} is not a sequence", seq.0)));
        }
        let t = self.team.as_mut().ok_or_else(not_team)?;
        if !t.may_edit(seq) && t.requested.insert(seq) {
            t.link.take_lock(seq);
        }
        Ok(())
    }

    /// Give up the lock on `seq` once its edits are saved on the Drive.
    pub fn team_release_lock(&mut self, seq: ItemId) -> Result<()> {
        let t = self.team.as_mut().ok_or_else(not_team)?;
        if t.held.contains_key(&seq) {
            if let Some(due) = t.dirty.get_mut(&seq) {
                *due = Instant::now();
            }
            t.releasing.insert(seq);
        }
        Ok(())
    }

    /// A teammate's version of sequence `id` at revision `rev`: written into the project and every
    /// undo/redo snapshot, without an undo step. Ignored for a sequence this member edits and for
    /// an older revision than the one already here.
    pub fn apply_remote_sequence(&mut self, id: ItemId, rev: u64, json: &Value) -> Result<()> {
        let t = self.team.as_ref().ok_or_else(not_team)?;
        if t.may_edit(id) || t.revs.get(&id).is_some_and(|r| *r >= rev) {
            return Ok(());
        }
        let mut p = (*self.project).clone();
        filmcraft_team::replace_sequence(&mut p, id, json).map_err(|e| EngineError::Other(e.to_string()))?;
        let item = p.items.get(&id).cloned().ok_or_else(|| EngineError::Other(format!("sequence {} was not added", id.0)))?;
        self.project = Arc::new(p);
        patch_snapshots(&mut self.history, |q| {
            q.items.insert(id, item.clone());
        });
        if let Some(t) = self.team.as_mut() {
            t.revs.insert(id, rev);
        }
        self.remote_applied();
        Ok(())
    }

    /// The shared part at revision `rev` (items, bins and project-level fields) from the Drive:
    /// written into the project and every undo/redo snapshot, without an undo step. Items and
    /// fields with changes still on their way to the Drive keep this Mac's version, and so do the
    /// bins while a bin change is. This Mac's scratch disks and ingest destination stay.
    pub fn apply_remote_shared(&mut self, rev: u64, shared: &Value) -> Result<()> {
        let t = self.team.as_ref().ok_or_else(not_team)?;
        if rev <= t.shared_rev {
            return Ok(());
        }
        let sequences = self.project.sequences().filter_map(|q| Some((q.id, filmcraft_team::sequence_json(&self.project, q.id)?))).collect();
        // `drive:/` paths stay as they are here; `keep_local_media` resolves them per item
        let remote =
            filmcraft_team::merge(&TeamDoc { shared: shared.clone(), sequences }, &|_| LocalMedia::default()).map_err(|e| EngineError::Other(e.to_string()))?;
        let link = &t.link;
        let mut outgoing = t.shared_pending.clone();
        if let Some(f) = &t.shared_in_flight {
            outgoing.absorb(f.clone());
        }
        let remote_ids: BTreeSet<ItemId> = remote.items.values().filter(|i| i.as_sequence().is_none()).map(|i| i.id).collect();
        let mut items = BTreeMap::new();
        for id in remote_ids.iter().filter(|id| !outgoing.items.contains(id)) {
            let Some(it) = remote.item(*id) else { continue };
            items.insert(*id, keep_local_media(&**link, self.project.item(*id), it));
        }
        let gone: Vec<ItemId> = t.known_shared.iter().filter(|id| !remote_ids.contains(id) && !outgoing.items.contains(id)).copied().collect();
        // bins on their way to the Drive: keep this Mac's tree, plus what teammates added
        let root = if outgoing.bins { merge_bins(&self.project.root, &remote.root) } else { remote.root.clone() };
        // fields on their way to the Drive keep this Mac's value (the last writer wins)
        let skip_fields = outgoing.fields;
        let patch = |q: &mut Project| {
            for (id, it) in &items {
                q.items.insert(*id, it.clone());
            }
            for id in &gone {
                if q.item(*id).is_some_and(|i| i.as_sequence().is_none()) {
                    q.items.remove(id);
                }
            }
            q.root = root.clone();
            filmcraft_team::copy_fields(q, &remote, &skip_fields);
        };
        let mut p = (*self.project).clone();
        patch(&mut p);
        self.project = Arc::new(p);
        patch_snapshots(&mut self.history, patch);
        if let Some(t) = self.team.as_mut() {
            t.shared_rev = rev;
            t.known_shared = remote_ids;
        }
        self.remote_applied();
        Ok(())
    }

    /// The lock on `id` is gone (heartbeat failed for too long, or the Drive refused a push).
    /// Unsynced edits are kept as a new sequence "Recovered – <name>" that this member owns, and
    /// `id` turns read-only until the Drive's version arrives.
    pub fn lock_lost(&mut self, id: ItemId) {
        let Some(t) = self.team.as_mut() else { return };
        let had = t.held.remove(&id).is_some();
        t.requested.remove(&id);
        t.releasing.remove(&id);
        if let Some(d) = t.deleting.remove(&id) {
            // deleted here but not on the Drive: it comes back as the Drive has it
            t.delete_in_flight.remove(&id);
            t.revs.remove(&id);
            t.local_new.remove(&id);
            t.link.refresh();
            let name = d.item.name.clone();
            self.put_back(d);
            tell(self, format!("You lost the lock on “{name}” before it was deleted on the Drive, so it is back"));
            return;
        }
        if t.local_new.contains(&id) {
            // never on the Drive: nobody else can have it, so just ask again
            t.requested.insert(id);
            t.link.take_lock(id);
            return;
        }
        let unsynced = t.unsynced(id);
        t.dirty.remove(&id);
        t.in_flight.remove(&id);
        t.revs.remove(&id);
        t.link.refresh();
        let name = self.project.item(id).map(|i| i.name.clone()).unwrap_or_default();
        if !unsynced {
            if had {
                tell(self, format!("You no longer hold the lock on “{name}”; it is read-only now"));
            }
            return;
        }
        match self.save_recovered(id) {
            Some(copy) => tell(self, format!("You lost the lock on “{name}”. Your unsynced edits are saved as “{copy}”")),
            None => tell(self, format!("You lost the lock on “{name}”; its unsynced edits could not be copied")),
        }
    }

    /// A teammate deleted sequence `id` on the Drive at revision `rev`: it leaves the project,
    /// every undo/redo snapshot and the bins, without an undo step. Ignored for this member's
    /// own delete and for an older revision than the one already here.
    pub fn apply_remote_delete(&mut self, id: ItemId, rev: u64) -> Result<()> {
        let t = self.team.as_mut().ok_or_else(not_team)?;
        if t.deleting.contains_key(&id) || t.local_new.contains(&id) || t.revs.get(&id).is_some_and(|r| *r >= rev) {
            return Ok(());
        }
        if self.project.item(id).is_some_and(|i| i.as_sequence().is_none()) {
            return Err(EngineError::Other(format!("item {} is not a sequence", id.0)));
        }
        for set in [&mut t.requested, &mut t.in_flight, &mut t.releasing] {
            set.remove(&id);
        }
        t.held.remove(&id);
        t.holders.remove(&id);
        t.dirty.remove(&id);
        t.revs.insert(id, rev);
        t.gone.insert(id);
        let drop = |q: &mut Project| {
            if q.item(id).is_some_and(|i| i.as_sequence().is_some()) {
                q.items.remove(&id);
                q.root.remove_item(id);
            }
        };
        let mut p = (*self.project).clone();
        drop(&mut p);
        self.project = Arc::new(p);
        patch_snapshots(&mut self.history, drop);
        self.remote_applied();
        Ok(())
    }

    /// Put a sequence whose delete the Drive refused back into the project (where it is missing)
    /// and its bin, without an undo step; the bins go to the Drive again.
    fn put_back(&mut self, d: PendingDelete) {
        let id = d.item.id;
        let restore = |q: &mut Project| {
            if q.item(id).is_none() {
                q.items.insert(id, d.item.clone());
            }
            if q.root.parent_of(id).is_none() && !q.root.children.contains(&BinEntry::Item(id)) {
                match d.bin.and_then(|b| q.root.find_bin_mut(b)) {
                    Some(b) => b.children.push(BinEntry::Item(id)),
                    None => q.root.children.push(BinEntry::Item(id)),
                }
            }
        };
        let mut p = (*self.project).clone();
        restore(&mut p);
        self.project = Arc::new(p);
        patch_snapshots(&mut self.history, restore);
        let now = Instant::now();
        if let Some(t) = self.team.as_mut() {
            t.shared_pending.bins = true;
            t.base = self.project.clone();
        }
        send_shared(self, now);
        self.fix_state();
        self.bump();
        self.previews.live.publish_project(self.project.clone());
    }

    /// Copy sequence `id` as "Recovered – <name>" into the project and every snapshot, owned by
    /// this member and queued for the Drive. Returns the copy's name.
    fn save_recovered(&mut self, id: ItemId) -> Option<String> {
        let item = self.project.item(id)?.clone();
        let mut p = (*self.project).clone();
        let bin = p.root.parent_of(id);
        let name = format!("Recovered – {}", item.name);
        let nid = p.add_item(&name, item.label, item.kind.clone(), bin);
        let copy = p.item(nid)?.clone();
        let next_id = p.next_id;
        self.project = Arc::new(p);
        patch_snapshots(&mut self.history, |q| {
            q.items.insert(nid, copy.clone());
            if q.root.parent_of(nid).is_none() && !q.root.children.contains(&BinEntry::Item(nid)) {
                q.root.children.push(BinEntry::Item(nid));
            }
            q.next_id = q.next_id.max(next_id);
        });
        let now = Instant::now();
        let t = self.team.as_mut()?;
        t.local_new.insert(nid);
        t.requested.insert(nid);
        t.link.take_lock(nid);
        t.dirty.insert(nid, now);
        t.shared_pending.bins = true;
        t.base = self.project.clone();
        send_shared(self, now);
        self.fix_state();
        self.bump();
        Some(name)
    }

    /// A change that stays on this Mac (where it keeps a team media item's original and proxy):
    /// written into the project, every undo/redo snapshot and the team's last-synced copy, so it
    /// is neither an undo step nor sent to the Drive. Works without a team too.
    pub(crate) fn patch_local(&mut self, f: impl Fn(&mut Project)) {
        let synced = self.team.as_ref().is_some_and(|t| Arc::ptr_eq(&t.base, &self.project));
        let mut p = (*self.project).clone();
        f(&mut p);
        self.project = Arc::new(p);
        patch_snapshots(&mut self.history, &f);
        let project = self.project.clone();
        if let Some(t) = self.team.as_mut() {
            t.base = if synced {
                project
            } else {
                let mut b = (*t.base).clone();
                f(&mut b);
                Arc::new(b)
            };
        }
        self.fix_state();
        self.bump();
        self.previews.live.publish_project(self.project.clone());
    }

    fn remote_applied(&mut self) {
        if let Some(t) = self.team.as_mut() {
            t.base = self.project.clone();
        }
        self.fix_state();
        self.bump();
        self.previews.live.publish_project(self.project.clone());
    }
}

fn not_team() -> EngineError {
    EngineError::Other("this is not a team project".into())
}

/// Write `f` into every undo and redo snapshot.
fn patch_snapshots(h: &mut History, f: impl Fn(&mut Project)) {
    for (_, snap, _) in h.undo.iter_mut().chain(h.redo.iter_mut()) {
        let mut q = (**snap).clone();
        f(&mut q);
        *snap = Arc::new(q);
    }
}

/// A teammate's version of a shared item (with its `drive:/` path). This Mac's file path and
/// proxy stay when the item still names the same file; otherwise the local cache fills them in.
fn keep_local_media(link: &dyn TeamLink, local: Option<&ProjectItem>, remote: &ProjectItem) -> ProjectItem {
    let mut out = remote.clone();
    let Some(r) = out.as_media_mut() else { return out };
    if let Some(l) = local.and_then(ProjectItem::as_media) {
        let shared_ref = match &l.media {
            MediaRef::File { path } => MediaRef::File { path: link.media_ref(&l.media).unwrap_or_else(|| file_name(path).to_owned()) },
            other => other.clone(),
        };
        if r.media == shared_ref {
            r.media = l.media.clone();
            r.proxy = l.proxy.clone();
            return out;
        }
    }
    if let MediaRef::File { path } = &r.media
        && path.starts_with(filmcraft_team::DRIVE_PREFIX)
    {
        let found = link.resolve(path);
        if let Some(o) = found.original {
            r.media = MediaRef::File { path: o.to_string_lossy().into_owned() };
        }
        r.proxy = found.proxy.map(|p| MediaRef::File { path: p.to_string_lossy().into_owned() });
    }
    out
}

/// `local` plus the items and bins of `remote` that `local` does not have, each in the bin it is
/// in on the Drive (the top level when this Mac has no such bin).
fn merge_bins(local: &Bin, remote: &Bin) -> Bin {
    let mut out = local.clone();
    let mut have = BTreeSet::new();
    bin_ids(&out, &mut have, 0);
    add_missing(&mut out, remote, &have, 0);
    out
}

const MAX_BIN_DEPTH: usize = 64;

fn bin_ids(b: &Bin, out: &mut BTreeSet<u64>, depth: usize) {
    out.insert(b.id.0);
    if depth > MAX_BIN_DEPTH {
        return;
    }
    for c in &b.children {
        match c {
            BinEntry::Item(id) => {
                out.insert(id.0);
            }
            BinEntry::Bin(sub) => bin_ids(sub, out, depth + 1),
        }
    }
}

fn add_missing(out: &mut Bin, remote: &Bin, have: &BTreeSet<u64>, depth: usize) {
    if depth > MAX_BIN_DEPTH {
        return;
    }
    for c in &remote.children {
        match c {
            BinEntry::Item(id) if !have.contains(&id.0) => place(out, remote.id, c.clone()),
            BinEntry::Bin(sub) if !have.contains(&sub.id.0) => place(out, remote.id, c.clone()),
            BinEntry::Bin(sub) => add_missing(out, sub, have, depth + 1),
            BinEntry::Item(_) => {}
        }
    }
}

fn place(out: &mut Bin, parent: BinId, entry: BinEntry) {
    match out.find_bin_mut(parent) {
        Some(b) => b.children.push(entry),
        None => out.children.push(entry),
    }
}

/// The last component of a path, for either separator.
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}
