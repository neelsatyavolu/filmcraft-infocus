//! The link when answers go missing: retried saves and deletes whose first try reached the Drive,
//! a feed refetch that failed, a lock taken after missing a teammate's save, and refusals that
//! trying again won't fix.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use filmcraft_drive::DriveError;
use filmcraft_drive::mock::{MockServer, Reply, Req};
use filmcraft_engine::team::{LinkError, LinkEvent, TeamLink};
use filmcraft_project::ItemId;
use serde_json::{Value, json};

use crate::tests::{PID, client, events, known, project};
use crate::*;

/// A Drive holding sequence 7 (lock `tok7`) whose answer to the first PUT and the first DELETE
/// never arrives, though the Drive did both.
struct Lossy {
    rev: u64,
    json: Value,
    deleted: bool,
    puts: usize,
    deletes: usize,
}

fn lossy_server(state: Arc<Mutex<Lossy>>) -> MockServer {
    MockServer::start(move |req: &Req| {
        let mut st = state.lock().unwrap();
        let p = req.path().to_string();
        match (req.method.as_str(), p.as_str()) {
            ("POST", p) if p.ends_with("/sequences/7/lock") => Reply::json(200, json!({"token": "tok7", "expires_at": 10.0})),
            ("PUT", p) if p.ends_with("/sequences/7") => {
                let body = req.json();
                if body["base_rev"].as_u64() != Some(st.rev) {
                    return Reply::json(409, json!({"error": "stale", "message": "x", "rev": st.rev}));
                }
                st.rev += 1;
                st.json = body["json"].clone();
                st.puts += 1;
                let answer = json!({"rev": st.rev}).to_string().into_bytes();
                // the first answer is cut off: the Mac sees a network error
                if st.puts == 1 { Reply::Truncated(200, vec![], answer, 3) } else { Reply::Full(200, vec![], answer) }
            }
            ("DELETE", p) if p.ends_with("/sequences/7") => {
                if st.deleted {
                    return Reply::json(404, json!({"detail": "Sequence not found"}));
                }
                st.deleted = true;
                st.rev += 1;
                st.deletes += 1;
                Reply::Truncated(200, vec![], json!({"rev": st.rev}).to_string().into_bytes(), 3)
            }
            ("GET", p) if p.ends_with("/sequences/7") => {
                if st.deleted {
                    return Reply::json(404, json!({"detail": "Sequence not found"}));
                }
                Reply::json(200, json!({"seq": 7, "rev": st.rev, "json": st.json}))
            }
            ("GET", p) if p.ends_with("/events") => Reply::json(503, json!({"detail": "down"})),
            ("GET", _) => Reply::json(200, project(&[(7, st.rev)], 0)),
            _ => Reply::json(404, json!({"detail": "x"})),
        }
    })
}

fn lossy() -> (Arc<Mutex<Lossy>>, MockServer, DriveTeamLink) {
    let state = Arc::new(Mutex::new(Lossy { rev: 3, json: json!({"id": 7, "v": 0}), deleted: false, puts: 0, deletes: 0 }));
    let server = lossy_server(state.clone());
    let mut link = DriveTeamLink::start(client(&server.url, "ifd_test"), PID, known(&[(7, 3)], 0), Arc::default());
    link.take_lock(ItemId(7));
    let got = events(&mut link, 2);
    assert!(got.contains(&LinkEvent::LockTaken { seq: ItemId(7) }), "{got:?}");
    (state, server, link)
}

fn put_failed_on_the_network(got: &[LinkEvent]) -> bool {
    matches!(got, [LinkEvent::SequencePutFailed { seq: ItemId(7), error: LinkError::Network(_) }])
}

#[test]
fn retried_save_whose_first_try_landed_is_accepted() {
    let (_state, _server, mut link) = lossy();
    link.put_sequence(ItemId(7), 3, json!({"id": 7, "v": 1}));
    let got = events(&mut link, 1);
    assert!(put_failed_on_the_network(&got), "{got:?}");
    // the session tries again from the same base: the Drive already has exactly this
    link.put_sequence(ItemId(7), 3, json!({"id": 7, "v": 1}));
    assert_eq!(events(&mut link, 1), vec![LinkEvent::SequencePut { seq: ItemId(7), rev: 4 }]);
}

#[test]
fn retried_save_with_newer_edits_goes_on_top_of_the_lost_one() {
    let (state, _server, mut link) = lossy();
    link.put_sequence(ItemId(7), 3, json!({"id": 7, "v": 1}));
    assert!(put_failed_on_the_network(&events(&mut link, 1)));
    // more edits meanwhile: the retry sends the newer version
    link.put_sequence(ItemId(7), 3, json!({"id": 7, "v": 2}));
    assert_eq!(events(&mut link, 1), vec![LinkEvent::SequencePut { seq: ItemId(7), rev: 5 }]);
    assert_eq!(state.lock().unwrap().json, json!({"id": 7, "v": 2}));
}

