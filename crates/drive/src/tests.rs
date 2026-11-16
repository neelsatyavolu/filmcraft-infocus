use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

use crate::mock::{MockServer, Reply, Req};
use crate::*;

const PID: &str = "0123456789abcdef0123456789abcdef";

fn app() -> AppInfo {
    AppInfo::new("0.20.1", 17).unwrap()
}

fn client(server: &MockServer) -> DriveClient {
    DriveClient::connect(&DriveConfig::new(&server.url, None).unwrap(), "ifd_test", app()).unwrap()
}

fn sha_hex(bytes: &[u8]) -> String {
    sha256_hex(&mut &bytes[..]).unwrap()
}

// ---- sign-in -------------------------------------------------------------------------------

#[test]
fn pkce_challenge_is_s256() {
    // Expected value computed independently (Python hashlib + base64.urlsafe_b64encode, no padding).
    assert_eq!(pkce_challenge("dBjftJeZ4CVP-mJ0kNdD8rIy8D56k8pmBcoE6dxKXJk"), "gUSogXC974ddyCkPfcKKDu_O-A0cHlkMyqRdVt_lR-0");
    // The server accepts exactly 43 base64url characters.
    let c = pkce_challenge("anything");
    assert_eq!(c.len(), 43);
    assert!(c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
}

/// Plays the browser: reads the authorize URL, then calls the loopback callback.
fn fake_browser(seen: Arc<Mutex<Option<url::Url>>>, answer: &'static str) -> impl Fn(&str) {
    move |link: &str| {
        let u = url::Url::parse(link).unwrap();
        let q = |k: &str| u.query_pairs().find(|(key, _)| key == k).unwrap().1.into_owned();
        let (port, state) = (q("port"), q("state"));
        *seen.lock().unwrap() = Some(u.clone());
        std::thread::spawn(move || {
            // A stray request with the wrong state is ignored, like the CLI does.
            let _ = ureq::get(format!("http://127.0.0.1:{port}/callback?code=x&state=wrong")).call();
            let _ = ureq::get(format!("http://127.0.0.1:{port}/callback?{answer}&state={state}")).call();
        });
    }
}

#[test]
fn login_exchanges_code_with_verifier_and_stores_token() {
    let server = MockServer::start(|req| match req.path() {
        "/api/cli/token" => Reply::json(200, json!({"token": "ifd_new", "id": "t1", "username": "student1", "device": "InFocus Cut"})),
        _ => Reply::json(404, json!({"detail": "nope"})),
    });
    let cfg = DriveConfig::new(&server.url, None).unwrap();
    let store = MemoryStore::new();
    let seen = Arc::new(Mutex::new(None));
    let opts = LoginOptions { device: "InFocus Cut test".into(), timeout: Duration::from_secs(10) };
    let signed = login(&cfg, &store, &opts, &fake_browser(seen.clone(), "code=code123")).unwrap();
    assert_eq!(signed.token, "ifd_new");
    assert_eq!(signed.username, "student1");
    assert_eq!(store.load().unwrap().as_deref(), Some("ifd_new"));

    let authorize = seen.lock().unwrap().clone().unwrap();
    assert_eq!(authorize.path(), "/cli/authorize");
    let q = |k: &str| authorize.query_pairs().find(|(key, _)| key == k).unwrap().1.into_owned();
    assert_eq!(q("device"), "InFocus Cut test");
    let token_req = server.requests().into_iter().find(|r| r.path() == "/api/cli/token").unwrap();
    let body = token_req.json();
    assert_eq!(body["code"], "code123");
    assert_eq!(pkce_challenge(body["verifier"].as_str().unwrap()), q("challenge"));
}

#[test]
fn login_denied_in_browser_is_an_error() {
    let server = MockServer::start(|_| Reply::json(500, json!({})));
    let cfg = DriveConfig::new(&server.url, None).unwrap();
    let store = MemoryStore::new();
    let opts = LoginOptions { timeout: Duration::from_secs(10), ..LoginOptions::default() };
    let err = login(&cfg, &store, &opts, &fake_browser(Arc::default(), "error=access_denied")).unwrap_err();
    assert!(matches!(err, DriveError::Login(_)), "{err:?}");
    assert_eq!(store.load().unwrap(), None);
}

#[test]
fn login_times_out_without_browser() {
    let cfg = DriveConfig::new("https://drive.example.com", None).unwrap();
    let opts = LoginOptions { timeout: Duration::from_millis(200), ..LoginOptions::default() };
    let err = login(&cfg, &MemoryStore::new(), &opts, &|_| {}).unwrap_err();
    assert!(matches!(err, DriveError::Login(m) if m.contains("timed out")));
}

#[test]
fn bad_token_exchange_reports_server_detail() {
    let server = MockServer::start(|_| Reply::json(401, json!({"detail": "Sign-in code is invalid or expired."})));
    let cfg = DriveConfig::new(&server.url, None).unwrap();
    let opts = LoginOptions { timeout: Duration::from_secs(10), ..LoginOptions::default() };
    let err = login(&cfg, &MemoryStore::new(), &opts, &fake_browser(Arc::default(), "code=c")).unwrap_err();
    assert!(matches!(&err, DriveError::Login(m) if m.contains("invalid or expired")), "{err:?}");
}

// ---- config --------------------------------------------------------------------------------

#[test]
fn example_config_parses() {
    let text = include_str!("../../../config/infocus.example.toml");
    let cfg = DriveConfig::from_toml(text).unwrap();
    assert_eq!(cfg.drive_url, "https://drive.example.com");
    assert_eq!(cfg.lan_url.as_deref(), Some("http://192.168.1.50:8790"));
}

#[test]
fn config_rejects_plain_http_internet_route() {
    assert!(matches!(DriveConfig::new("http://drive.example.com", None), Err(DriveError::Config(_))));
    assert!(DriveConfig::new("http://127.0.0.1:8000/", None).is_ok());
    assert!(matches!(DriveConfig::new("ftp://drive.example.com", None), Err(DriveError::Config(_))));
    assert!(matches!(DriveConfig::new("not a url", None), Err(DriveError::Config(_))));
    assert_eq!(DriveConfig::new("https://drive.example.com/", Some("")).unwrap().lan_url, None);
}

#[test]
fn config_file_wins_then_env() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    assert_eq!(DriveConfig::load_from(Some(&path), Some("https://drive.example.com")).unwrap().drive_url, "https://drive.example.com");
    assert!(matches!(DriveConfig::load_from(Some(&path), None), Err(DriveError::Config(_))));
    std::fs::write(&path, "drive_url = \"https://drive.example.com/x/\"\n").unwrap();
    assert_eq!(DriveConfig::load_from(Some(&path), Some("https://other.example.com")).unwrap().drive_url, "https://drive.example.com/x");
    std::fs::write(&path, "drive_url = 3\n").unwrap();
    assert!(matches!(DriveConfig::load_from(Some(&path), None), Err(DriveError::Config(_))));
}

