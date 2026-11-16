//! The link, the change feed and the downloader against an in-process HTTP server.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use filmcraft_drive::mock::{MockServer, Reply, Req};
use filmcraft_drive::{AppInfo, DriveClient, DriveConfig, partial_path};
use filmcraft_engine::team::{LinkError, LinkEvent, SharedPush, TeamLink};
use filmcraft_engine::team_media::{DownloadError, Downloader};
use filmcraft_project::{ItemId, MediaRef};
use filmcraft_team::cache::CacheIndex;
use serde_json::{Value, json};

use crate::*;

pub(crate) const PID: &str = "0123456789abcdef0123456789abcdef";

pub(crate) fn client(url: &str, token: &str) -> Arc<DriveClient> {
    let app = AppInfo::new("0.20.1", 17).unwrap().with_device("Studio Mac");
    Arc::new(DriveClient::connect(&DriveConfig::new(url, None).unwrap(), token, app).unwrap())
}

/// `GET /api/team/projects/{id}` with the given sequence revisions and shared revision.
fn project(seqs: &[(u64, u64)], shared_rev: u64) -> Value {
    let sequences: BTreeMap<String, Value> =
        seqs.iter().map(|(id, rev)| (id.to_string(), json!({"rev": rev, "json": {"id": id, "name": format!("seq {id} v{rev}")}}))).collect();
    json!({"id": PID, "name": "A", "meta": {}, "shared": {"items": {}}, "sequences": sequences, "shared_rev": shared_rev, "locks": {}, "slot": 1})
}

fn known(seqs: &[(u64, u64)], shared_rev: u64) -> FeedState {
    FeedState { sequences: seqs.iter().copied().collect(), shared_rev, locks: BTreeMap::new() }
}

/// Polls the link until `n` events came or 10 s passed.
fn events(link: &mut DriveTeamLink, n: usize) -> Vec<LinkEvent> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    while out.len() < n && Instant::now() < deadline {
        out.extend(link.poll());
        std::thread::sleep(Duration::from_millis(20));
    }
    out
}

