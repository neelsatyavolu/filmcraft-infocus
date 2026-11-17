//! The controller against an in-process Drive: sign in, list, create from the current project,
//! open, keep, export guard and sign out.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use filmcraft_drive::mock::{MockServer, Reply, Req};
use filmcraft_drive::{AppInfo, DriveConfig, LoginOptions, MemoryStore, TokenStore};
use filmcraft_engine::Session;
use serde_json::{Value, json};

use crate::tests::PID;
use crate::*;

/// A Drive with one team project, created through `POST /api/team/projects`.
fn drive(me_ok: bool) -> (MockServer, Arc<Mutex<Option<Value>>>) {
    let stored: Arc<Mutex<Option<Value>>> = Arc::default();
    let st = stored.clone();
    let server = MockServer::start(move |req: &Req| {
        let path = req.path().to_string();
        let project = format!("/api/team/projects/{PID}");
        match (req.method.as_str(), path.as_str()) {
            ("POST", "/api/cli/token") => Reply::json(200, json!({"token": "ifd_new", "username": "student1"})),
            ("GET", "/api/me") if me_ok => Reply::json(200, json!({"authenticated": true, "nas_username": "student1"})),
            ("GET", "/api/me") => Reply::json(200, json!({"authenticated": false})),
            ("GET", "/api/team/projects") => Reply::json(
                200,
                json!([{"id": PID, "name": "Robotics", "folder": "Team Projects/Robotics.fcteam", "locks": {}, "last_open": 1.0,
                        "cleanup": {"notice_sent_at": 1.0, "move_after": 2.0, "keep": false}}]),
            ),
            ("POST", "/api/team/projects") => {
                *st.lock().unwrap() = Some(req.json()["project"].clone());
                Reply::json(200, json!({"id": PID, "slot": 2}))
            }
            ("GET", p) if p == project => match st.lock().unwrap().clone() {
                Some(proj) => Reply::json(200, project_data(&proj)),
                None => Reply::json(404, json!({"detail": "x"})),
            },
            ("GET", p) if p.ends_with("/events") => Reply::json(503, json!({"detail": "down"})),
            ("POST", p) if p.ends_with("/open") => Reply::json(200, json!({"ok": true, "last_open": 5.0})),
            ("POST", p) if p.starts_with("/api/team/exports/") => Reply::json(200, json!({"ok": true})),
            ("POST", p) if p.ends_with("/lock") => Reply::json(200, json!({"token": "t", "expires_at": 9.0})),
            _ => Reply::json(404, json!({"detail": "x"})),
        }
    });
    (server, stored)
}

/// The project as the Drive stores and returns it: sequences split off at revision 1.
pub(crate) fn project_data(project: &Value) -> Value {
    let mut shared = project.clone();
    let mut sequences = serde_json::Map::new();
    if let Some(items) = shared["items"].as_object_mut() {
        let ids: Vec<String> = items.iter().filter(|(_, v)| v["kind"].get("Sequence").is_some()).map(|(k, _)| k.clone()).collect();
        for id in ids {
            let item = items.remove(&id).unwrap();
            sequences.insert(id, json!({"rev": 1, "json": item}));
        }
    }
    json!({"id": PID, "name": "Robotics", "meta": {}, "shared": shared, "sequences": sequences, "shared_rev": 1, "locks": {}, "slot": 2,
           "cleanup": {"notice_sent_at": 1.0, "move_after": 2.0, "keep": false}})
}

/// Plays the browser: approves the sign-in at once.
fn browser(link: &str) {
    let u = url::Url::parse(link).unwrap();
    let q = |k: &str| u.query_pairs().find(|(key, _)| key == k).unwrap().1.into_owned();
    let (port, state) = (q("port"), q("state"));
    std::thread::spawn(move || {
        let _ = ureq::get(format!("http://127.0.0.1:{port}/callback?code=c1&state={state}")).call();
    });
}

pub(crate) fn config(url: &str, store: Arc<MemoryStore>, cache: &Path) -> ControllerConfig {
    ControllerConfig {
        drive: Ok(DriveConfig::new(url, None).unwrap()),
        app: AppInfo::new("0.20.1", 17).unwrap().with_device("Studio Mac"),
        store,
        cache_dir: cache.to_path_buf(),
        open_browser: Arc::new(browser),
        login: LoginOptions { timeout: Duration::from_secs(10), ..LoginOptions::default() },
    }
}

