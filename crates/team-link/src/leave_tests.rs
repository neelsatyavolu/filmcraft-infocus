//! Leaving a team project (Close, Sign Out, Quit) waits for edits still going to the Drive, and
//! after a while asks whether to wait more or leave anyway.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use filmcraft_drive::MemoryStore;
use filmcraft_drive::mock::{MockServer, Reply, Req};
use filmcraft_engine::Session;
use filmcraft_engine::team::SequenceStatus;
use filmcraft_project::ItemId;
use serde_json::{Value, json};

use crate::controller_tests::{config, poll_until, project_data};
use crate::tests::PID;
use crate::*;

/// A Drive whose sequence saves take a while, or (`down`) never get through.
fn drive(down: Arc<AtomicBool>) -> MockServer {
    let stored: Arc<Mutex<Option<Value>>> = Arc::default();
    MockServer::start(move |req: &Req| {
        let path = req.path().to_string();
        let project = format!("/api/team/projects/{PID}");
        match (req.method.as_str(), path.as_str()) {
            ("POST", "/api/cli/token") => Reply::json(200, json!({"token": "ifd_new", "username": "student1"})),
            ("GET", "/api/me") => Reply::json(200, json!({"authenticated": true, "nas_username": "student1"})),
            ("GET", "/api/team/projects") => {
                Reply::json(200, json!([{"id": PID, "name": "Robotics", "folder": "Team Projects/Robotics.fcteam", "locks": {}, "last_open": 1.0}]))
            }
            ("POST", "/api/team/projects") => {
                *stored.lock().unwrap() = Some(req.json()["project"].clone());
                Reply::json(200, json!({"id": PID, "slot": 2}))
            }
            ("GET", p) if p == project => Reply::json(200, project_data(&stored.lock().unwrap().clone().unwrap())),
            ("GET", p) if p.ends_with("/events") => Reply::json(503, json!({"detail": "down"})),
            ("POST", p) if p.ends_with("/lock") => Reply::json(200, json!({"token": "t", "expires_at": 9.0})),
            ("PUT", _) if down.load(Ordering::SeqCst) => Reply::json(503, json!({"detail": "busy"})),
            ("PUT", _) => Reply::Slow(Duration::from_millis(700), Box::new(Reply::json(200, json!({"rev": 2})))),
            ("POST", p) if p.ends_with("/open") => Reply::json(200, json!({"ok": true, "last_open": 5.0})),
            _ => Reply::json(404, json!({"detail": "x"})),
        }
    })
}

/// Signed in, the demo made a team project, "Main Edit" held with an edit not on the Drive yet.
fn editing(server: &MockServer, cache: &std::path::Path) -> (TeamSessionController, Session, ItemId) {
    let mut c = TeamSessionController::new(config(&server.url, Arc::new(MemoryStore::new()), cache));
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    poll_until(&mut c, &mut s, |c, _| c.busy().is_none());
    c.sign_in();
    poll_until(&mut c, &mut s, |c, _| c.phase() == Phase::SignedIn);
    c.create_from_current(&s, "Robotics");
    poll_until(&mut c, &mut s, |_, s| s.team.is_some());
    let main = s.project.sequences().find(|q| q.name == "Main Edit").unwrap().id;
    s.team_take_lock(main).unwrap();
    frames(&mut c, &mut s, |_, s| matches!(s.team_status(main), Some(SequenceStatus::Editing { .. })));
    s.state.active_sequence = Some(main);
    s.execute("markers.add", json!({"name": "unsaved"})).unwrap();
    (c, s, main)
}

/// Runs frames (the session's team side, then the controller) until `done`.
fn frames(c: &mut TeamSessionController, s: &mut Session, mut done: impl FnMut(&TeamSessionController, &Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done(c, s) {
        assert!(Instant::now() < deadline, "timed out (leaving: {:?})", c.leaving());
        s.poll_team(web_time::Instant::now());
        c.poll(s);
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn close_waits_until_the_edits_are_on_the_drive() {
    let down = Arc::new(AtomicBool::new(false));
    let server = drive(down);
    let cache = tempfile::tempdir().unwrap();
    let (mut c, mut s, main) = editing(&server, cache.path());
    c.leave(&mut s, Leave::Close);
    assert!(s.team.is_some(), "still open while the edit is on its way");
    assert_eq!(c.leaving(), Some((Leave::Close, false)));
    frames(&mut c, &mut s, |_, s| s.team.is_none());
    assert_eq!(c.leaving(), None);
    assert!(c.open_project().is_none());
    let reqs = server.requests();
    let put = reqs.iter().position(|r| r.method == "PUT" && r.path().ends_with(&format!("/sequences/{}", main.0))).expect("the edit was saved");
    let release = reqs.iter().position(|r| r.method == "DELETE" && r.path().ends_with("/lock")).expect("then the lock released");
    assert!(put < release);
}

#[test]
fn leaving_while_the_drive_cant_be_reached_asks_and_can_leave_anyway() {
    let down = Arc::new(AtomicBool::new(true));
    let server = drive(down.clone());
    let cache = tempfile::tempdir().unwrap();
    let (mut c, mut s, _) = editing(&server, cache.path());
    c.set_leave_wait(Duration::from_millis(300));
    c.leave(&mut s, Leave::Quit);
    frames(&mut c, &mut s, |c, _| c.leaving() == Some((Leave::Quit, true)));
    assert!(s.team.is_some() && !c.take_quit(), "nothing left yet: the member decides");
    // Wait: another round
    c.leave_wait(&mut s);
    assert_eq!(c.leaving(), Some((Leave::Quit, false)));
    frames(&mut c, &mut s, |c, _| c.leaving() == Some((Leave::Quit, true)));
    c.leave_anyway(&mut s);
    assert!(s.team.is_none() && c.open_project().is_none());
    assert!(c.take_quit(), "the app may quit now");
    assert!(!c.take_quit(), "once");
}

#[test]
fn leaving_with_nothing_unsent_is_immediate_and_cancel_stays() {
    let down = Arc::new(AtomicBool::new(true));
    let server = drive(down);
    let cache = tempfile::tempdir().unwrap();
    let (mut c, mut s, _) = editing(&server, cache.path());
    c.leave(&mut s, Leave::SignOut);
    assert!(c.leaving().is_some());
    c.cancel_leave();
    assert_eq!(c.leaving(), None);
    assert!(s.team.is_some(), "Cancel keeps the project open");
    assert_eq!(c.phase(), Phase::SignedIn);
    let mut plain = Session::default();
    let mut other = TeamSessionController::new(config(&server.url, Arc::new(MemoryStore::new()), cache.path()));
    other.leave(&mut plain, Leave::Quit);
    assert!(other.take_quit(), "no team project: quitting doesn't wait");
}
