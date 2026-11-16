//! Team projects: deleting sequences and sharing project-level fields, against the fake links of
//! `team_tests.rs`.

use super::*;

fn held(s: &mut Session, seq: ItemId) {
    s.team_take_lock(seq).unwrap();
    s.poll_team(Instant::now());
    assert!(matches!(s.team_status(seq), Some(SequenceStatus::Editing { .. })));
}

fn delete(s: &mut Session, seq: ItemId) -> crate::Result<Value> {
    s.execute("project.delete", json!({"items": [seq.0]}))
}

fn shared_pushes(calls: &[Call]) -> Vec<SharedPush> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::Shared(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

fn in_bins(p: &Project, seq: ItemId) -> bool {
    p.root.parent_of(seq).is_some() || p.root.children.contains(&filmcraft_project::BinEntry::Item(seq))
}

#[test]
fn deleting_a_held_sequence_deletes_it_on_the_drive() {
    let (mut s, fake, main, _) = team_demo();
    held(&mut s, main);
    calls(&fake);
    delete(&mut s, main).unwrap();
    assert!(s.project.item(main).is_none());
    let sent = calls(&fake);
    assert!(sent.contains(&Call::Delete(main, 1)), "deleted at its revision: {sent:?}");
    assert!(shared_pushes(&sent).iter().any(|p| p.bins.is_some()), "the bins without it go to the Drive");
    s.poll_team(later(1));
    let t = s.team.as_ref().unwrap();
    assert_eq!(t.revs.get(&main), Some(&2), "the Drive's revision is kept");
    assert!(t.held.is_empty(), "the lock went with the sequence");
    assert!(!calls(&fake).iter().any(|c| matches!(c, Call::Put(..) | Call::Release(_))));
}

#[test]
fn deleting_someone_elses_or_a_free_sequence_is_refused() {
    let (mut s, fake, main, rough) = team_demo();
    let err = delete(&mut s, rough).unwrap_err().to_string();
    assert!(err.contains("Maya is editing “Rough Cut”"), "{err}");
    let err = delete(&mut s, main).unwrap_err().to_string();
    assert!(err.contains("Take the lock to delete “Main Edit”"), "{err}");
    assert!(s.project.item(main).is_some() && s.project.item(rough).is_some());
    s.poll_team(later(2));
    assert!(!calls(&fake).iter().any(|c| matches!(c, Call::Delete(..))));
}

#[test]
fn delete_refused_as_nested_puts_the_sequence_back() {
    let (mut s, fake, main, _) = team_demo();
    held(&mut s, main);
    fake.lock().unwrap().refuse_delete = Some(LinkError::ItemInUse { item: main });
    let depth = s.history.undo.len();
    delete(&mut s, main).unwrap();
    s.poll_team(later(1));
    assert!(s.project.item(main).is_some(), "the sequence is back");
    assert!(in_bins(&s.project, main), "and in the bins");
    assert_eq!(s.history.undo.len(), depth + 1, "putting it back is not an undo step");
    assert!(matches!(s.team_status(main), Some(SequenceStatus::Editing { .. })), "the lock is still held");
    assert!(s.log.entries.iter().any(|e| e.message.contains("is used in another sequence on the Drive")));
    s.poll_team(later(3));
    assert!(calls(&fake).iter().any(|c| matches!(c, Call::Put(id, 1, _) if *id == main)), "and saved again");
}

#[test]
fn delete_refused_for_a_lost_lock_puts_the_sequence_back_read_only() {
    let (mut s, fake, main, _) = team_demo();
    held(&mut s, main);
    fake.lock().unwrap().refuse_delete = Some(LinkError::LockLost);
    delete(&mut s, main).unwrap();
    calls(&fake);
    s.poll_team(later(1));
    assert!(s.project.item(main).is_some() && in_bins(&s.project, main));
    assert_eq!(s.team_status(main), Some(SequenceStatus::Free));
    assert!(calls(&fake).contains(&Call::Refresh), "the Drive's version is fetched");
    assert!(!s.project.sequences().any(|q| q.name.starts_with("Recovered")), "nothing to recover");
}

#[test]
fn undo_of_a_delete_saves_the_sequence_from_its_last_revision() {
    let (mut s, fake, main, _) = team_demo();
    held(&mut s, main);
    delete(&mut s, main).unwrap();
    s.poll_team(later(1));
    calls(&fake);
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.item(main).is_some());
    assert!(calls(&fake).contains(&Call::Take(main)), "the lock is asked for again");
    s.poll_team(later(2));
    s.poll_team(later(4));
    let sent = calls(&fake);
    assert!(sent.iter().any(|c| matches!(c, Call::Put(id, 2, _) if *id == main)), "saved from revision 2: {sent:?}");
}