// ---- team API ------------------------------------------------------------------------------

#[test]
fn app_version_must_look_like_server_expects() {
    assert!(AppInfo::new("1.2.3", 1).is_ok());
    for bad in ["", "1.2.3.4.5", "v1", "1..2", "1234567"] {
        assert!(AppInfo::new(bad, 1).is_err(), "{bad}");
    }
}

#[test]
fn take_lock_409_maps_to_held_by() {
    let server = MockServer::start(|_| Reply::json(409, json!({"holder": "student2", "expires_at": 1700000120.5})));
    let err = client(&server).take_lock(PID, 7).unwrap_err();
    match err {
        DriveError::HeldBy { holder, expires_at } => {
            assert_eq!(holder.as_deref(), Some("student2"));
            assert!((expires_at - 1700000120.5).abs() < 1e-6);
        }
        other => panic!("{other:?}"),
    }
    let req = &server.requests()[0];
    assert_eq!((req.method.as_str(), req.path()), ("POST", format!("/api/team/projects/{PID}/sequences/7/lock").as_str()));
    assert_eq!(req.header("authorization"), Some("Bearer ifd_test"));
}

#[test]
fn take_lock_grant_and_heartbeat_and_release() {
    let server = MockServer::start(|req| match (req.method.as_str(), req.path()) {
        ("POST", p) if p.ends_with("/lock") => Reply::json(200, json!({"token": "tok", "expires_at": 10.0})),
        ("POST", p) if p.ends_with("/heartbeat") => Reply::json(200, json!({"expires_at": 30.0})),
        ("DELETE", _) => Reply::json(200, json!({"ok": true})),
        _ => Reply::json(404, json!({"detail": "x"})),
    });
    let c = client(&server);
    let grant = c.take_lock(PID, 7).unwrap();
    assert_eq!(grant.token, "tok");
    assert_eq!(c.heartbeat(PID, 7, "tok").unwrap(), 30.0);
    c.release(PID, 7, "tok").unwrap();
    let reqs = server.requests();
    assert_eq!(reqs[1].json(), json!({"token": "tok"}));
    assert_eq!(reqs[2].method, "DELETE");
    assert_eq!(reqs[2].json(), json!({"token": "tok"}));
}

