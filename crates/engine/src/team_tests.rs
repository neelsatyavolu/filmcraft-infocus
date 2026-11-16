//! Team projects in the engine (`team.rs`): sequence locks, pushes, teammates' updates and a lost
//! lock, against fake links (no network).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use filmcraft_project::{ItemId, Project};
use serde_json::{Value, json};
use web_time::Instant;

use crate::Session;
use crate::team::{LinkError, LinkEvent, SequenceStatus, SharedPush, TeamLink, TeamState};

/// What a [`FakeLink`] was asked to do.
#[derive(Clone, Debug, PartialEq)]
enum Call {
    Take(ItemId),
    Heartbeat(ItemId),
    Release(ItemId),
    Put(ItemId, u64, Value),
    Delete(ItemId, u64),
    Shared(SharedPush),
    Refresh,
}

#[derive(Default)]
struct Fake {
    calls: Vec<Call>,
    queued: Vec<LinkEvent>,
    /// Locks someone else holds: taking them is refused.
    others: BTreeMap<ItemId, String>,
    /// Heartbeats get no answer (the Mac is asleep or offline).
    silent: bool,
    /// The Drive's answer to a sequence delete (None: done).
    refuse_delete: Option<LinkError>,
    /// The revision a shared post gets (None: the next one).
    post_rev: Option<u64>,
}

/// Answers every request at once, as a Drive that always agrees would.
struct FakeLink(Arc<Mutex<Fake>>);

impl FakeLink {
    fn with<R>(&self, f: impl FnOnce(&mut Fake) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl TeamLink for FakeLink {
    fn take_lock(&mut self, seq: ItemId) {
        self.with(|f| {
            f.calls.push(Call::Take(seq));
            let ev = match f.others.get(&seq) {
                Some(h) => LinkEvent::LockRefused { seq, holder: h.clone() },
                None => LinkEvent::LockTaken { seq },
            };
            f.queued.push(ev);
        });
    }
    fn heartbeat(&mut self, seq: ItemId) {
        self.with(|f| {
            f.calls.push(Call::Heartbeat(seq));
            if !f.silent {
                f.queued.push(LinkEvent::HeartbeatOk { seq });
            }
        });
    }
    fn release(&mut self, seq: ItemId) {
        self.with(|f| f.calls.push(Call::Release(seq)));
    }
    fn put_sequence(&mut self, seq: ItemId, base_rev: u64, json: Value) {
        self.with(|f| {
            f.calls.push(Call::Put(seq, base_rev, json));
            f.queued.push(LinkEvent::SequencePut { seq, rev: base_rev + 1 });
        });
    }
    fn delete_sequence(&mut self, seq: ItemId, base_rev: u64) {
        self.with(|f| {
            f.calls.push(Call::Delete(seq, base_rev));
            let ev = match f.refuse_delete.clone() {
                Some(error) => LinkEvent::SequenceDeleteFailed { seq, error },
                None => LinkEvent::SequenceDeleted { seq, rev: base_rev + 1 },
            };
            f.queued.push(ev);
        });
    }
    fn post_shared(&mut self, push: SharedPush) {
        self.with(|f| {
            let rev = f.post_rev.unwrap_or(push.base_rev + 1);
            f.calls.push(Call::Shared(push));
            f.queued.push(LinkEvent::SharedPosted { rev });
        });
    }
    fn refresh(&mut self) {
        self.with(|f| f.calls.push(Call::Refresh));
    }
    fn poll(&mut self) -> Vec<LinkEvent> {
        self.with(|f| std::mem::take(&mut f.queued))
    }
}

fn seq_named(s: &Session, name: &str) -> ItemId {
    s.project.sequences().find(|q| q.name == name).map(|q| q.id).unwrap()
}

/// The demo project as a team project at slot 1; Maya holds the lock on "Rough Cut".
/// Returns the session, the fake's state, "Main Edit" and "Rough Cut".
fn team_demo() -> (Session, Arc<Mutex<Fake>>, ItemId, ItemId) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let (main, rough) = (seq_named(&s, "Main Edit"), seq_named(&s, "Rough Cut"));
    let fake = Arc::new(Mutex::new(Fake::default()));
    fake.lock().unwrap().others.insert(rough, "Maya".into());
    let revs = s.project.sequences().map(|q| (q.id, 1)).collect();
    let holders = BTreeMap::from([(rough, "Maya".to_string())]);
    s.attach_team(TeamState::new(Box::new(FakeLink(fake.clone())), "me".into(), 1, revs, 1, holders)).unwrap();
    (s, fake, main, rough)
}

fn calls(fake: &Arc<Mutex<Fake>>) -> Vec<Call> {
    std::mem::take(&mut fake.lock().unwrap().calls)
}

fn add_marker(s: &mut Session, seq: ItemId, name: &str) -> crate::Result<Value> {
    s.state.active_sequence = Some(seq);
    s.execute("markers.add", json!({"name": name}))
}

fn has_marker(p: &Project, seq: ItemId, name: &str) -> bool {
    p.sequence(seq).unwrap().markers.iter().any(|m| m.name == name)
}

fn later(secs: u64) -> Instant {
    Instant::now() + Duration::from_secs(secs)
}

#[test]
fn session_without_team_behaves_as_before() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let main = seq_named(&s, "Main Edit");
    let depth = s.history.undo.len();
    add_marker(&mut s, main, "plain").unwrap();
    assert!(s.team.is_none());
    assert_eq!(s.history.undo.len(), depth + 1);
    assert!(has_marker(&s.project, main, "plain"));
    assert_eq!(s.team_status(main), None);
    s.poll_team(later(200)); // no team: nothing happens
    s.execute("edit.undo", json!({})).unwrap();
    assert!(!has_marker(&s.project, main, "plain"));
}

