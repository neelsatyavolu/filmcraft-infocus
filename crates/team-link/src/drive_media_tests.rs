//! Footage put in the project's media folder from the Drive website: offered once no item uses
//! it on two checks, then Add to Project downloads it, imports it and shares its Drive path.

use std::sync::{Arc, Mutex};

use filmcraft_drive::mock::{MockServer, Reply, Req};
use filmcraft_drive::{MemoryStore, TokenStore};
use filmcraft_engine::Session;
use serde_json::json;

use crate::controller_tests::{config, poll_until, project_data};
use crate::members_tests::{stored_demo, write_wav};
use crate::tests::PID;
use crate::*;

const WEB_FILE: &str = "From the website/tone.wav";

fn drive(wav: Vec<u8>, lists: Arc<Mutex<u32>>, feed: bool) -> MockServer {
    let stored = stored_demo();
    MockServer::start(move |req: &Req| {
        let project = format!("/api/team/projects/{PID}");
        match (req.method.as_str(), req.path()) {
            ("GET", "/api/me") => {
                Reply::json(200, json!({"authenticated": true, "nas_username": "student1", "name": "Student One", "email": "s1@example.org"}))
            }
            ("GET", "/api/team/people") => Reply::json(200, json!({"people": []})),
            ("GET", "/api/team/projects") => Reply::json(
                200,
                json!([{"id": PID, "name": "Robotics", "folder": "Team Projects/Robotics.fcteam", "role": "member",
                        "members": [{"username": "student1", "name": "Student One", "online": true}]}]),
            ),
            ("GET", p) if p == project => Reply::json(200, project_data(&stored)),
            ("GET", p) if p == format!("{project}/media") => match req.query("path") {
                None => {
                    *lists.lock().unwrap() += 1;
                    Reply::json(200, json!({"files": [{"path": WEB_FILE, "size": wav.len(), "mtime": 1.0}]}))
                }
                Some(p) if p == WEB_FILE => Reply::Full(200, vec![("ETag".into(), "\"w1\"".into())], wav.clone()),
                Some(_) => Reply::json(404, json!({"detail": "Not found"})),
            },
            ("POST", p) if p.ends_with("/open") => Reply::json(200, json!({"ok": true, "last_open": 5.0})),
            ("GET", p) if p == format!("{project}/overview") => {
                Reply::json(200, json!({"id": PID, "members": [{"username": "student1", "name": "Student One", "online": false, "last_seen": 1700000000.0}]}))
            }
            ("GET", p) if p.ends_with("/events") && feed => {
                // a teammate's upload lands a moment after the feed connects
                let chunks = [": subscribed\n\n", "event: media_changed\ndata: {\"rev\":2,\"path\":\"x.mov\",\"by\":\"student2\"}\n\n"];
                let pings = std::iter::repeat_n(": ping\n\n", 200);
                Reply::Chunks(
                    200,
                    vec![("Content-Type".into(), "text/event-stream".into())],
                    chunks.into_iter().chain(pings).map(|c| c.as_bytes().to_vec()).collect(),
                    std::time::Duration::from_millis(30),
                )
            }
            ("GET", p) if p.ends_with("/events") => Reply::json(503, json!({"detail": "down"})),
            _ => Reply::json(404, json!({"detail": "x"})),
        }
    })
}

#[test]
fn web_footage_is_offered_then_added_with_its_drive_path() {
    let cache = tempfile::tempdir().unwrap();
    let src = cache.path().join("src.wav");
    write_wav(&src);
    let lists = Arc::new(Mutex::new(0));
    let server = drive(std::fs::read(&src).unwrap(), lists.clone(), false);
    let store = Arc::new(MemoryStore::new());
    store.save("ifd_test").unwrap();
    let mut c = TeamSessionController::new(config(&server.url, store, cache.path()));
    let mut s = Session::default();
    poll_until(&mut c, &mut s, |c, _| c.phase() == Phase::SignedIn && !c.projects().is_empty());
    assert_eq!(c.user_name(), Some("Student One"), "the name from the Drive, never the email");
    assert_eq!(c.projects()[0].members[0].online, Some(true));
    c.open(PID);
    poll_until(&mut c, &mut s, |_, s| s.team.is_some());
    // who is online comes from the overview, fetched with the media folder
    poll_until(&mut c, &mut s, |c, _| c.projects()[0].members[0].last_seen.is_some());
    assert_eq!(c.projects()[0].members[0].online, Some(false));
    // the first check only notes the file; the second offers it
    poll_until(&mut c, &mut s, |_, _| *lists.lock().unwrap() >= 1);
    poll_until(&mut c, &mut s, |c, _| c.busy().is_none());
    assert!(c.drive_only().is_empty(), "not offered after one check (a teammate's upload may be on its way)");
    c.refresh_media();
    poll_until(&mut c, &mut s, |c, _| !c.drive_only().is_empty());
    assert_eq!(c.drive_only()[0].0, WEB_FILE);

    c.add_drive_footage();
    poll_until(&mut c, &mut s, |_, s| s.project.items.values().any(|i| i.name == "tone.wav"));
    assert!(c.drive_only().is_empty(), "added: no longer offered");
    let doc = c.team_doc(&s).unwrap();
    let shared = doc.shared.to_string();
    assert!(shared.contains("drive:/Team Projects/Robotics Media/From the website/tone.wav"), "teammates get the Drive path: {shared}");
    // listed twice more: the file is used now, so it is never offered again
    for _ in 0..2 {
        let n = *lists.lock().unwrap();
        c.refresh_media();
        poll_until(&mut c, &mut s, |_, _| *lists.lock().unwrap() > n);
        for _ in 0..10 {
            c.poll(&mut s);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    assert!(c.drive_only().is_empty());
}

#[test]
fn a_footage_change_on_the_feed_lists_the_media_folder_again() {
    let cache = tempfile::tempdir().unwrap();
    let src = cache.path().join("src.wav");
    write_wav(&src);
    let lists = Arc::new(Mutex::new(0));
    let server = drive(std::fs::read(&src).unwrap(), lists.clone(), true);
    let store = Arc::new(MemoryStore::new());
    store.save("ifd_test").unwrap();
    let mut c = TeamSessionController::new(config(&server.url, store, cache.path()));
    let mut s = Session::default();
    poll_until(&mut c, &mut s, |c, _| c.phase() == Phase::SignedIn && !c.projects().is_empty());
    c.open(PID);
    poll_until(&mut c, &mut s, |_, _| *lists.lock().unwrap() >= 1);
    // the regular check is 30 s away: only the media_changed event can bring the second one
    poll_until(&mut c, &mut s, |_, _| *lists.lock().unwrap() >= 2);
}