#[test]
fn put_426_maps_to_update_required() {
    let server = MockServer::start(|_| Reply::json(426, json!({"error": "update InFocus Cut", "schema_version": 18, "min_app_version": "0.21.0"})));
    let err = client(&server).put_sequence(PID, 7, "tok", 3, &json!({"id": 7})).unwrap_err();
    assert!(matches!(&err, DriveError::UpdateRequired { min_app_version, schema_version: 18 } if min_app_version == "0.21.0"), "{err:?}");
    let body = server.requests()[0].json();
    assert_eq!(body, json!({"token": "tok", "base_rev": 3, "json": {"id": 7}, "app_version": "0.20.1", "schema_version": 17}));
    assert_eq!(server.requests()[0].method, "PUT");
}

#[test]
fn conflicts_map_to_their_variants() {
    let cases = [
        (json!({"error": "lock lost"}), "lock"),
        (json!({"error": "stale", "rev": 9}), "stale"),
        (json!({"error": "item is used by a sequence", "item": 42}), "item"),
        (json!({"error": "project files were changed outside InFocus Cut"}), "files"),
    ];
    for (body, kind) in cases {
        let server = MockServer::start(move |_| Reply::json(409, body.clone()));
        let err = client(&server).put_sequence(PID, 7, "tok", 0, &json!({})).unwrap_err();
        let ok = match kind {
            "lock" => matches!(err, DriveError::LockLost),
            "stale" => matches!(err, DriveError::Stale { rev: 9 }),
            "item" => matches!(err, DriveError::ItemInUse { item: 42 }),
            _ => matches!(err, DriveError::FilesChanged(_)),
        };
        assert!(ok, "{kind}: {err:?}");
    }
}

#[test]
fn http_statuses_map() {
    for (status, check) in [(401u16, "unauth"), (403, "forbidden"), (404, "missing"), (500, "other")] {
        let server = MockServer::start(move |_| Reply::json(status, json!({"detail": "msg"})));
        let err = client(&server).list_projects().unwrap_err();
        let ok = match check {
            "unauth" => matches!(err, DriveError::Unauthorized(_)),
            "forbidden" => matches!(err, DriveError::Forbidden(_)),
            "missing" => matches!(err, DriveError::NotFound(_)),
            _ => matches!(err, DriveError::Http { status: 500, .. }),
        };
        assert!(ok, "{status}: {err:?}");
    }
}

