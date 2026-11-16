//! Team projects: ids that must never be handed out twice, shared parts that arrive late, a
//! teammate's delete of an item a held sequence still uses, and flushing before leaving, against
//! the fake links of `team_tests.rs`.

use std::collections::BTreeSet;

use super::*;

fn ids(s: &Session) -> BTreeSet<ItemId> {
    s.project.items.keys().copied().collect()
}

/// The one item `s` has that `before` did not.
fn added(before: &BTreeSet<ItemId>, s: &Session) -> ItemId {
    let new: Vec<ItemId> = ids(s).difference(before).copied().collect();
    assert_eq!(new.len(), 1, "{new:?}");
    new[0]
}

#[test]
fn undo_of_an_import_never_hands_its_id_out_again() {
    let (mut s, _fake, _, _) = team_demo();
    let before = ids(&s);
    s.execute("file.newAdjustmentLayer", json!({})).unwrap();
    let first = added(&before, &s);
    // the item went to the Drive at once: teammates have its id
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.item(first).is_none());
    s.execute("file.newAdjustmentLayer", json!({})).unwrap();
    let second = added(&before, &s);
    assert_ne!(first, second, "an id the Drive already has is not handed out again");
    // undo from the History panel (outside `execute`) too
    s.undo();
    s.poll_team(Instant::now());
    s.execute("file.newAdjustmentLayer", json!({})).unwrap();
    let third = added(&before, &s);
    assert!(third != first && third != second, "{first:?} {second:?} {third:?}");
    // no undo step for raising it
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.item(third).is_none(), "undo still takes back the import itself");
}

#[test]
fn ids_from_this_members_other_mac_are_not_handed_out_again() {
    // an older Drive gives a member one slot for all their Macs
    let mut demo = Session::default();
    demo.execute("file.openDemoProject", json!({})).unwrap();
    let doc = filmcraft_team::split(&demo.project, &|_| None).unwrap();
    let svc = Arc::new(Mutex::new(Service { shared: doc.shared, seqs: doc.sequences.into_iter().map(|(id, j)| (id, (1, j))).collect(), ..Default::default() }));
    let mut laptop = member(&svc, "ana-laptop", 1);
    let mut lab = member(&svc, "ana-lab", 1);
    let before = ids(&laptop);
    laptop.execute("file.newAdjustmentLayer", json!({})).unwrap();
    let theirs = added(&before, &laptop);
    let t0 = Instant::now();
    laptop.poll_team(t0);
    lab.poll_team(t0);
    assert!(lab.project.item(theirs).is_some(), "the lab Mac got the laptop's item");
    let before = ids(&lab);
    lab.execute("file.newAdjustmentLayer", json!({})).unwrap();
    let mine = added(&before, &lab);
    assert!(mine.0 > theirs.0, "the lab Mac allocates past the laptop's ids: {mine:?} vs {theirs:?}");
}

#[test]
fn late_shared_part_keeps_items_this_mac_posted_after_it() {
    let (mut s, fake, _, _) = team_demo();
    let older = filmcraft_team::split(&s.project, &|_| None).unwrap().shared;
    // a teammate's post lands first (rev 2), then this Mac's new item (rev 3)
    fake.lock().unwrap().post_rev = Some(3);
    let before = ids(&s);
    s.execute("file.newAdjustmentLayer", json!({})).unwrap();
    let mine = added(&before, &s);
    s.poll_team(Instant::now());
    // the teammate's shared part at rev 2 arrives late: it can't know this Mac's item
    fake.lock().unwrap().queued.push(LinkEvent::SharedUpdated { rev: 2, shared: older });
    s.poll_team(Instant::now());
    assert!(s.project.item(mine).is_some(), "an item posted at a newer revision is not a delete");
}

#[test]
fn teammates_delete_of_an_item_a_held_sequence_uses_is_put_back() {
    let (mut s, fake, main, _) = team_demo();
    s.team_take_lock(main).unwrap();
    s.poll_team(Instant::now());
    let used = s.project.sequence(main).unwrap().all_tracks().flat_map(|t| t.items.iter()).next().unwrap().item;
    // the teammate deleted it on the Drive (where Main Edit doesn't use it yet)
    let mut theirs = (*s.project).clone();
    theirs.items.remove(&used);
    theirs.root.remove_item(used);
    let shared = filmcraft_team::split(&theirs, &|_| None).unwrap().shared;
    calls(&fake);
    fake.lock().unwrap().queued.push(LinkEvent::SharedUpdated { rev: 2, shared });
    s.poll_team(Instant::now());
    assert!(s.project.item(used).is_some(), "Main Edit still uses it here");
    assert!(s.project.root.parent_of(used).is_some() || s.project.root.children.contains(&filmcraft_project::BinEntry::Item(used)), "and it is in a bin");
    let sent = calls(&fake);
    assert!(sent.iter().any(|c| matches!(c, Call::Shared(p) if p.upserts.contains_key(&used))), "it goes to the Drive again: {sent:?}");
}

#[test]
fn flush_sends_debounced_edits_at_once() {
    let (mut s, fake, main, _) = team_demo();
    s.team_take_lock(main).unwrap();
    s.poll_team(Instant::now());
    add_marker(&mut s, main, "now").unwrap();
    calls(&fake);
    assert!(!s.team.as_ref().unwrap().idle());
    s.team_flush();
    assert!(calls(&fake).iter().any(|c| matches!(c, Call::Put(id, ..) if *id == main)), "pushed without waiting for the debounce");
    s.poll_team(Instant::now());
    assert!(s.team.as_ref().unwrap().idle());
}

#[test]
fn a_lock_given_up_after_a_refused_push_is_released() {
    // a stale push means the lock may still be this Mac's on the Drive: it is released, not left
    // to expire while teammates wait
    let (mut s, fake, main, _) = team_demo();
    s.team_take_lock(main).unwrap();
    s.poll_team(Instant::now());
    calls(&fake);
    fake.lock().unwrap().queued.push(LinkEvent::SequencePutFailed { seq: main, error: LinkError::Stale { rev: 7 } });
    s.poll_team(Instant::now());
    let sent = calls(&fake);
    let release = sent.iter().position(|c| *c == Call::Release(main)).expect("the lock is released");
    let refresh = sent.iter().position(|c| *c == Call::Refresh).expect("the Drive's version is fetched");
    assert!(release < refresh, "{sent:?}");
}