pub(crate) fn poll_until(c: &mut TeamSessionController, s: &mut Session, mut done: impl FnMut(&TeamSessionController, &Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        c.poll(s);
        if done(c, s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out (busy: {:?}, error: {:?})", c.busy(), c.error());
}

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

#[test]
fn sign_in_create_open_keep_export_and_sign_out() {
    let (server, stored) = drive(true);
    let cache = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    let mut c = TeamSessionController::new(config(&server.url, store.clone(), cache.path()));
    let mut s = demo();
    poll_until(&mut c, &mut s, |c, _| c.busy().is_none());
    assert_eq!(c.phase(), Phase::SignedOut, "no stored token");

    c.sign_in();
    assert_eq!(c.phase(), Phase::Connecting);
    poll_until(&mut c, &mut s, |c, _| c.phase() == Phase::SignedIn && !c.projects().is_empty());
    assert_eq!(c.user(), Some("student1"));
    assert_eq!(store.load().unwrap().as_deref(), Some("ifd_new"));
    assert!(c.projects()[0].cleanup.as_ref().is_some_and(|k| k.move_after.is_some()), "the cleanup notice shows in the list");

    // create from the current project: it stays open, now as a team project at the Drive's slot
    let main = s.project.sequences().find(|q| q.name == "Main Edit").unwrap().id;
    c.create_from_current(&s, "Robotics");
    poll_until(&mut c, &mut s, |_, s| s.team.is_some());
    assert_eq!(c.open_project().map(|o| o.id.as_str()), Some(PID));
    assert_eq!(s.project.id_slot, 2);
    assert!(s.project.item(main).is_some());
    let sent = stored.lock().unwrap().clone().unwrap();
    assert!(sent["items"][main.0.to_string()]["kind"].get("Sequence").is_some(), "sequences go with the project");
    // the session's link works: the lock comes back through the worker thread
    s.team_take_lock(main).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(s.team_status(main), Some(filmcraft_engine::team::SequenceStatus::Editing { .. })) {
        assert!(Instant::now() < deadline, "the lock never came");
        s.poll_team(web_time::Instant::now());
        c.poll(&mut s);
        std::thread::sleep(Duration::from_millis(20));
    }

    // open it in another session, from the Drive
    let mut other = Session::default();
    c.open(PID);
    poll_until(&mut c, &mut other, |_, o| o.team.is_some());
    assert!(other.project.item(main).is_some(), "merged from the Drive");
    assert_eq!(other.project.id_slot, 2);
    assert!(other.path.is_none());

    // keep, and the export guard
    c.keep(PID);
    c.export_begin();
    assert!(c.exporting() && c.media().unwrap().is_paused());
    c.export_end();
    assert!(!c.exporting() && !c.media().unwrap().is_paused());
    poll_until(&mut c, &mut other, |c, _| c.busy().is_none());
    let reqs = server.requests();
    for route in [format!("/api/team/projects/{PID}/open"), format!("/api/team/exports/{PID}/begin"), format!("/api/team/exports/{PID}/end")] {
        assert!(reqs.iter().any(|r| r.method == "POST" && r.path() == route), "{route}");
    }
    assert!(c.export_missing(&other, &[main]).is_empty(), "the demo's media is generated, nothing to download");

    c.sign_out(&mut other);
    assert!(other.team.is_none());
    assert_eq!(c.phase(), Phase::SignedOut);
    poll_until(&mut c, &mut other, |c, _| c.busy().is_none());
    assert_eq!(store.load().unwrap(), None, "the token is gone from this Mac");
}

#[test]
fn stored_token_the_drive_refuses_is_signed_out() {
    let (server, _) = drive(false);
    let cache = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new());
    store.save("ifd_old").unwrap();
    let mut c = TeamSessionController::new(config(&server.url, store, cache.path()));
    assert_eq!(c.phase(), Phase::Connecting);
    let mut s = Session::default();
    poll_until(&mut c, &mut s, |c, _| c.busy().is_none());
    assert_eq!(c.phase(), Phase::SignedOut);
    assert!(c.error().is_some_and(|e| e.contains("Sign in")), "{:?}", c.error());
    // nothing to do while signed out
    c.open(PID);
    assert!(c.busy().is_none() && c.error().is_some_and(|e| e.contains("Sign in")));
}

#[test]
fn missing_drive_settings_are_an_error_not_a_crash() {
    let cache = tempfile::tempdir().unwrap();
    let mut cfg = config("http://127.0.0.1:9", Arc::new(MemoryStore::new()), cache.path());
    cfg.drive = Err("InFocus Drive settings: no config file".into());
    let mut c = TeamSessionController::new(cfg);
    assert_eq!(c.phase(), Phase::SignedOut);
    c.sign_in();
    assert_eq!(c.error(), Some("InFocus Drive settings: no config file"));
    let mut s = Session::default();
    c.poll(&mut s);
    assert!(c.open_project().is_none() && !c.feed_live());
}