#[test]
fn get_project_parses_sequences_and_locks() {
    let server = MockServer::start(|_| {
        Reply::json(
            200,
            json!({
                "id": PID, "name": "Robotics", "meta": {"schema_version": 17},
                "shared": {"items": {"5": {"id": 5}}},
                "sequences": {"7": {"rev": 2, "json": {"id": 7, "kind": {"Sequence": {}}}}},
                "shared_rev": 4, "locks": {"7": {"holder": "student2", "expires_at": 99.0}}, "slot": 3
            }),
        )
    });
    let p = client(&server).get_project(PID).unwrap();
    assert_eq!(p.sequences[&7].rev, 2);
    assert_eq!(p.locks[&7].holder, "student2");
    assert_eq!((p.shared_rev, p.slot), (4, 3));
}

#[test]
fn list_and_create_and_shared_shapes() {
    let server = MockServer::start(|req| match (req.method.as_str(), req.path()) {
        ("GET", "/api/team/projects") => Reply::json(200, json!([{"id": PID, "name": "A", "folder": "Team Projects/A.fcteam", "locks": {}, "last_open": 5.0}])),
        ("POST", "/api/team/projects") => Reply::json(200, json!({"id": PID, "slot": 1})),
        ("POST", p) if p.ends_with("/shared") => Reply::json(200, json!({"rev": 6})),
        ("POST", p) if p.ends_with("/keep") => Reply::json(200, json!({"keep": true})),
        ("POST", p) if p.ends_with("/open") => Reply::json(200, json!({"ok": true, "last_open": 12.0})),
        ("POST", p) if p.starts_with("/api/team/exports/") => Reply::json(200, json!({"ok": true})),
        _ => Reply::json(404, json!({"detail": "x"})),
    });
    let c = client(&server);
    assert_eq!(c.list_projects().unwrap()[0].folder, "Team Projects/A.fcteam");
    assert_eq!(c.create_project("A", None).unwrap(), CreatedProject { id: PID.into(), slot: 1 });
    let change = SharedChange { base_rev: 5, upserts: [(9, json!({"id": 9}))].into(), deletes: vec![4], bins: None };
    assert_eq!(c.post_shared(PID, &change).unwrap(), 6);
    assert!(c.keep(PID, true).unwrap());
    assert_eq!(c.open(PID).unwrap(), 12.0);
    c.export_begin(PID).unwrap();
    c.export_end(PID).unwrap();
    let reqs = server.requests();
    assert_eq!(reqs[1].json(), json!({"name": "A", "app_version": "0.20.1", "schema_version": 17}));
    assert_eq!(reqs[2].json(), json!({"base_rev": 5, "upserts": {"9": {"id": 9}}, "deletes": [4]}));
    assert_eq!(reqs[5].path(), format!("/api/team/exports/{PID}/begin"));
}

#[test]
fn hostile_ids_never_reach_the_network() {
    let server = MockServer::start(|_| Reply::json(200, json!({})));
    let c = client(&server);
    assert!(matches!(c.get_project("../../api/me"), Err(DriveError::Invalid(_))));
    assert!(matches!(c.take_lock(PID, 0), Err(DriveError::Invalid(_))));
    assert!(matches!(c.take_lock(PID, MAX_ID + 1), Err(DriveError::Invalid(_))));
    assert!(server.requests().is_empty());
}

// ---- change feed ---------------------------------------------------------------------------