#[test]
fn stale_save_without_a_lost_answer_is_still_stale() {
    let (state, _server, mut link) = lossy();
    // a version this Mac never sent
    state.lock().unwrap().rev = 9;
    link.put_sequence(ItemId(7), 3, json!({"id": 7, "v": 1}));
    assert_eq!(events(&mut link, 1), vec![LinkEvent::SequencePutFailed { seq: ItemId(7), error: LinkError::Stale { rev: 9 } }]);
}

#[test]
fn retried_delete_whose_first_try_landed_is_done() {
    let (state, _server, mut link) = lossy();
    link.delete_sequence(ItemId(7), 3);
    let got = events(&mut link, 1);
    assert!(matches!(&got[..], [LinkEvent::SequenceDeleteFailed { seq: ItemId(7), error: LinkError::Network(_) }]), "{got:?}");
    link.delete_sequence(ItemId(7), 3);
    assert_eq!(events(&mut link, 1), vec![LinkEvent::SequenceDeleted { seq: ItemId(7), rev: 4 }]);
    assert_eq!(state.lock().unwrap().deletes, 1);
}

#[test]
fn taking_a_lock_brings_the_drives_latest_version_first() {
    // a teammate's save this Mac missed (a refetch failed): the first push would be stale
    let server = MockServer::start(|req: &Req| {
        let p = req.path().to_string();
        match (req.method.as_str(), p.as_str()) {
            ("POST", p) if p.ends_with("/lock") => Reply::json(200, json!({"token": "tok", "expires_at": 10.0})),
            ("GET", p) if p.ends_with("/sequences/7") => Reply::json(200, json!({"seq": 7, "rev": 5, "json": {"id": 7, "name": "theirs"}})),
            ("GET", p) if p.ends_with("/sequences/8") => Reply::json(404, json!({"detail": "Sequence not found"})),
            ("GET", p) if p.ends_with("/events") => Reply::json(503, json!({"detail": "down"})),
            ("GET", _) => Reply::json(200, project(&[(7, 3)], 0)),
            _ => Reply::json(404, json!({"detail": "x"})),
        }
    });
    let mut link = DriveTeamLink::start(client(&server.url, "ifd_test"), PID, known(&[(7, 5)], 0), Arc::default());
    link.take_lock(ItemId(7));
    let got = events(&mut link, 2);
    assert!(matches!(&got[0], LinkEvent::SequenceUpdated { seq: ItemId(7), rev: 5, json, .. } if json["name"] == "theirs"), "{got:?}");
    assert_eq!(got[1], LinkEvent::LockTaken { seq: ItemId(7) });
    // a sequence made on this Mac isn't on the Drive yet: just the lock
    link.take_lock(ItemId(8));
    assert_eq!(events(&mut link, 1), vec![LinkEvent::LockTaken { seq: ItemId(8) }]);
}

#[test]
fn failed_refetch_is_made_up_with_the_whole_project() {
    let wholes = Arc::new(AtomicUsize::new(0));
    let w = wholes.clone();
    let server = MockServer::start(move |req: &Req| {
        let p = req.path().to_string();
        if p.ends_with("/events") {
            let event = "event: sequence_updated\ndata: {\"seq\":7,\"rev\":2,\"by\":\"student2\"}\n\n";
            // the stream stays up long after the event, so reconnecting can't be what fixes it
            let pings = std::iter::repeat_n(": ping\n\n", 300);
            return Reply::Chunks(
                200,
                vec![("Content-Type".into(), "text/event-stream".into())],
                std::iter::once(event).chain(pings).map(|c| c.as_bytes().to_vec()).collect(),
                Duration::from_millis(50),
            );
        }
        if p.ends_with("/sequences/7") {
            // the light refetch fails
            return Reply::Truncated(200, vec![], json!({"seq": 7, "rev": 2, "json": {}}).to_string().into_bytes(), 3);
        }
        // the catch-up at connect still has the old version
        if w.fetch_add(1, Ordering::SeqCst) == 0 { Reply::json(200, project(&[(7, 1)], 0)) } else { Reply::json(200, project(&[(7, 2)], 0)) }
    });
    let mut link = DriveTeamLink::start(client(&server.url, "ifd_test"), PID, known(&[(7, 1)], 0), Arc::default());
    let got = events(&mut link, 1);
    assert!(got.iter().any(|e| matches!(e, LinkEvent::SequenceUpdated { seq: ItemId(7), rev: 2, .. })), "{got:?}");
}

#[test]
fn refusals_trying_again_wont_fix_stop_syncing() {
    for e in [DriveError::NoSlots("x".into()), DriveError::SchemaUnsupported { max_schema_version: 16 }, DriveError::TooLarge("x".into())] {
        assert!(matches!(link_error(&e), LinkError::Refused(m) if m == e.to_string()), "{e:?}");
    }
    assert!(matches!(link_error(&DriveError::TooManyStreams("x".into())), LinkError::Network(_)));
}
