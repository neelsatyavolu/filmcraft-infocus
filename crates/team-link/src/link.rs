//! [`DriveTeamLink`]: the engine's `TeamLink` over the Drive client. Each request method queues a
//! command for the worker thread and returns at once; outcomes come back through `poll`. Lock
//! tokens stay on the worker side (shared with the feed thread, which skips fetching sequences
//! this Mac holds).

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_drive::{DRIVE_PREFIX, DriveClient, DriveError, SharedChange};
use filmcraft_engine::team::{LinkError, LinkEvent, SharedPush, TeamLink};
use filmcraft_project::{ItemId, MediaRef};
use filmcraft_team::LocalMedia;
use filmcraft_team::cache::CacheIndex;
use serde_json::Value;

use crate::feed::{self, FeedState};
use crate::uploads::UploadNotes;

/// Lock tokens by sequence id.
pub(crate) type Tokens = Arc<Mutex<BTreeMap<u64, String>>>;

/// How the connection is doing, for the Team panel.
#[derive(Debug, Default)]
pub struct LinkStatus {
    live: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl LinkStatus {
    /// True while the change feed is connected (otherwise the project is polled every 5 s).
    pub fn feed_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }
    /// The last request or feed error, until a later request succeeds.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
    pub(crate) fn set_live(&self, live: bool) {
        self.live.store(live, Ordering::Relaxed);
    }
    pub(crate) fn set_error(&self, error: Option<String>) {
        *self.last_error.lock().unwrap_or_else(PoisonError::into_inner) = error;
    }
}

enum Cmd {
    Take(u64),
    Beat(u64),
    Release(u64),
    Put { seq: u64, base: u64, json: Value },
    Delete { seq: u64, base: u64 },
    Shared(SharedPush),
    Refresh,
}

/// A team session's connection to the Drive. Dropping it stops both threads (the worker after
/// the requests already queued, such as the releases `detach_team` sends).
pub struct DriveTeamLink {
    cmds: Sender<Cmd>,
    events: Receiver<LinkEvent>,
    index: Arc<Mutex<CacheIndex>>,
    status: Arc<LinkStatus>,
    stop: Arc<AtomicBool>,
    notes: Option<Arc<Mutex<UploadNotes>>>,
}