#[test]
fn sse_parser_handles_split_chunks() {
    let stream = b": subscribed\r\n\r\nevent: sequence_updated\r\ndata: {\"seq\":7,\"rev\":3,\"by\":\"student1\"}\r\n\r\n\
                   : keepalive\n\nevent: lock_changed\ndata: {\"seq\":7,\"holder\":null}\n\nevent: shared_updated\rdata: {\"rev\":4}\r\r\
                   event: note\ndata: a\ndata: b\n\n";
    let whole = SseParser::new().push(stream).unwrap();
    let mut split = Vec::new();
    let mut p = SseParser::new();
    for b in stream.iter() {
        split.extend(p.push(std::slice::from_ref(b)).unwrap());
    }
    assert_eq!(whole, split);
    let events: Vec<TeamEvent> = whole.into_iter().map(|e| TeamEvent::from_sse(e).unwrap()).collect();
    assert_eq!(
        events,
        vec![
            TeamEvent::SequenceUpdated { seq: 7, rev: 3, by: "student1".into() },
            TeamEvent::LockChanged { seq: 7, holder: None },
            TeamEvent::SharedUpdated { rev: 4 },
            TeamEvent::Other { event: "note".into(), data: "a\nb".into() },
        ]
    );
}

#[test]
fn sse_parser_rejects_endless_line_and_bad_payload() {
    let mut p = SseParser::new();
    assert!(p.push(&vec![b'a'; 2 * 1024 * 1024]).is_err());
    let ev = SseEvent { event: "sequence_updated".into(), data: "{\"seq\":-1}".into() };
    assert!(TeamEvent::from_sse(ev).is_err());
}

#[test]
fn events_stream_from_server() {
    let server = MockServer::start(|_| {
        let chunks = [": subscribed\n\nevent: lock_cha", "nged\ndata: {\"seq\":2,\"holder\":\"student2\"}\n", "\nevent: shared_updated\ndata: {\"rev\":1}\n\n"];
        Reply::Chunks(
            200,
            vec![("Content-Type".into(), "text/event-stream".into())],
            chunks.iter().map(|c| c.as_bytes().to_vec()).collect(),
            Duration::from_millis(20),
        )
    });
    let events: Vec<TeamEvent> = client(&server).events(PID).unwrap().map(|e| e.unwrap()).collect();
    assert_eq!(events, vec![TeamEvent::LockChanged { seq: 2, holder: Some("student2".into()) }, TeamEvent::SharedUpdated { rev: 1 }]);
    assert_eq!(server.requests()[0].header("accept"), Some("text/event-stream"));
}

// ---- downloads -----------------------------------------------------------------------------

fn media() -> Vec<u8> {
    (0..100_000u32).map(|i| (i % 251) as u8).collect()
}

/// A Drive download route that honours `Range: bytes=N-` like Starlette's FileResponse.
fn range_server(body: Vec<u8>, honour_range: bool) -> MockServer {
    MockServer::start(move |req: &Req| {
        assert_eq!(req.path(), "/api/download");
        assert_eq!(req.query("path").as_deref(), Some("Team Projects/A Media/clip.mov"));
        let etag = vec![("ETag".to_string(), "\"v1\"".to_string())];
        match req.header("range").and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.strip_suffix('-')) {
            Some(start) if honour_range => {
                let start: usize = start.parse().unwrap();
                if start >= body.len() {
                    return Reply::Full(416, vec![("Content-Range".into(), format!("bytes */{}", body.len()))], vec![]);
                }
                let mut h = etag.clone();
                h.push(("Content-Range".into(), format!("bytes {start}-{}/{}", body.len() - 1, body.len())));
                Reply::Full(206, h, body[start..].to_vec())
            }
            _ => Reply::Full(200, etag, body.clone()),
        }
    })
}

