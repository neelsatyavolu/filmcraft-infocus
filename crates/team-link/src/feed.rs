//! The change feed thread: follows `GET …/events`, fetches the project when a teammate saved
//! something, and turns what changed into the engine's `LinkEvent`s. When the stream ends or
//! cannot be opened it reconnects with backoff (1 s doubling to 60 s) and polls the project every
//! 5 s in between, so updates still arrive while the feed is down.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use filmcraft_drive::{DriveClient, ProjectData, TeamEvent};
use filmcraft_engine::team::LinkEvent;
use filmcraft_project::ItemId;

use crate::link::{LinkStatus, Tokens};

/// How often the project is polled while the feed is down.
pub const POLL_EVERY: Duration = Duration::from_secs(5);
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A stream that stayed up this long resets the backoff (one that keeps dropping at once does not).
const STABLE: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(100);

/// What this Mac last saw of the project: sequence revisions, the shared revision and the locks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FeedState {
    pub sequences: BTreeMap<u64, u64>,
    pub shared_rev: u64,
    pub locks: BTreeMap<u64, String>,
}

impl FeedState {
    /// The state of a project as fetched.
    pub fn of(data: &ProjectData) -> Self {
        Self {
            sequences: data.sequences.iter().map(|(id, part)| (*id, part.rev)).collect(),
            shared_rev: data.shared_rev,
            locks: data.locks.iter().map(|(id, l)| (*id, l.holder.clone())).collect(),
        }
    }

    /// The events that take this state to `data` (and the state becomes `data`'s). `by` names
    /// who saved a sequence when an event said so. Sequences in `held` are this Mac's: their
    /// revision is noted but they are not reported.
    pub fn advance(&mut self, data: &ProjectData, by: Option<(u64, &str)>, held: &[u64]) -> Vec<LinkEvent> {
        let mut out = Vec::new();
        for (id, part) in &data.sequences {
            let newer = self.sequences.get(id).is_none_or(|r| *r < part.rev);
            self.sequences.insert(*id, part.rev);
            if newer && !held.contains(id) {
                let by = by.filter(|(s, _)| s == id).map(|(_, b)| b.to_string()).unwrap_or_default();
                out.push(LinkEvent::SequenceUpdated { seq: ItemId(*id), rev: part.rev, by, json: part.json.clone() });
            }
        }
        let gone: Vec<(u64, u64)> = self.sequences.iter().filter(|(id, _)| !data.sequences.contains_key(id)).map(|(id, r)| (*id, *r)).collect();
        for (id, rev) in gone {
            self.sequences.remove(&id);
            // a delete bumps the revision once
            out.push(LinkEvent::SequenceRemoved { seq: ItemId(id), rev: rev.saturating_add(1), by: String::new() });
        }
        if data.shared_rev > self.shared_rev {
            self.shared_rev = data.shared_rev;
            out.push(LinkEvent::SharedUpdated { rev: data.shared_rev, shared: data.shared.clone() });
        }
        let locks: BTreeMap<u64, String> = data.locks.iter().map(|(id, l)| (*id, l.holder.clone())).collect();
        for id in self.locks.keys().chain(locks.keys()).copied().collect::<std::collections::BTreeSet<u64>>() {
            let (was, is) = (self.locks.get(&id), locks.get(&id));
            if was != is {
                out.push(LinkEvent::LockChanged { seq: ItemId(id), holder: is.cloned() });
            }
        }
        self.locks = locks;
        out
    }
}

/// Every part of a fetched project, for the session's `refresh` (it ignores what it already has).
pub(crate) fn whole_project(data: &ProjectData, held: &[u64]) -> Vec<LinkEvent> {
    FeedState::default().advance(data, None, held)
}

pub(crate) struct Feed {
    pub client: Arc<DriveClient>,
    pub project: String,
    pub tokens: Tokens,
    pub events: Sender<LinkEvent>,
    pub status: Arc<LinkStatus>,
    pub stop: Arc<AtomicBool>,
    pub known: FeedState,
}

impl Feed {
    pub fn run(mut self) {
        let mut backoff = FIRST_BACKOFF;
        while !self.stopped() {
            let started = Instant::now();
            if catch_unwind(AssertUnwindSafe(|| self.follow())).unwrap_or(false) && started.elapsed() >= STABLE {
                backoff = FIRST_BACKOFF;
            }
            self.status.set_live(false);
            if self.stopped() {
                break;
            }
            self.poll_until(Instant::now() + backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn held(&self) -> Vec<u64> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner).keys().copied().collect()
    }

    /// Follows the stream until it ends. True when it connected.
    fn follow(&mut self) -> bool {
        let stream = match self.client.events(&self.project) {
            Ok(s) => s,
            Err(e) => {
                self.status.set_error(Some(format!("Live updates: {e}")));
                return false;
            }
        };
        self.status.set_live(true);
        // catch up on what changed while the feed was down
        self.fetch(None);
        for ev in stream {
            if self.stopped() {
                break;
            }
            match ev {
                Ok(ev) => self.on_event(ev),
                Err(e) => {
                    self.status.set_error(Some(format!("Live updates: {e}")));
                    break;
                }
            }
        }
        true
    }

    fn on_event(&mut self, ev: TeamEvent) {
        match ev {
            TeamEvent::SequenceUpdated { seq, rev, by } => {
                let ours = self.held().contains(&seq);
                if ours || self.known.sequences.get(&seq).is_some_and(|r| *r >= rev) {
                    self.known.sequences.insert(seq, rev);
                } else {
                    self.fetch(Some((seq, by.as_str())));
                }
            }
            TeamEvent::SequenceDeleted { seq, rev, by } => {
                self.known.sequences.remove(&seq);
                self.emit(LinkEvent::SequenceRemoved { seq: ItemId(seq), rev, by });
            }
            TeamEvent::LockChanged { seq, holder } => {
                match &holder {
                    Some(h) => self.known.locks.insert(seq, h.clone()),
                    None => self.known.locks.remove(&seq),
                };
                self.emit(LinkEvent::LockChanged { seq: ItemId(seq), holder });
            }
            TeamEvent::SharedUpdated { rev, .. } => {
                if rev > self.known.shared_rev {
                    self.fetch(None);
                }
            }
            TeamEvent::Other { .. } => {}
        }
    }

    /// Fetches the project and reports what changed since the last look.
    fn fetch(&mut self, by: Option<(u64, &str)>) {
        match self.client.get_project(&self.project) {
            Ok(data) => {
                let held = self.held();
                for ev in self.known.advance(&data, by, &held) {
                    self.emit(ev);
                }
            }
            Err(e) => self.status.set_error(Some(format!("Updates: {e}"))),
        }
    }

    /// Polls every [`POLL_EVERY`] (the first at once) until `deadline`.
    fn poll_until(&mut self, deadline: Instant) {
        let mut next_poll = Instant::now();
        while !self.stopped() {
            let now = Instant::now();
            if now >= next_poll {
                if catch_unwind(AssertUnwindSafe(|| self.fetch(None))).is_err() {
                    self.status.set_error(Some("Polling the Drive failed unexpectedly".into()));
                }
                next_poll = Instant::now() + POLL_EVERY;
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(TICK);
        }
    }

    fn emit(&self, ev: LinkEvent) {
        let _ = self.events.send(ev);
    }
}