impl DriveTeamLink {
    /// Starts the worker and feed threads for `project`. `known` is what the session was opened
    /// at (so the feed reports only what changed after); `index` mirrors this Mac's media cache
    /// for `media_ref` and `resolve`.
    pub fn start(client: Arc<DriveClient>, project: &str, known: FeedState, index: Arc<Mutex<CacheIndex>>) -> Self {
        let (cmd_tx, cmd_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        let status = Arc::new(LinkStatus::default());
        let stop = Arc::new(AtomicBool::new(false));
        let tokens: Tokens = Arc::default();
        let worker = Worker { client: client.clone(), project: project.to_string(), tokens: tokens.clone(), events: ev_tx.clone(), status: status.clone() };
        std::thread::Builder::new()
            .name("team-link".into())
            .spawn(move || worker.run(cmd_rx))
            .map_err(|e| status.set_error(Some(format!("Could not start the Drive connection: {e}"))))
            .ok();
        let feed = feed::Feed {
            client,
            project: project.to_string(),
            tokens,
            events: ev_tx,
            status: status.clone(),
            stop: stop.clone(),
            known,
            etags: BTreeMap::new(),
        };
        std::thread::Builder::new()
            .name("team-feed".into())
            .spawn(move || feed.run())
            .map_err(|e| status.set_error(Some(format!("Could not start the change feed: {e}"))))
            .ok();
        Self { cmds: cmd_tx, events: ev_rx, index, status, stop, notes: None }
    }

    /// Shares who is uploading what: this member's uploading items are marked in what the link
    /// sends, and teammates' are read from every shared part that arrives.
    pub fn with_upload_notes(mut self, notes: Arc<Mutex<UploadNotes>>) -> Self {
        self.notes = Some(notes);
        self
    }

    /// The connection's status (shared with the threads).
    pub fn status(&self) -> Arc<LinkStatus> {
        self.status.clone()
    }

    fn send(&self, cmd: Cmd) {
        if self.cmds.send(cmd).is_err() {
            self.status.set_error(Some("The Drive connection stopped; open the project again".into()));
        }
    }
}

impl Drop for DriveTeamLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl TeamLink for DriveTeamLink {
    fn take_lock(&mut self, seq: ItemId) {
        self.send(Cmd::Take(seq.0));
    }
    fn heartbeat(&mut self, seq: ItemId) {
        self.send(Cmd::Beat(seq.0));
    }
    fn release(&mut self, seq: ItemId) {
        self.send(Cmd::Release(seq.0));
    }
    fn put_sequence(&mut self, seq: ItemId, base_rev: u64, json: Value) {
        self.send(Cmd::Put { seq: seq.0, base: base_rev, json });
    }
    fn delete_sequence(&mut self, seq: ItemId, base_rev: u64) {
        self.send(Cmd::Delete { seq: seq.0, base: base_rev });
    }
    fn post_shared(&mut self, mut push: SharedPush) {
        if let Some(notes) = &self.notes {
            notes.lock().unwrap_or_else(PoisonError::into_inner).mark(&mut push.upserts);
        }
        self.send(Cmd::Shared(push));
    }
    fn refresh(&mut self) {
        self.send(Cmd::Refresh);
    }
    fn poll(&mut self) -> Vec<LinkEvent> {
        let events: Vec<LinkEvent> = self.events.try_iter().collect();
        if let Some(notes) = &self.notes {
            for ev in &events {
                if let LinkEvent::SharedUpdated { shared, .. } = ev {
                    notes.lock().unwrap_or_else(PoisonError::into_inner).read_shared(shared);
                }
            }
        }
        events
    }
    fn media_ref(&self, media: &MediaRef) -> Option<String> {
        let MediaRef::File { path } = media else { return None };
        if path.starts_with(DRIVE_PREFIX) {
            return Some(path.clone());
        }
        self.index.lock().unwrap_or_else(PoisonError::into_inner).drive_ref_of(Path::new(path)).map(str::to_string)
    }
    fn resolve(&self, drive_path: &str) -> LocalMedia {
        self.index.lock().unwrap_or_else(PoisonError::into_inner).resolve(drive_path)
    }
}

/// The engine's view of a Drive error. Refusals the session reacts to keep their meaning;
/// everything else is retried as a network problem.
pub fn link_error(e: &DriveError) -> LinkError {
    match e {
        DriveError::HeldBy { holder, device, .. } => LinkError::HeldBy { holder: holder_name(holder.as_deref(), device.as_deref()) },
        DriveError::LockLost => LinkError::LockLost,
        DriveError::Stale { rev } => LinkError::Stale { rev: *rev },
        DriveError::ItemInUse { item } => LinkError::ItemInUse { item: ItemId(*item) },
        DriveError::FilesChanged(_) => LinkError::FilesChanged,
        DriveError::UpdateRequired { .. } => LinkError::UpdateRequired,
        other => LinkError::Network(other.to_string()),
    }
}

/// The name a lock refusal shows: the teammate, or this member on another Mac.
fn holder_name(holder: Option<&str>, device: Option<&str>) -> String {
    match (device, holder) {
        (Some(d), _) => filmcraft_engine::team::own_device_holder(d),
        (None, Some(h)) if !h.is_empty() => h.to_string(),
        _ => "Someone".to_string(),
    }
}

struct Worker {
    client: Arc<DriveClient>,
    project: String,
    tokens: Tokens,
    events: Sender<LinkEvent>,
    status: Arc<LinkStatus>,
}

impl Worker {
    /// Runs commands in order until the link is dropped and the queue is empty.
    fn run(self, cmds: Receiver<Cmd>) {
        while let Ok(cmd) = cmds.recv() {
            if catch_unwind(AssertUnwindSafe(|| self.handle(cmd))).is_err() {
                self.status.set_error(Some("A Drive request failed unexpectedly and was dropped".into()));
            }
        }
    }