#[test]
fn resume_from_partial() {
    let body = media();
    let server = range_server(body.clone(), true);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("original");
    std::fs::write(partial_path(&dest), &body[..40_000]).unwrap();
    let mut seen = Vec::new();
    let expected = Expected { size: Some(body.len() as u64), sha256: Some(sha_hex(&body)) };
    let n = client(&server).download("drive:/Team Projects/A Media/clip.mov", &dest, &expected, &mut |done, total| {
        seen.push((done, total));
        true
    });
    assert_eq!(n.unwrap(), body.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(!partial_path(&dest).exists());
    assert_eq!(server.requests()[0].header("range"), Some("bytes=40000-"));
    assert_eq!(seen.first(), Some(&(40_000, 100_000)));
    assert_eq!(seen.last(), Some(&(100_000, 100_000)));
}

#[test]
fn partial_never_promoted() {
    let body = media();
    let full_len = body.len();
    let calls = Arc::new(Mutex::new(0));
    let counter = calls.clone();
    // First answer: the connection drops after 30 000 bytes. Later answers honour Range.
    let server = MockServer::start(move |req: &Req| {
        let mut n = counter.lock().unwrap();
        *n += 1;
        match req.header("range") {
            None if *n == 1 => Reply::Truncated(200, vec![], body.clone(), 30_000),
            Some(r) => {
                let start: usize = r.trim_start_matches("bytes=").trim_end_matches('-').parse().unwrap();
                Reply::Full(206, vec![("Content-Range".into(), format!("bytes {start}-{}/{}", full_len - 1, full_len))], body[start..].to_vec())
            }
            None => Reply::Full(200, vec![], body.clone()),
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("original");
    let expected = Expected { size: Some(full_len as u64), sha256: None };
    let c = client(&server);
    let err = c.download("drive:/Team Projects/A Media/clip.mov", &dest, &expected, &mut |_, _| true).unwrap_err();
    assert!(err.is_network(), "{err:?}");
    assert!(!dest.exists(), "a cut download must never become the original");
    assert_eq!(std::fs::metadata(partial_path(&dest)).unwrap().len(), 30_000);
    // The next attempt resumes and completes.
    c.download("drive:/Team Projects/A Media/clip.mov", &dest, &expected, &mut |_, _| true).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), media());
    assert_eq!(server.requests()[1].header("range"), Some("bytes=30000-"));
}

#[test]
fn hash_mismatch_is_never_promoted() {
    let server = range_server(media(), true);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("original");
    let expected = Expected { size: None, sha256: Some("00".repeat(32)) };
    let err = client(&server).download("Team Projects/A Media/clip.mov", &dest, &expected, &mut |_, _| true).unwrap_err();
    assert!(matches!(err, DriveError::Verify(_)), "{err:?}");
    assert!(!dest.exists());
    assert!(!partial_path(&dest).exists(), "bad bytes are dropped so the retry starts clean");
}

#[test]
fn server_ignoring_range_restarts_from_zero() {
    let body = media();
    let server = range_server(body.clone(), false);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("original");
    std::fs::write(partial_path(&dest), vec![0xEE; 50_000]).unwrap();
    client(&server).download("drive:/Team Projects/A Media/clip.mov", &dest, &Expected::default(), &mut |_, _| true).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);
}

#[test]
fn complete_partial_finishes_on_416() {
    let body = media();
    let server = range_server(body.clone(), true);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("original");
    std::fs::write(partial_path(&dest), &body).unwrap();
    client(&server).download("drive:/Team Projects/A Media/clip.mov", &dest, &Expected::default(), &mut |_, _| true).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);
}

#[test]
fn cancel_keeps_partial_for_later() {
    let server = range_server(media(), true);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("original");
    let err = client(&server).download("drive:/Team Projects/A Media/clip.mov", &dest, &Expected::default(), &mut |done, _| done == 0).unwrap_err();
    assert!(matches!(err, DriveError::Cancelled));
    assert!(!dest.exists());
    assert!(partial_path(&dest).exists());
}

#[test]
fn download_rejects_parent_paths() {
    let server = range_server(media(), true);
    let dir = tempfile::tempdir().unwrap();
    let err = client(&server).download("drive:/Team Projects/../secret", &dir.path().join("o"), &Expected::default(), &mut |_, _| true).unwrap_err();
    assert!(matches!(err, DriveError::Invalid(_)));
    assert!(server.requests().is_empty());
}

// ---- LAN route -----------------------------------------------------------------------------