fn until(deadline_s: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(deadline_s);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn lock_server() -> MockServer {
    MockServer::start(|req: &Req| {
        let p = req.path().to_string();
        match (req.method.as_str(), p.as_str()) {
            ("POST", p) if p.ends_with("/sequences/7/lock") => Reply::json(200, json!({"token": "tok7", "expires_at": 10.0})),
            ("POST", p) if p.ends_with("/sequences/8/lock") => {
                Reply::json(409, json!({"error": "lock_held", "message": "x", "holder": "student2", "expires_at": 9.0}))
            }
            ("POST", p) if p.ends_with("/lock/heartbeat") => Reply::json(200, json!({"expires_at": 30.0})),
            ("PUT", p) if p.ends_with("/sequences/7") => Reply::json(200, json!({"rev": 4})),
            ("DELETE", p) if p.ends_with("/sequences/7") => Reply::json(200, json!({"rev": 5})),
            ("POST", p) if p.ends_with("/shared") => Reply::json(409, json!({"error": "stale", "message": "x", "rev": 9})),
            ("GET", p) if p.ends_with("/events") => Reply::json(503, json!({"detail": "down"})),
            ("GET", _) => Reply::json(200, project(&[(7, 3)], 0)),
            _ => Reply::json(404, json!({"detail": "x"})),
        }
    })
}

#[test]
fn link_requests_carry_tokens_and_map_answers() {
    let server = lock_server();
    let mut link = DriveTeamLink::start(client(&server.url, "ifd_test"), PID, known(&[(7, 3)], 0), Arc::default());
    link.take_lock(ItemId(7));
    link.take_lock(ItemId(8));
    link.put_sequence(ItemId(7), 3, json!({"id": 7}));
    link.heartbeat(ItemId(7));
    link.post_shared(SharedPush { base_rev: 1, bins: Some(json!({})), ..Default::default() });
    link.delete_sequence(ItemId(7), 4);
    link.put_sequence(ItemId(7), 5, json!({"id": 7}));
    let got = events(&mut link, 7);
    assert_eq!(
        got,
        vec![
            LinkEvent::LockTaken { seq: ItemId(7) },
            LinkEvent::LockRefused { seq: ItemId(8), holder: "student2".into() },
            LinkEvent::SequencePut { seq: ItemId(7), rev: 4 },
            LinkEvent::HeartbeatOk { seq: ItemId(7) },
            LinkEvent::SharedPostFailed { error: LinkError::Stale { rev: 9 } },
            LinkEvent::SequenceDeleted { seq: ItemId(7), rev: 5 },
            LinkEvent::SequencePutFailed { seq: ItemId(7), error: LinkError::LockLost },
        ],
        "the lock went with the deleted sequence"
    );
    let reqs = server.requests();
    let put = reqs.iter().find(|r| r.method == "PUT").unwrap();
    assert_eq!((put.json()["token"].clone(), put.json()["base_rev"].clone()), (json!("tok7"), json!(3)));
    assert!(reqs.iter().filter(|r| r.path().contains("/lock")).all(|r| r.header("x-drive-device") == Some("Studio Mac")));
    let del = reqs.iter().find(|r| r.method == "DELETE").unwrap();
    assert_eq!(del.json(), json!({"token": "tok7", "base_rev": 4}));
}

#[test]
fn unreachable_drive_is_a_network_error_and_lock_failure() {
    // a port nothing listens on
    let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut link = DriveTeamLink::start(client(&format!("http://{addr}"), "ifd_test"), PID, FeedState::default(), Arc::default());
    link.take_lock(ItemId(7));
    let got = events(&mut link, 1);
    assert!(matches!(&got[..], [LinkEvent::LockFailed { seq: ItemId(7), message }] if message.contains("reach")), "{got:?}");
    assert!(link.status().last_error().is_some());
    assert!(!link.status().feed_live());
}

#[test]
fn errors_map_for_the_engine() {
    use filmcraft_drive::DriveError;
    let own = DriveError::HeldBy { holder: Some("student1".into()), expires_at: 0.0, device: Some("Lab Mac".into()) };
    assert_eq!(link_error(&own), LinkError::HeldBy { holder: "You on Lab Mac".into() });
    let LinkError::HeldBy { holder } = link_error(&own) else { panic!("held") };
    assert_eq!(filmcraft_engine::team::editing_message(&holder, "this"), "You're editing this on Lab Mac");
    assert_eq!(link_error(&DriveError::ItemInUse { item: 4 }), LinkError::ItemInUse { item: ItemId(4) });
    assert_eq!(link_error(&DriveError::FilesChanged("x".into())), LinkError::FilesChanged);
    assert!(matches!(link_error(&DriveError::Unauthorized("x".into())), LinkError::Network(_)));
}

#[test]
fn feed_fetches_on_events_and_reconnects() {
    let gets = Arc::new(AtomicUsize::new(0));
    let streams = Arc::new(AtomicUsize::new(0));
    let (g, st) = (gets.clone(), streams.clone());
    let server = MockServer::start(move |req: &Req| {
        if req.path().ends_with("/events") {
            st.fetch_add(1, Ordering::SeqCst);
            let chunks = [
                ": subscribed\n\n",
                "event: lock_changed\ndata: {\"seq\":2,\"holder\":\"student2\"}\n\n",
                "event: sequence_updated\ndata: {\"seq\":7,\"rev\":2,\"by\":\"student2\"}\n\n",
                "event: sequence_deleted\ndata: {\"seq\":9,\"rev\":4,\"by\":\"student2\"}\n\n",
                "event: shared_updated\ndata: {\"rev\":5}\n\n",
            ];
            // keep-alives hold the stream open a while after the events
            let pings = std::iter::repeat_n(": ping\n\n", 20);
            return Reply::Chunks(
                200,
                vec![("Content-Type".into(), "text/event-stream".into())],
                chunks.into_iter().chain(pings).map(|c| c.as_bytes().to_vec()).collect(),
                Duration::from_millis(30),
            );
        }
        // a teammate's save is fetched alone, by the routes that don't record an open
        if req.path().ends_with("/sequences/7") {
            return Reply::json(200, json!({"seq": 7, "rev": 2, "json": {"id": 7, "name": "seq 7 v2"}}));
        }
        if req.path().ends_with("/shared") {
            return Reply::json(200, json!({"rev": 5, "shared": {"items": {}}}));
        }
        // the first fetch (catching up at connect) still has the old parts
        if g.fetch_add(1, Ordering::SeqCst) == 0 { Reply::json(200, project(&[(7, 1), (9, 3)], 0)) } else { Reply::json(200, project(&[(7, 2)], 0)) }
    });
    let mut link = DriveTeamLink::start(client(&server.url, "ifd_test"), PID, known(&[(7, 1), (9, 3)], 0), Arc::default());
    let got = events(&mut link, 4);
    assert_eq!(gets.load(Ordering::SeqCst), 1, "the whole project only to catch up at connect");
    assert!(got.contains(&LinkEvent::LockChanged { seq: ItemId(2), holder: Some("student2".into()) }), "{got:?}");
    assert!(
        got.iter().any(|e| matches!(e, LinkEvent::SequenceUpdated { seq: ItemId(7), rev: 2, by, json } if by == "student2" && json["name"] == "seq 7 v2")),
        "{got:?}"
    );
    assert!(got.iter().any(|e| matches!(e, LinkEvent::SequenceRemoved { seq: ItemId(9), rev: 4, .. })), "{got:?}");
    assert!(got.iter().any(|e| matches!(e, LinkEvent::SharedUpdated { rev: 5, .. })), "{got:?}");
    let reqs = server.requests();
    assert!(reqs.iter().any(|r| r.path() == format!("/api/team/projects/{PID}/sequences/7")));
    assert!(reqs.iter().any(|r| r.path() == format!("/api/team/projects/{PID}/shared")));

    assert!(until(10, || streams.load(Ordering::SeqCst) >= 2), "the feed reconnects after the stream ends");
}

#[test]
fn feed_polls_while_the_stream_is_down() {
    let server =
        MockServer::start(
            |req: &Req| {
                if req.path().ends_with("/events") { Reply::json(503, json!({"detail": "down"})) } else { Reply::json(200, project(&[(7, 1)], 3)) }
            },
        );
    let mut link = DriveTeamLink::start(client(&server.url, "ifd_test"), PID, known(&[(7, 1)], 1), Arc::default());
    let got = events(&mut link, 1);
    assert!(matches!(&got[..], [LinkEvent::SharedUpdated { rev: 3, .. }]), "{got:?}");
    assert!(!link.status().feed_live());
    assert!(link.status().last_error().is_some_and(|e| e.contains("Live updates")));
}

#[test]
fn link_maps_media_through_the_cache_index() {
    let index = Arc::new(Mutex::new(CacheIndex::default()));
    let link = DriveTeamLink::start(client("http://127.0.0.1:9", "ifd_test"), PID, FeedState::default(), index);
    let on_drive = MediaRef::File { path: "drive:/Team Projects/A Media/clip.mov".into() };
    assert_eq!(link.media_ref(&on_drive).as_deref(), Some("drive:/Team Projects/A Media/clip.mov"), "not downloaded yet: still the Drive path");
    assert_eq!(link.media_ref(&MediaRef::File { path: "/Users/me/clip.mov".into() }), None, "a file only on this Mac");
    assert_eq!(link.resolve("drive:/Team Projects/A Media/clip.mov"), Default::default());
}

#[test]
fn whole_project_reports_every_part_but_held_ones() {
    let data: filmcraft_drive::ProjectData = serde_json::from_value(project(&[(7, 2), (8, 1)], 4)).unwrap();
    let evs = crate::feed::whole_project(&data, &[8]);
    assert!(evs.iter().any(|e| matches!(e, LinkEvent::SequenceUpdated { seq: ItemId(7), rev: 2, .. })));
    assert!(!evs.iter().any(|e| matches!(e, LinkEvent::SequenceUpdated { seq: ItemId(8), .. })));
    assert!(evs.iter().any(|e| matches!(e, LinkEvent::SharedUpdated { rev: 4, .. })));
}

// ---- downloads -----------------------------------------------------------------------------

fn media_server(body: Vec<u8>) -> MockServer {
    MockServer::start(move |req: &Req| {
        assert_eq!(req.query("path").as_deref(), Some("Day 2/clip.mov"));
        match req.header("range").and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.strip_suffix('-')) {
            Some(start) => {
                let start: usize = start.parse().unwrap();
                let h = vec![("Content-Range".into(), format!("bytes {start}-{}/{}", body.len() - 1, body.len()))];
                Reply::Full(206, h, body[start..].to_vec())
            }
            None => Reply::Full(200, vec![], body.clone()),
        }
    })
}

#[test]
fn downloader_fetches_resumes_and_pauses() {
    let body: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let server = media_server(body.clone());
    let d = DriveDownloader::new(client(&server.url, "ifd_test"), PID, "Team Projects/A.fcteam");
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("clip.mov");
    let drive_ref = "drive:/Team Projects/A Media/Day 2/clip.mov";
    // paused at once: nothing becomes the original, the partial file stays for later
    let paused = d.download(drive_ref, &dest, Some(body.len() as u64), &mut |done, _| done == 0);
    assert_eq!(paused, Err(DownloadError::Paused));
    assert!(!dest.exists() && partial_path(&dest).exists());
    let n = d.download(drive_ref, &dest, Some(body.len() as u64), &mut |_, _| true).unwrap();
    assert_eq!(n, body.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    let outside = d.download("drive:/Elsewhere/clip.mov", &dir.path().join("x"), None, &mut |_, _| true);
    assert!(matches!(outside, Err(DownloadError::Failed(m)) if m.contains("not in this project's media folder")));
}