#[test]
fn undo_before_the_delete_is_sent_keeps_the_lock() {
    let (mut s, fake, main, _) = team_demo();
    held(&mut s, main);
    s.execute("file.newSequence", json!({"name": "Interview"})).unwrap();
    let other = seq_named(&s, "Interview");
    s.poll_team(Instant::now());
    add_marker(&mut s, other, "edit").unwrap();
    calls(&fake);
    delete(&mut s, main).unwrap();
    let sent = calls(&fake);
    assert!(sent.iter().any(|c| matches!(c, Call::Put(id, ..) if *id == other)), "pending pushes go first: {sent:?}");
    assert!(!sent.iter().any(|c| matches!(c, Call::Delete(..))), "the delete waits for them");
    s.execute("edit.undo", json!({})).unwrap();
    s.poll_team(later(1));
    s.poll_team(later(3));
    assert!(s.project.item(main).is_some());
    assert!(!calls(&fake).iter().any(|c| matches!(c, Call::Delete(..) | Call::Take(_))), "never deleted, the lock never left");
    assert!(matches!(s.team_status(main), Some(SequenceStatus::Editing { .. })));
}

#[test]
fn remote_delete_removes_the_sequence_everywhere() {
    let (mut s, fake, main, rough) = team_demo();
    held(&mut s, main);
    add_marker(&mut s, main, "mine").unwrap();
    let depth = s.history.undo.len();
    fake.lock().unwrap().queued.push(LinkEvent::SequenceRemoved { seq: rough, rev: 2, by: "Maya".into() });
    fake.lock().unwrap().queued.push(LinkEvent::LockChanged { seq: rough, holder: None });
    s.poll_team(later(1));
    assert!(s.project.item(rough).is_none());
    assert!(!in_bins(&s.project, rough), "it left the bins");
    assert_eq!(s.history.undo.len(), depth, "no undo step");
    assert!(s.history.undo.iter().all(|(_, p, _)| p.item(rough).is_none()), "undo never brings it back");
    assert_eq!(s.team_status(rough), Some(SequenceStatus::Free));
    assert!(s.log.entries.iter().any(|e| e.message.contains("Maya deleted “Rough Cut”")));
    // an older revision of it is ignored
    s.apply_remote_sequence(rough, 1, &json!({})).unwrap();
    assert!(s.project.item(rough).is_none());
    // and my edit is untouched and not sent back as a change
    assert!(has_marker(&s.project, main, "mine"));
    s.poll_team(later(3));
    assert!(!shared_pushes(&calls(&fake)).iter().any(|p| p.bins.is_some()));
}

