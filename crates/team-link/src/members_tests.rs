//! The controller's membership side against an in-process Drive: an invitation shows, joining
//! opens the project, invites and their cancelling, declining, leaving, the invite step after
//! creating, "Add footage…" and the list fetched again now and then.

use std::path::Path;
use std::sync::{Arc, Mutex};

use filmcraft_drive::mock::{MockServer, Reply, Req};
use filmcraft_drive::{MemoryStore, Role, TokenStore};
use filmcraft_engine::Session;
use serde_json::{Value, json};

use crate::controller_tests::{config, poll_until, project_data};
use crate::tests::PID;
use crate::*;

/// The demo project as `POST /api/team/projects` would have stored it.
pub(crate) fn stored_demo() -> Value {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let doc = filmcraft_team::split(&s.project, &|_| None).unwrap();
    let mut project = doc.shared;
    let items = project["items"].as_object_mut().unwrap();
    for (id, seq) in doc.sequences {
        items.insert(id.0.to_string(), seq);
    }
    project
}

/// A Drive where student1 is invited to project PID until joining.
fn drive() -> (MockServer, Arc<Mutex<bool>>) {
    let joined = Arc::new(Mutex::new(false));
    let j = joined.clone();
    let stored = stored_demo();
    let server = MockServer::start(move |req: &Req| {
        let project = format!("/api/team/projects/{PID}");
        let path = req.path().to_string();
        match (req.method.as_str(), path.as_str()) {
            ("GET", "/api/me") => Reply::json(200, json!({"authenticated": true, "nas_username": "student1"})),
            ("GET", "/api/team/people") => Reply::json(200, json!({"people": [{"username": "student2", "name": "Student Two"}]})),
            ("GET", "/api/team/projects") => {
                let role = if *j.lock().unwrap() { "member" } else { "invited" };
                Reply::json(
                    200,
                    json!([{"id": PID, "name": "Robotics", "folder": "Team Projects/Robotics.fcteam", "role": role,
                            "invited_by": "Student Two", "members": [{"username": "student2", "name": "Student Two"}],
                            "media_count": 2, "media_bytes": 2048}]),
                )
            }
            ("POST", p) if p == format!("{project}/join") => {
                *j.lock().unwrap() = true;
                Reply::json(200, json!({"id": PID, "slot": 3}))
            }
            ("GET", p) if p == project => Reply::json(200, project_data(&stored)),
            ("POST", p) if p.ends_with("/invites") => Reply::json(200, json!({"invites": [{"user": "student3", "by": "student1"}]})),
            ("DELETE", p) if p.contains("/invites/") => Reply::Full(204, Vec::new(), Vec::new()),
            ("POST", p) if p.ends_with("/decline") => Reply::Full(204, Vec::new(), Vec::new()),
            ("POST", p) if p.ends_with("/leave") => Reply::json(409, json!({"error": "last_member", "message": "You're the only member left."})),
            ("GET", p) if p.ends_with("/events") => Reply::json(503, json!({"detail": "down"})),
            _ => Reply::json(404, json!({"detail": "x"})),
        }
    });
    (server, joined)
}

fn signed_in(server: &MockServer, cache: &Path) -> (TeamSessionController, Session) {
    let store = Arc::new(MemoryStore::new());
    store.save("ifd_test").unwrap();
    let mut c = TeamSessionController::new(config(&server.url, store, cache));
    let mut s = Session::default();
    poll_until(&mut c, &mut s, |c, _| c.phase() == Phase::SignedIn && !c.projects().is_empty());
    assert!(c.people().is_empty(), "the Drive's people are only searched, never loaded whole");
    c.search_people("stu");
    poll_until(&mut c, &mut s, |c, _| !c.people().is_empty());
    (c, s)
}