#[test]
fn edit_touching_foreign_lock_reverts() {
    let (mut s, fake, _main, rough) = team_demo();
    s.state.active_sequence = Some(rough);
    let project = s.project.clone();
    let depth = s.history.undo.len();
    let err = add_marker(&mut s, rough, "mine").unwrap_err().to_string();
    assert!(err.contains("Maya is editing “Rough Cut”"), "{err}");
    assert!(Arc::ptr_eq(&project, &s.project), "the project is the one from before the edit");
    assert_eq!(s.history.undo.len(), depth, "no undo step is left behind");
    assert!(!s.is_dirty());
    s.poll_team(later(5));
    assert!(!calls(&fake).iter().any(|c| matches!(c, Call::Put(..))), "nothing is pushed");
}

#[test]
fn edit_of_free_sequence_without_lock_reverts() {
    let (mut s, _fake, main, _) = team_demo();
    let err = add_marker(&mut s, main, "x").unwrap_err().to_string();
    assert!(err.contains("Take the lock to edit “Main Edit”"), "{err}");
    assert_eq!(s.team_status(main), Some(SequenceStatus::Free));
}

#[test]
fn edit_in_held_sequence_pushes_after_debounce() {
    let (mut s, fake, main, _) = team_demo();
    s.team_take_lock(main).unwrap();
    assert_eq!(s.team_status(main), Some(SequenceStatus::Requested));
    s.poll_team(Instant::now());
    assert_eq!(s.team_status(main), Some(SequenceStatus::Editing { unsynced: false }));
    calls(&fake);
    add_marker(&mut s, main, "cut here").unwrap();
    assert_eq!(s.team_status(main), Some(SequenceStatus::Editing { unsynced: true }));
    s.poll_team(Instant::now());
    assert!(!calls(&fake).iter().any(|c| matches!(c, Call::Put(..))), "not before the 1 s debounce");
    s.poll_team(later(2));
    let puts: Vec<Call> = calls(&fake).into_iter().filter(|c| matches!(c, Call::Put(..))).collect();
    assert_eq!(puts.len(), 1);
    let Call::Put(id, base, json) = &puts[0] else { unreachable!() };
    assert_eq!((*id, *base), (main, 1));
    assert!(json.to_string().contains("cut here"));
    s.poll_team(later(3));
    assert_eq!(s.team.as_ref().unwrap().revs[&main], 2);
    assert_eq!(s.team_status(main), Some(SequenceStatus::Editing { unsynced: false }));
}