#[test]
fn field_changes_go_to_the_drive_without_local_folders() {
    let (mut s, fake, _, _) = team_demo();
    calls(&fake);
    s.edit("Notes", |p, _| {
        p.notes = "shoot B-roll Friday".into();
        Ok(())
    })
    .unwrap();
    s.poll_team(Instant::now());
    let pushes = shared_pushes(&calls(&fake));
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].fields.keys().collect::<Vec<_>>(), ["notes"]);
    assert_eq!(pushes[0].fields["notes"], json!("shoot B-roll Friday"));
    // this Mac's ingest folder is not a team change
    s.edit("Ingest", |p, _| {
        p.settings.ingest.destination = Some("/Users/me/Ingest".into());
        Ok(())
    })
    .unwrap();
    s.poll_team(later(1));
    assert!(shared_pushes(&calls(&fake)).iter().all(|p| p.fields.is_empty()));
    // a settings change sends the settings, still without the folder
    s.edit("Preset", |p, _| {
        p.settings.ingest.preset = "prores_proxy_half".into();
        Ok(())
    })
    .unwrap();
    s.poll_team(later(2));
    let pushes = shared_pushes(&calls(&fake));
    let settings = &pushes.iter().find(|p| p.fields.contains_key("settings")).unwrap().fields["settings"];
    assert_eq!(settings["ingest"]["destination"], Value::Null);
    assert!(!s.log.entries.iter().any(|e| e.message.contains("kept on this Mac")));
}

#[test]
fn remote_fields_apply_everywhere_but_keep_local_folders_and_pending_fields() {
    let (mut s, _fake, main, _) = team_demo();
    held(&mut s, main);
    add_marker(&mut s, main, "undoable").unwrap();
    s.edit("Ingest", |p, _| {
        p.settings.ingest.destination = Some("/Users/me/Ingest".into());
        Ok(())
    })
    .unwrap();
    s.poll_team(Instant::now());
    let mut theirs = filmcraft_team::split(&s.project, &|_| None).unwrap().shared;
    theirs["notes"] = json!("from Maya");
    theirs["settings"]["ingest"]["preset"] = json!("prores_proxy_half");
    let depth = s.history.undo.len();
    s.apply_remote_shared(5, &theirs).unwrap();
    assert_eq!(s.project.notes, "from Maya");
    assert_eq!(s.project.settings.ingest.preset, "prores_proxy_half");
    assert_eq!(s.project.settings.ingest.destination.as_deref(), Some("/Users/me/Ingest"), "this Mac's folder stays");
    assert_eq!(s.history.undo.len(), depth);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.notes, "from Maya", "undo keeps the teammate's notes");
    // a field still on its way keeps this Mac's value
    s.team.as_mut().unwrap().problem = Some("hold sends".into());
    s.edit("Notes", |p, _| {
        p.notes = "mine".into();
        Ok(())
    })
    .unwrap();
    s.poll_team(later(1));
    theirs["notes"] = json!("older from Maya");
    s.apply_remote_shared(6, &theirs).unwrap();
    assert_eq!(s.project.notes, "mine");
}

#[test]
fn two_sessions_converge_on_deletes_and_fields() {
    let mut demo = Session::default();
    demo.execute("file.openDemoProject", json!({})).unwrap();
    let doc = filmcraft_team::split(&demo.project, &|_| None).unwrap();
    let svc = Arc::new(Mutex::new(Service { shared: doc.shared, seqs: doc.sequences.into_iter().map(|(id, j)| (id, (1, j))).collect(), ..Default::default() }));
    let mut a = member(&svc, "ana", 1);
    let mut b = member(&svc, "ben", 2);
    let rough = seq_named(&a, "Rough Cut");
    let t0 = Instant::now();
    a.team_take_lock(rough).unwrap();
    a.poll_team(t0);
    delete(&mut a, rough).unwrap();
    a.edit("Notes", |p, _| {
        p.notes = "picture lock Friday".into();
        Ok(())
    })
    .unwrap();
    b.edit("Preset", |p, _| {
        p.settings.ingest.preset = "prores_proxy_half".into();
        Ok(())
    })
    .unwrap();
    for k in 1..6u64 {
        let now = t0 + Duration::from_secs(k);
        a.poll_team(now);
        b.poll_team(now);
    }
    assert!(b.project.item(rough).is_none(), "Ben no longer has the deleted sequence");
    assert_eq!(b.project.notes, "picture lock Friday");
    assert_eq!(a.project.settings.ingest.preset, "prores_proxy_half");
    let (da, db) = (shared_view(&a), shared_view(&b));
    assert_eq!(da.sequences, db.sequences);
    assert_eq!(da.shared, db.shared);
}