#[test]
fn an_invitation_is_joined_and_the_project_opens() {
    let (server, joined) = drive();
    let cache = tempfile::tempdir().unwrap();
    let (mut c, mut s) = signed_in(&server, cache.path());
    let p = &c.projects()[0];
    assert_eq!((p.role, p.invited_by.as_deref(), p.media_count), (Role::Invited, Some("Student Two"), 2));
    assert_eq!(c.people()[0].shown(), "Student Two");

    c.join(PID);
    poll_until(&mut c, &mut s, |c, s| s.team.is_some() && c.projects().first().is_some_and(|p| p.role == Role::Member));
    assert!(*joined.lock().unwrap());
    assert_eq!(c.open_project().map(|o| o.id.as_str()), Some(PID));
    assert_eq!(s.project.id_slot, 2, "the open takes the slot the Drive gives this Mac");
    assert!(c.invite_prompt().is_none(), "joining is not creating: no invite step");
    let join = server.requests().into_iter().find(|r| r.path().ends_with("/join")).unwrap();
    assert_eq!(join.header(filmcraft_drive::DEVICE_HEADER), Some("Studio Mac"));
}

#[test]
fn invite_cancel_decline_and_a_refused_leave() {
    let (server, _) = drive();
    let cache = tempfile::tempdir().unwrap();
    let (mut c, mut s) = signed_in(&server, cache.path());
    let users: Vec<String> = (0..12).map(|i| format!("student{}", i + 3)).collect();
    c.invite(PID, &users);
    c.cancel_invite(PID, "student3");
    c.decline(PID);
    poll_until(&mut c, &mut s, |c, _| c.busy().is_none());
    assert_eq!(c.error(), None);
    let reqs = server.requests();
    let batches: Vec<usize> = reqs.iter().filter(|r| r.path().ends_with("/invites")).map(|r| r.json()["users"].as_array().map_or(0, Vec::len)).collect();
    assert_eq!(batches, vec![10, 2], "twelve names go in batches the Drive accepts");
    assert!(reqs.iter().any(|r| r.method == "DELETE" && r.path().ends_with("/invites/student3")));
    assert!(reqs.iter().any(|r| r.path().ends_with("/decline")));

    c.leave_team(&mut s, PID);
    poll_until(&mut c, &mut s, |c, _| c.busy().is_none());
    assert_eq!(c.error(), Some("You're the only member left."), "the Drive's own words, not a status code");
}

#[test]
fn footage_needs_an_open_project_and_real_files() {
    let (server, _) = drive();
    let cache = tempfile::tempdir().unwrap();
    let (mut c, mut s) = signed_in(&server, cache.path());
    let wav = cache.path().join("tone.wav");
    write_wav(&wav);
    let paths = vec![wav.to_string_lossy().into_owned()];
    assert!(c.add_footage(&mut s, &paths).unwrap_err().contains("Open a team project"));

    c.join(PID);
    poll_until(&mut c, &mut s, |_, s| s.team.is_some());
    assert!(c.add_footage(&mut s, &["/no/such/file.mov".into()]).unwrap_err().contains("file.mov"));
    assert_eq!(c.add_footage(&mut s, &[]).unwrap(), 0);
    let added = c.add_footage(&mut s, &paths).unwrap();
    assert_eq!(added, 1);
    assert!(s.project.items.values().any(|i| i.name == "tone.wav"), "imported into the team project");
    // the new file is this Mac's own media: it is queued to upload to the project's media folder
    poll_until(&mut c, &mut s, |c, _| c.uploads().is_some_and(|u| !u.pending().is_empty() || u.busy()));
}

#[test]
fn the_list_is_fetched_again_now_and_then() {
    let (server, _) = drive();
    let cache = tempfile::tempdir().unwrap();
    let (mut c, mut s) = signed_in(&server, cache.path());
    let lists = || server.requests().iter().filter(|r| r.method == "GET" && r.path() == "/api/team/projects").count();
    let before = lists();
    c.poll(&mut s);
    assert_eq!(lists(), before, "not before the interval");
    c.age_list();
    poll_until(&mut c, &mut s, |_, _| lists() > before);
}

/// A 0.1 s, 8 kHz mono 16-bit PCM WAV file.
pub(crate) fn write_wav(path: &Path) {
    let samples: Vec<u8> = (0..800u32).flat_map(|i| (((i % 40) as i16 - 20) * 500).to_le_bytes()).collect();
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&8000u32.to_le_bytes());
    b.extend_from_slice(&16000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    b.extend_from_slice(&samples);
    std::fs::write(path, b).unwrap();
}