/// A LAN host that proves itself, and an internet Drive that checks the proof.
fn lan_pair(lan_delay: Duration) -> (MockServer, MockServer) {
    let lan = MockServer::start(move |req| {
        let reply = match req.path() {
            "/api/lan/proof" => Reply::json(200, json!({"proof": format!("p-{}", req.query("nonce").unwrap())})),
            "/api/team/projects" if req.header("authorization") == Some("Bearer ifl_lan") => Reply::json(200, json!([])),
            _ => Reply::json(401, json!({"detail": "LAN sign-in expired"})),
        };
        if lan_delay.is_zero() { reply } else { Reply::Slow(lan_delay, Box::new(reply)) }
    });
    let drive = MockServer::start(|req| match req.path() {
        "/api/cli/lan-token" => {
            let form: Vec<(String, String)> = url::form_urlencoded::parse(&req.body).into_owned().collect();
            let get = |k: &str| form.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default();
            if req.header("authorization") == Some("Bearer ifd_test") && get("proof") == format!("p-{}", get("nonce")) {
                Reply::json(200, json!({"token": "ifl_lan", "lan_origin": "http://192.168.1.50:8790", "expires_in": 28800}))
            } else {
                Reply::json(403, json!({"detail": "That LAN host isn't this Drive"}))
            }
        }
        p if p.ends_with("/lock") => Reply::json(200, json!({"token": "tok", "expires_at": 1.0})),
        _ => Reply::json(200, json!([])),
    });
    (lan, drive)
}

#[test]
fn lan_preferred_when_reachable() {
    let (lan, drive) = lan_pair(Duration::ZERO);
    let cfg = DriveConfig::new(&drive.url, Some(&lan.url)).unwrap();
    let c = DriveClient::connect(&cfg, "ifd_test", app()).unwrap();
    assert!(c.on_lan());
    c.list_projects().unwrap();
    let lan_reqs = lan.requests();
    let last = lan_reqs.last().unwrap();
    assert_eq!(last.path(), "/api/team/projects");
    assert_eq!(last.header("authorization"), Some("Bearer ifl_lan"), "the long-lived token never goes over the LAN");
    assert!(lan_reqs.iter().all(|r| r.header("authorization") != Some("Bearer ifd_test")));
    assert!(drive.requests().iter().all(|r| r.path() == "/api/cli/lan-token"));
}

#[test]
fn slow_lan_falls_back_to_internet() {
    let (lan, drive) = lan_pair(Duration::from_millis(900));
    let cfg = DriveConfig::new(&drive.url, Some(&lan.url)).unwrap();
    let c = DriveClient::connect(&cfg, "ifd_test", app()).unwrap();
    assert!(!c.on_lan());
    c.list_projects().unwrap();
    assert_eq!(drive.requests().last().unwrap().header("authorization"), Some("Bearer ifd_test"));
}

#[test]
fn unreachable_lan_falls_back_to_internet() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://127.0.0.1:{}", closed.local_addr().unwrap().port());
    drop(closed);
    let drive = MockServer::start(|_| Reply::json(200, json!([])));
    let c = DriveClient::connect(&DriveConfig::new(&drive.url, Some(&dead)).unwrap(), "ifd_test", app()).unwrap();
    assert!(!c.on_lan());
    assert_eq!(c.base_url(), drive.url);
}

#[test]
fn lan_token_rejected_mid_session_falls_back() {
    let (lan, drive) = lan_pair(Duration::ZERO);
    let cfg = DriveConfig::new(&drive.url, Some(&lan.url)).unwrap();
    let c = DriveClient::connect(&cfg, "ifd_test", app()).unwrap();
    assert!(c.on_lan());
    // The LAN token expires: the LAN host answers 401, the call is retried over the internet.
    assert!(c.take_lock(PID, 3).is_ok());
    assert!(!c.on_lan());
    assert_eq!(drive.requests().last().unwrap().path(), format!("/api/team/projects/{PID}/sequences/3/lock"));
}