#[test]
fn failed_lock_request_is_told_or_asked_again_for_a_new_sequence() {
    let (mut s, fake, main, _) = team_demo();
    s.team_take_lock(main).unwrap();
    fake.lock().unwrap().queued.clear();
    fake.lock().unwrap().queued.push(LinkEvent::LockFailed { seq: main, message: "Can't reach InFocus Drive".into() });
    s.poll_team(Instant::now());
    assert_eq!(s.team_status(main), Some(SequenceStatus::Free));
    assert!(s.log.entries.iter().any(|e| e.message.contains("Could not take the lock on “Main Edit”")));
    // a sequence made here keeps asking until it gets its lock
    s.execute("file.newSequence", json!({"name": "Interview"})).unwrap();
    let seq = seq_named(&s, "Interview");
    fake.lock().unwrap().queued.clear();
    fake.lock().unwrap().queued.push(LinkEvent::LockFailed { seq, message: "offline".into() });
    let t0 = Instant::now();
    s.poll_team(t0);
    calls(&fake);
    s.poll_team(t0 + Duration::from_secs(6));
    assert!(calls(&fake).contains(&Call::Take(seq)));
    s.poll_team(t0 + Duration::from_secs(7));
    assert!(matches!(s.team_status(seq), Some(SequenceStatus::Editing { .. })));
}

#[test]
fn open_team_project_replaces_the_project_and_attaches() {
    let mut demo = Session::default();
    demo.execute("file.openDemoProject", json!({})).unwrap();
    let project = (*demo.project).clone();
    let main = seq_named(&demo, "Main Edit");
    let mut s = Session::default();
    s.execute("file.newProject", json!({"name": "Other"})).unwrap();
    let fake = Arc::new(Mutex::new(Fake::default()));
    let revs = project.sequences().map(|q| (q.id, 3)).collect();
    s.open_team_project(project, TeamState::new(Box::new(FakeLink(fake.clone())), "me".into(), 4, revs, 2, BTreeMap::new())).unwrap();
    assert!(s.project.item(main).is_some());
    assert_eq!(s.project.id_slot, 4);
    assert!(s.path.is_none() && !s.is_dirty());
    assert_eq!(s.team_status(main), Some(SequenceStatus::Free));
    s.poll_team(Instant::now());
    assert!(s.team.is_some(), "still attached after a poll");
}

#[test]
fn shared_post_past_a_teammates_revision_still_gets_their_changes() {
    // Upserts without bins merge on the Drive at any base, so this post can land after a
    // teammate's (rev 2) and come back as rev 3. The change feed then fetches the shared part at
    // rev 3, which holds the teammate's item: it must not be skipped as already seen.
    let (mut s, fake, _, _) = team_demo();
    let media = s.project.items.values().find(|i| i.as_media().is_some()).map(|i| i.id).unwrap();
    s.team_push_items(&[media]);
    let sent = calls(&fake);
    assert_eq!(shared_pushes(&sent).len(), 1, "{sent:?}");
    fake.lock().unwrap().queued = vec![LinkEvent::SharedPosted { rev: 3 }];
    s.poll_team(later(0));
    let mut shared = filmcraft_team::split(&s.project, &|_| None).unwrap().shared;
    let mut theirs = shared["items"][media.0.to_string()].clone();
    let new_id = (2u64 << filmcraft_project::ID_SLOT_SHIFT) + 1;
    theirs["id"] = json!(new_id);
    theirs["name"] = json!("from teammate");
    shared["items"][new_id.to_string()] = theirs;
    fake.lock().unwrap().queued.push(LinkEvent::SharedUpdated { rev: 3, shared });
    s.poll_team(later(1));
    assert!(s.project.item(ItemId(new_id)).is_some_and(|i| i.name == "from teammate"), "the teammate's item arrived");
    assert_eq!(s.team.as_ref().unwrap().shared_rev, 3);
}