    fn token(&self, seq: u64) -> Option<String> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner).get(&seq).cloned()
    }

    fn set_token(&self, seq: u64, token: Option<String>) {
        let mut t = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        match token {
            Some(tok) => t.insert(seq, tok),
            None => t.remove(&seq),
        };
    }

    fn emit(&self, ev: LinkEvent) {
        // the receiver is gone only when the session dropped the link: nothing to tell
        let _ = self.events.send(ev);
    }

    /// Records how a request went for the status line.
    fn outcome<T>(&self, r: &Result<T, DriveError>) {
        match r {
            Ok(_) => self.status.set_error(None),
            Err(e) => self.status.set_error(Some(e.to_string())),
        }
    }

    fn handle(&self, cmd: Cmd) {
        let (c, p) = (&self.client, self.project.as_str());
        match cmd {
            Cmd::Take(seq) => {
                let r = c.take_lock(p, seq);
                self.outcome(&r);
                let id = ItemId(seq);
                match r {
                    Ok(grant) => {
                        self.set_token(seq, Some(grant.token));
                        self.emit(LinkEvent::LockTaken { seq: id });
                    }
                    Err(e @ DriveError::HeldBy { .. }) => {
                        let LinkError::HeldBy { holder } = link_error(&e) else { return };
                        self.emit(LinkEvent::LockRefused { seq: id, holder });
                    }
                    Err(e) => self.emit(LinkEvent::LockFailed { seq: id, message: e.to_string() }),
                }
            }
            Cmd::Beat(seq) => {
                let id = ItemId(seq);
                let Some(token) = self.token(seq) else {
                    return self.emit(LinkEvent::HeartbeatFailed { seq: id, error: LinkError::LockLost });
                };
                let r = c.heartbeat(p, seq, &token);
                self.outcome(&r);
                match r {
                    Ok(_) => self.emit(LinkEvent::HeartbeatOk { seq: id }),
                    Err(e) => self.failed_with_lock(seq, &e, |error| LinkEvent::HeartbeatFailed { seq: id, error }),
                }
            }
            Cmd::Release(seq) => {
                if let Some(token) = self.token(seq) {
                    self.set_token(seq, None);
                    // a release that fails leaves a lock that expires in 120 s
                    let r = c.release(p, seq, &token);
                    self.outcome(&r);
                }
            }
            Cmd::Put { seq, base, json } => {
                let id = ItemId(seq);
                let Some(token) = self.token(seq) else {
                    return self.emit(LinkEvent::SequencePutFailed { seq: id, error: LinkError::LockLost });
                };
                let r = c.put_sequence(p, seq, &token, base, &json);
                self.outcome(&r);
                match r {
                    Ok(rev) => self.emit(LinkEvent::SequencePut { seq: id, rev }),
                    Err(e) => self.failed_with_lock(seq, &e, |error| LinkEvent::SequencePutFailed { seq: id, error }),
                }
            }
            Cmd::Delete { seq, base } => {
                let id = ItemId(seq);
                let Some(token) = self.token(seq) else {
                    return self.emit(LinkEvent::SequenceDeleteFailed { seq: id, error: LinkError::LockLost });
                };
                let r = c.delete_sequence(p, seq, &token, base);
                self.outcome(&r);
                match r {
                    Ok(rev) => {
                        self.set_token(seq, None);
                        self.emit(LinkEvent::SequenceDeleted { seq: id, rev });
                    }
                    Err(e) => self.failed_with_lock(seq, &e, |error| LinkEvent::SequenceDeleteFailed { seq: id, error }),
                }
            }
            Cmd::Shared(push) => {
                let change = SharedChange {
                    base_rev: push.base_rev,
                    upserts: push.upserts.into_iter().map(|(id, v)| (id.0, v)).collect(),
                    deletes: push.deletes.into_iter().map(|id| id.0).collect(),
                    bins: push.bins,
                    fields: push.fields,
                };
                let r = c.post_shared(p, &change);
                self.outcome(&r);
                match r {
                    Ok(rev) => self.emit(LinkEvent::SharedPosted { rev }),
                    Err(e) => self.emit(LinkEvent::SharedPostFailed { error: link_error(&e) }),
                }
            }
            Cmd::Refresh => {
                let r = c.get_project(p);
                self.outcome(&r);
                if let Ok(data) = r {
                    let held: Vec<u64> = self.tokens.lock().unwrap_or_else(PoisonError::into_inner).keys().copied().collect();
                    for ev in feed::whole_project(&data, &held) {
                        self.emit(ev);
                    }
                }
            }
        }
    }

    /// A failed request under a lock: a lock that is gone forgets its token.
    fn failed_with_lock(&self, seq: u64, e: &DriveError, event: impl FnOnce(LinkError) -> LinkEvent) {
        let error = link_error(e);
        if matches!(error, LinkError::LockLost | LinkError::HeldBy { .. }) {
            self.set_token(seq, None);
        }
        self.emit(event(error));
    }
}