#[test]
fn import_pushes_shared_immediately() {
    let (mut s, fake, _, _) = team_demo();
    s.execute("file.newAdjustmentLayer", json!({})).unwrap();
    let shared: Vec<SharedPush> = calls(&fake)
        .into_iter()
        .filter_map(|c| match c {
            Call::Shared(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(shared.len(), 1, "sent without waiting for a poll");
    let new = shared[0].upserts.keys().next().copied().unwrap();
    assert_eq!(new.0 >> filmcraft_project::ID_SLOT_SHIFT, 1, "the new item has an id from this member's slot");
    assert!(shared[0].bins.is_some(), "the bins changed too");
    assert!(shared[0].deletes.is_empty());
}

#[test]
fn deleting_a_used_shared_item_is_refused() {
    let (mut s, fake, main, _) = team_demo();
    let used = s.project.sequence(main).unwrap().all_tracks().flat_map(|t| t.items.iter()).next().unwrap().item;
    let name = s.project.item(used).unwrap().name.clone();
    calls(&fake);
    // Clear in the Project panel also takes the clips out of Main Edit, which this member does not hold
    let err = s.execute("project.delete", json!({"items": [used.0]})).unwrap_err().to_string();
    assert!(err.contains("Take the lock to edit “Main Edit”"), "{err}");
    assert!(s.project.item(used).is_some());
    // an edit made outside `execute` that drops the item but leaves its clips is taken back
    s.edit("Drop item", |p, _| {
        p.items.remove(&used);
        Ok(())
    })
    .unwrap();
    s.poll_team(Instant::now());
    assert!(s.project.item(used).is_some(), "the item is back");
    assert!(s.log.entries.iter().any(|e| e.message.contains(&format!("“{name}” is used in “Main Edit”"))));
    assert!(!calls(&fake).iter().any(|c| matches!(c, Call::Shared(p) if p.deletes.contains(&used))));
}

#[test]
fn remote_update_survives_local_undo() {
    let (mut s, _fake, main, rough) = team_demo();
    s.team_take_lock(main).unwrap();
    s.poll_team(Instant::now());
    add_marker(&mut s, main, "local").unwrap();
    // Maya saved Rough Cut with a marker
    let mut theirs = (*s.project).clone();
    let mut marker = s.project.sequence(main).unwrap().markers[0].clone();
    marker.name = "maya".into();
    theirs.sequence_mut(rough).unwrap().markers.push(marker);
    let json = filmcraft_team::sequence_json(&theirs, rough).unwrap();
    let depth = s.history.undo.len();
    s.apply_remote_sequence(rough, 2, &json).unwrap();
    assert_eq!(s.history.undo.len(), depth, "no undo step for a teammate's work");
    assert!(has_marker(&s.project, rough, "maya"));
    s.execute("edit.undo", json!({})).unwrap();
    assert!(!has_marker(&s.project, main, "local"), "my edit is undone");
    assert!(has_marker(&s.project, rough, "maya"), "Maya's edit stays");
    s.execute("edit.redo", json!({})).unwrap();
    assert!(has_marker(&s.project, main, "local") && has_marker(&s.project, rough, "maya"));
    // an older revision is ignored
    let before = s.project.clone();
    s.apply_remote_sequence(rough, 1, &filmcraft_team::sequence_json(&theirs, rough).unwrap()).unwrap();
    assert!(Arc::ptr_eq(&before, &s.project));
}

#[test]
fn lost_lock_saves_recovered_copy() {
    let (mut s, fake, main, _) = team_demo();
    let t0 = Instant::now();
    s.team_take_lock(main).unwrap();
    s.poll_team(t0);
    add_marker(&mut s, main, "unsaved work").unwrap();
    // the Mac sleeps before the push; nobody answers heartbeats
    fake.lock().unwrap().silent = true;
    calls(&fake);
    s.poll_team(t0 + Duration::from_secs(130));
    let sent = calls(&fake);
    assert!(!sent.iter().any(|c| matches!(c, Call::Put(id, ..) if *id == main)), "the stale edits are not pushed: {sent:?}");
    assert!(sent.contains(&Call::Refresh), "the Drive's version is fetched again");
    assert_eq!(s.team_status(main), Some(SequenceStatus::Free));
    let copy = s.project.sequences().find(|q| q.name == "Recovered – Main Edit").map(|q| q.id).expect("a recovered copy");
    assert!(has_marker(&s.project, copy, "unsaved work"));
    assert_eq!(copy.0 >> filmcraft_project::ID_SLOT_SHIFT, 1, "the copy has an id from this member's slot");
    assert!(sent.contains(&Call::Take(copy)), "the copy is locked for this member");
    // undo does not take the copy away
    while s.history.can_undo() {
        s.undo();
    }
    assert!(s.project.item(copy).is_some());
    // and the copy is saved on the Drive
    fake.lock().unwrap().silent = false;
    let t1 = t0 + Duration::from_secs(131);
    s.poll_team(t1);
    s.poll_team(t1 + Duration::from_secs(2));
    assert!(calls(&fake).iter().any(|c| matches!(c, Call::Put(id, 0, _) if *id == copy)));
}

#[test]
fn lock_taken_by_a_teammate_is_given_up_before_refetching() {
    // the Mac slept past the lock's expiry and a teammate took it: the link must drop its stale
    // token first (a release does), or it keeps treating the sequence as this Mac's and never
    // fetches theirs
    let (mut s, fake, main, _) = team_demo();
    s.team_take_lock(main).unwrap();
    s.poll_team(Instant::now());
    add_marker(&mut s, main, "unsaved work").unwrap();
    calls(&fake);
    fake.lock().unwrap().queued.push(LinkEvent::LockChanged { seq: main, holder: Some("Maya".into()) });
    s.poll_team(later(0));
    let sent = calls(&fake);
    let forget = sent.iter().position(|c| *c == Call::Release(main)).expect("the lost lock is given up");
    let refresh = sent.iter().position(|c| *c == Call::Refresh).expect("the Drive's version is fetched");
    assert!(forget < refresh, "{sent:?}");
    assert!(s.project.sequences().any(|q| q.name == "Recovered – Main Edit"));
}

#[test]
fn idle_once_everything_is_on_the_drive() {
    let (mut s, _fake, main, _) = team_demo();
    assert!(s.team.as_ref().unwrap().idle());
    s.team_take_lock(main).unwrap();
    assert!(!s.team.as_ref().unwrap().idle(), "the lock is asked for");
    s.poll_team(later(0));
    add_marker(&mut s, main, "work").unwrap();
    assert!(!s.team.as_ref().unwrap().idle(), "the edit waits for its push");
    s.poll_team(later(2));
    s.poll_team(later(3));
    assert!(s.team.as_ref().unwrap().idle());
}

#[test]
fn heartbeat_every_20_s_keeps_the_lock() {
    let (mut s, fake, main, _) = team_demo();
    let t0 = Instant::now();
    s.team_take_lock(main).unwrap();
    s.poll_team(t0);
    calls(&fake);
    s.poll_team(t0 + Duration::from_secs(10));
    assert!(calls(&fake).is_empty());
    for k in 1..=10u64 {
        s.poll_team(t0 + Duration::from_secs(20 * k));
    }
    let beats = calls(&fake).iter().filter(|c| **c == Call::Heartbeat(main)).count();
    assert!(beats >= 9, "{beats}");
    assert!(matches!(s.team_status(main), Some(SequenceStatus::Editing { .. })), "200 s later the lock is still held");
}

#[test]
fn push_refused_for_lost_lock_saves_recovered_copy() {
    let (mut s, fake, main, _) = team_demo();
    s.team_take_lock(main).unwrap();
    s.poll_team(Instant::now());
    add_marker(&mut s, main, "late").unwrap();
    // the server answers the push with "lock lost"
    s.poll_team(later(2));
    fake.lock().unwrap().queued.retain(|e| !matches!(e, LinkEvent::SequencePut { .. }));
    fake.lock().unwrap().queued.push(LinkEvent::SequencePutFailed { seq: main, error: LinkError::LockLost });
    s.poll_team(later(3));
    assert!(s.project.sequences().any(|q| q.name == "Recovered – Main Edit"));
}

#[test]
fn new_sequence_is_locked_and_pushed() {
    let (mut s, fake, _, _) = team_demo();
    s.execute("file.newSequence", json!({"name": "Interview"})).unwrap();
    let seq = seq_named(&s, "Interview");
    assert!(calls(&fake).contains(&Call::Take(seq)));
    s.poll_team(later(2));
    s.poll_team(later(3));
    assert!(calls(&fake).iter().any(|c| matches!(c, Call::Put(id, 0, _) if *id == seq)));
    assert_eq!(s.team.as_ref().unwrap().revs.get(&seq), Some(&1));
}

#[test]
fn opening_another_project_leaves_team_mode() {
    let (mut s, fake, main, _) = team_demo();
    s.team_take_lock(main).unwrap();
    s.poll_team(Instant::now());
    calls(&fake);
    s.execute("file.newProject", json!({})).unwrap();
    assert!(s.team.is_none());
    assert!(calls(&fake).contains(&Call::Release(main)));
}

// ---- two sessions through one in-memory Drive ----

#[derive(Default)]
struct Service {
    shared: Value,
    shared_rev: u64,
    seqs: BTreeMap<ItemId, (u64, Value)>,
    locks: BTreeMap<ItemId, String>,
    outbox: BTreeMap<String, Vec<LinkEvent>>,
}

impl Service {
    fn broadcast(&mut self, from: &str, ev: &LinkEvent) {
        for (who, q) in self.outbox.iter_mut() {
            if who != from {
                q.push(ev.clone());
            }
        }
    }
    fn reply(&mut self, to: &str, ev: LinkEvent) {
        self.outbox.entry(to.into()).or_default().push(ev);
    }
}

struct ServiceLink {
    me: String,
    svc: Arc<Mutex<Service>>,
}

impl TeamLink for ServiceLink {
    fn take_lock(&mut self, seq: ItemId) {
        let mut v = self.svc.lock().unwrap();
        match v.locks.get(&seq).cloned() {
            Some(h) if h != self.me => v.reply(&self.me, LinkEvent::LockRefused { seq, holder: h }),
            _ => {
                v.locks.insert(seq, self.me.clone());
                v.reply(&self.me, LinkEvent::LockTaken { seq });
                let ev = LinkEvent::LockChanged { seq, holder: Some(self.me.clone()) };
                v.broadcast(&self.me, &ev);
            }
        }
    }
    fn heartbeat(&mut self, seq: ItemId) {
        self.svc.lock().unwrap().reply(&self.me, LinkEvent::HeartbeatOk { seq });
    }
    fn release(&mut self, seq: ItemId) {
        let mut v = self.svc.lock().unwrap();
        v.locks.remove(&seq);
        v.broadcast(&self.me, &LinkEvent::LockChanged { seq, holder: None });
    }
    fn put_sequence(&mut self, seq: ItemId, base_rev: u64, json: Value) {
        let mut v = self.svc.lock().unwrap();
        let cur = v.seqs.get(&seq).map(|s| s.0).unwrap_or(0);
        if v.locks.get(&seq) != Some(&self.me) {
            v.reply(&self.me, LinkEvent::SequencePutFailed { seq, error: LinkError::LockLost });
        } else if base_rev != cur {
            v.reply(&self.me, LinkEvent::SequencePutFailed { seq, error: LinkError::Stale { rev: cur } });
        } else {
            v.seqs.insert(seq, (cur + 1, json.clone()));
            v.reply(&self.me, LinkEvent::SequencePut { seq, rev: cur + 1 });
            let ev = LinkEvent::SequenceUpdated { seq, rev: cur + 1, by: self.me.clone(), json };
            v.broadcast(&self.me, &ev);
        }
    }
    fn delete_sequence(&mut self, seq: ItemId, base_rev: u64) {
        let mut v = self.svc.lock().unwrap();
        let cur = v.seqs.get(&seq).map(|s| s.0).unwrap_or(0);
        if v.locks.get(&seq) != Some(&self.me) {
            v.reply(&self.me, LinkEvent::SequenceDeleteFailed { seq, error: LinkError::LockLost });
        } else if base_rev != cur {
            v.reply(&self.me, LinkEvent::SequenceDeleteFailed { seq, error: LinkError::Stale { rev: cur } });
        } else {
            v.seqs.remove(&seq);
            v.locks.remove(&seq);
            v.reply(&self.me, LinkEvent::SequenceDeleted { seq, rev: cur + 1 });
            let ev = LinkEvent::SequenceRemoved { seq, rev: cur + 1, by: self.me.clone() };
            v.broadcast(&self.me, &ev);
            v.broadcast(&self.me, &LinkEvent::LockChanged { seq, holder: None });
        }
    }
    fn post_shared(&mut self, push: SharedPush) {
        let mut v = self.svc.lock().unwrap();
        if push.bins.is_some() && push.base_rev != v.shared_rev {
            let rev = v.shared_rev;
            v.reply(&self.me, LinkEvent::SharedPostFailed { error: LinkError::Stale { rev } });
            return;
        }
        let items = v.shared["items"].as_object_mut().unwrap();
        for (id, item) in push.upserts {
            items.insert(id.0.to_string(), item);
        }
        for id in push.deletes {
            items.remove(&id.0.to_string());
        }
        if let Some(b) = push.bins {
            v.shared["root"] = b;
        }
        for (key, value) in push.fields {
            v.shared[key.as_str()] = value;
        }
        v.shared_rev += 1;
        let rev = v.shared_rev;
        v.reply(&self.me, LinkEvent::SharedPosted { rev });
        let ev = LinkEvent::SharedUpdated { rev, shared: v.shared.clone() };
        v.broadcast(&self.me, &ev);
    }
    fn refresh(&mut self) {}
    fn poll(&mut self) -> Vec<LinkEvent> {
        std::mem::take(self.svc.lock().unwrap().outbox.entry(self.me.clone()).or_default())
    }
}

fn member(svc: &Arc<Mutex<Service>>, me: &str, slot: u16) -> Session {
    let (doc, revs) = {
        let v = svc.lock().unwrap();
        let doc = filmcraft_team::TeamDoc { shared: v.shared.clone(), sequences: v.seqs.iter().map(|(id, (_, j))| (*id, j.clone())).collect() };
        (doc, v.seqs.iter().map(|(id, (r, _))| (*id, *r)).collect())
    };
    let mut s = Session::new(Arc::new(crate::FsServices));
    s.project = Arc::new(filmcraft_team::merge(&doc, &|_| Default::default()).unwrap());
    svc.lock().unwrap().outbox.insert(me.into(), Vec::new());
    s.attach_team(TeamState::new(Box::new(ServiceLink { me: me.into(), svc: svc.clone() }), me.into(), slot, revs, 0, BTreeMap::new())).unwrap();
    s
}

fn shared_view(s: &Session) -> filmcraft_team::TeamDoc {
    let mut doc = filmcraft_team::split(&s.project, &|_| None).unwrap();
    doc.shared.as_object_mut().unwrap().remove("next_id");
    doc
}

#[test]
fn two_sessions_converge() {
    let mut demo = Session::default();
    demo.execute("file.openDemoProject", json!({})).unwrap();
    let doc = filmcraft_team::split(&demo.project, &|_| None).unwrap();
    let svc = Arc::new(Mutex::new(Service { shared: doc.shared, seqs: doc.sequences.into_iter().map(|(id, j)| (id, (1, j))).collect(), ..Default::default() }));
    let mut a = member(&svc, "ana", 1);
    let mut b = member(&svc, "ben", 2);
    let (main, rough) = (seq_named(&a, "Main Edit"), seq_named(&a, "Rough Cut"));
    let t0 = Instant::now();
    a.team_take_lock(main).unwrap();
    b.team_take_lock(rough).unwrap();
    a.poll_team(t0);
    b.poll_team(t0);
    // B cannot take A's sequence
    b.team_take_lock(main).unwrap();
    b.poll_team(t0);
    assert_eq!(b.team_status(main), Some(SequenceStatus::ReadOnly { holder: "ana".into() }));
    assert!(add_marker(&mut b, main, "no").is_err());

    add_marker(&mut a, main, "from ana").unwrap();
    add_marker(&mut b, rough, "from ben").unwrap();
    a.execute("file.newAdjustmentLayer", json!({})).unwrap();
    b.execute("file.newBin", json!({"name": "Ben's bin"})).unwrap();
    for k in 1..6u64 {
        let now = t0 + Duration::from_secs(k);
        a.poll_team(now);
        b.poll_team(now);
    }
    assert!(has_marker(&b.project, main, "from ana"));
    assert!(has_marker(&a.project, rough, "from ben"));
    let (da, db) = (shared_view(&a), shared_view(&b));
    assert_eq!(da.sequences, db.sequences);
    assert_eq!(da.shared, db.shared);
}

#[path = "team_sync_tests.rs"]
mod sync;

#[path = "team_safety_tests.rs"]
mod safety;

/// A lock this member holds on another Mac reads as their own, not "You on Lab Mac is editing".
#[test]
fn own_lock_on_another_mac_reads_naturally() {
    use crate::team::{editing_message, own_device_holder};
    assert_eq!(editing_message(&own_device_holder("Lab Mac"), "this"), "You're editing this on Lab Mac");
    assert_eq!(editing_message(&own_device_holder("Lab Mac"), "“Rough Cut”"), "You're editing “Rough Cut” on Lab Mac");
    assert_eq!(editing_message("Maya", "“Rough Cut”"), "Maya is editing “Rough Cut”");
}

#[test]
fn edit_refused_by_own_lock_on_another_mac_says_so() {
    let (mut s, _fake, _main, rough) = team_demo();
    s.state.active_sequence = Some(rough);
    s.team.as_mut().unwrap().holders.insert(rough, crate::team::own_device_holder("Lab Mac"));
    let err = add_marker(&mut s, rough, "mine").unwrap_err().to_string();
    assert!(err.contains("You're editing “Rough Cut” on Lab Mac"), "{err}");
}

#[test]
fn push_items_sends_shared_items_as_they_are() {
    let (mut s, fake, main, _) = team_demo();
    calls(&fake);
    let media = s.project.items.values().find(|i| i.as_media().is_some()).map(|i| i.id).unwrap();
    s.team_push_items(&[media, main, ItemId(999_999)]);
    let shared: Vec<SharedPush> = calls(&fake)
        .into_iter()
        .filter_map(|c| match c {
            Call::Shared(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].upserts.keys().copied().collect::<Vec<_>>(), vec![media], "sequences and unknown ids are skipped");
    assert!(shared[0].bins.is_none() && shared[0].deletes.is_empty());
    s.team_push_items(&[main]);
    assert!(calls(&fake).is_empty(), "nothing to send");
    let mut plain = Session::default();
    plain.team_push_items(&[media]);
}
