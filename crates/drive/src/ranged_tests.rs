//! Parallel ranged downloads and parallel upload chunks against the mock server.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::mock::{MockServer, Reply, Req};
use crate::*;

const PID: &str = "0123456789abcdef0123456789abcdef";
const PATH: &str = "Day 2/clip.mov";

fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 241) as u8).collect()
}

/// Splits anything over 10 kB into ranges.
fn tuning() -> Transfers {
    Transfers { parallel_min_bytes: 10_000, ..Transfers::default() }
}

fn client(server: &MockServer, t: Transfers) -> DriveClient {
    let app = AppInfo::new("0.20.1", 17).unwrap();
    DriveClient::connect(&DriveConfig::new(&server.url, None).unwrap(), "ifd_test", app).unwrap().with_transfers(t)
}

/// What the file server does.
struct Files {
    body: Mutex<Vec<u8>>,
    etag: Mutex<String>,
    honour_ranges: bool,
    /// Requests in flight now, and the most seen at once.
    in_flight: AtomicUsize,
    most: AtomicUsize,
    /// Cut the next range answer after this many bytes.
    cut_next: Mutex<Option<usize>>,
}

impl Files {
    fn new(body: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            body: Mutex::new(body),
            etag: Mutex::new("\"v1\"".into()),
            honour_ranges: true,
            in_flight: AtomicUsize::new(0),
            most: AtomicUsize::new(0),
            cut_next: Mutex::new(None),
        })
    }
}

/// `bytes=a-b` or `bytes=a-`.
fn range(req: &Req, len: usize) -> Option<(usize, usize)> {
    let r = req.header("range")?.strip_prefix("bytes=")?;
    let (a, b) = r.split_once('-')?;
    let a: usize = a.parse().ok()?;
    let b: usize = if b.is_empty() { len - 1 } else { b.parse().ok()? };
    Some((a, b.min(len - 1)))
}

fn file_server(files: Arc<Files>) -> MockServer {
    MockServer::start(move |req: &Req| {
        assert_eq!(req.path(), format!("/api/team/projects/{PID}/media"));
        assert_eq!(req.query("path").as_deref(), Some(PATH));
        let now = files.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        files.most.fetch_max(now, Ordering::SeqCst);
        // long enough for parallel requests to overlap
        std::thread::sleep(Duration::from_millis(30));
        let body = files.body.lock().unwrap().clone();
        let etag = files.etag.lock().unwrap().clone();
        let fresh = req.header("if-range").is_none_or(|t| t == etag);
        let reply = match range(req, body.len()) {
            Some((a, b)) if files.honour_ranges && fresh => {
                let h = vec![("ETag".to_string(), etag.clone()), ("Content-Range".to_string(), format!("bytes {a}-{b}/{}", body.len()))];
                let part = body[a..=b].to_vec();
                match files.cut_next.lock().unwrap().take() {
                    Some(n) if n < part.len() => Reply::Truncated(206, h, part, n),
                    _ => Reply::Full(206, h, part),
                }
            }
            _ => Reply::Full(200, vec![("ETag".to_string(), etag)], body),
        };
        files.in_flight.fetch_sub(1, Ordering::SeqCst);
        reply
    })
}

fn expected(b: &[u8]) -> Expected {
    Expected { size: Some(b.len() as u64), sha256: Some(sha256_hex(&mut &b[..]).unwrap()) }
}

fn ranges_asked(server: &MockServer) -> Vec<String> {
    server.requests().iter().filter_map(|r| r.header("range").map(str::to_string)).filter(|r| r != "bytes=0-0").collect()
}

#[test]
fn big_file_downloads_as_parallel_ranges() {
    let data = body(400_000);
    let files = Files::new(data.clone());
    let server = file_server(files.clone());
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("clip.mov");
    let mut last = (0, 0);
    let n = client(&server, tuning())
        .download_media(PID, PATH, &dest, &expected(&data), &mut |d, t| {
            last = (d, t);
            true
        })
        .unwrap();
    assert_eq!(n, data.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert_eq!(last, (data.len() as u64, data.len() as u64));
    let mut asked = ranges_asked(&server);
    asked.sort();
    assert_eq!(asked, ["bytes=0-99999", "bytes=100000-199999", "bytes=200000-299999", "bytes=300000-399999"]);
    assert!(files.most.load(Ordering::SeqCst) > 1, "ranges ran at the same time");
    assert!(!parts_path(&dest).exists() && !partial_path(&dest).exists(), "nothing left behind");
}

#[test]
fn small_file_and_serial_settings_use_one_stream() {
    let data = body(400_000);
    let server = file_server(Files::new(data.clone()));
    let dir = tempfile::tempdir().unwrap();
    client(&server, Transfers::serial()).download_media(PID, PATH, &dir.path().join("a"), &expected(&data), &mut |_, _| true).unwrap();
    assert!(ranges_asked(&server).is_empty(), "one plain GET");
    assert_eq!(std::fs::read(dir.path().join("a")).unwrap(), data);
}

#[test]
fn stream_limit_holds_across_ranges() {
    let data = body(400_000);
    let files = Files::new(data.clone());
    let server = file_server(files.clone());
    let dir = tempfile::tempdir().unwrap();
    let t = Transfers { max_streams: 2, ..tuning() };
    client(&server, t).download_media(PID, PATH, &dir.path().join("a"), &expected(&data), &mut |_, _| true).unwrap();
    assert_eq!(std::fs::read(dir.path().join("a")).unwrap(), data);
    assert!(files.most.load(Ordering::SeqCst) <= 2, "at most two streams at once");
}

#[test]
fn a_dropped_range_is_tried_again() {
    let data = body(400_000);
    let files = Files::new(data.clone());
    *files.cut_next.lock().unwrap() = Some(1000);
    let server = file_server(files);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a");
    client(&server, tuning()).download_media(PID, PATH, &dest, &expected(&data), &mut |_, _| true).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

#[test]
fn stopped_download_resumes_only_what_is_missing() {
    let data = body(400_000);
    let files = Files::new(data.clone());
    let server = file_server(files);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a");
    let c = client(&server, tuning());
    // stop at once: the plan is on disk, little or nothing arrived
    let err = c.download_media(PID, PATH, &dest, &expected(&data), &mut |_, _| false).unwrap_err();
    assert!(matches!(err, DriveError::Cancelled), "{err:?}");
    assert!(parts_path(&dest).is_file());
    // a plan saying the first and third ranges are done
    let plan = serde_json::json!({"size": 400_000, "etag": "\"v1\"", "parts": [
        {"start": 0, "end": 100_000, "done": 100_000},
        {"start": 100_000, "end": 200_000, "done": 50_000},
        {"start": 200_000, "end": 300_000, "done": 100_000},
        {"start": 300_000, "end": 400_000, "done": 0}]});
    let mut bytes = data.clone();
    bytes[150_000..200_000].fill(0);
    bytes[300_000..].fill(0);
    std::fs::write(partial_path(&dest), &bytes).unwrap();
    std::fs::write(parts_path(&dest), plan.to_string()).unwrap();
    let before = server.requests().len();
    c.download_media(PID, PATH, &dest, &expected(&data), &mut |_, _| true).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    let mut asked: Vec<String> =
        server.requests()[before..].iter().filter_map(|r| r.header("range").map(str::to_string)).filter(|r| r != "bytes=0-0").collect();
    asked.sort();
    assert_eq!(asked, ["bytes=150000-199999", "bytes=300000-399999"]);
}

#[test]
fn changed_file_on_the_drive_starts_over() {
    let data = body(400_000);
    let files = Files::new(data.clone());
    let server = file_server(files.clone());
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a");
    let c = client(&server, tuning());
    let _ = c.download_media(PID, PATH, &dest, &expected(&data), &mut |_, _| false);
    assert!(parts_path(&dest).is_file());
    // the file changes: a new ETag; the old plan is dropped and every range fetched again
    let new = body(400_000).into_iter().map(|b| b.wrapping_add(1)).collect::<Vec<_>>();
    *files.body.lock().unwrap() = new.clone();
    *files.etag.lock().unwrap() = "\"v2\"".into();
    c.download_media(PID, PATH, &dest, &expected(&new), &mut |_, _| true).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), new);
}

#[test]
fn drive_without_ranges_falls_back_to_one_stream() {
    let data = body(400_000);
    let files = Arc::new(Files { honour_ranges: false, ..Arc::try_unwrap(Files::new(data.clone())).ok().unwrap() });
    let server = file_server(files);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a");
    client(&server, tuning()).download_media(PID, PATH, &dest, &expected(&data), &mut |_, _| true).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!parts_path(&dest).exists());
}

#[test]
fn hostile_parts_file_is_ignored() {
    let data = body(400_000);
    let server = file_server(Files::new(data.clone()));
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("a");
    std::fs::write(partial_path(&dest), vec![9u8; 10]).unwrap();
    std::fs::write(parts_path(&dest), r#"{"size":400000,"etag":"\"v1\"","parts":[{"start":0,"end":400000,"done":999999999}]}"#).unwrap();
    client(&server, tuning()).download_media(PID, PATH, &dest, &expected(&data), &mut |_, _| true).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);
}

// ---- parallel upload chunks ------------------------------------------------------------------

#[test]
fn upload_sends_chunks_in_parallel() {
    const MIB: u64 = 1024 * 1024;
    let chunks: Arc<Mutex<std::collections::BTreeMap<u64, Vec<u8>>>> = Arc::default();
    let (in_flight, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let size = Arc::new(Mutex::new(0u64));
    let server = {
        let (chunks, in_flight, most, size) = (chunks.clone(), in_flight.clone(), most.clone(), size.clone());
        MockServer::start(move |req: &Req| {
            let session =
                |size: u64| serde_json::json!({"upload_id": "up_1", "chunk_size": MIB, "total_chunks": size.div_ceil(MIB), "size": size, "received": []});
            match (req.method.as_str(), req.path()) {
                ("POST", p) if p.ends_with("/media/upload/init") => {
                    *size.lock().unwrap() = req.json()["size"].as_u64().unwrap();
                    Reply::json(200, session(*size.lock().unwrap()))
                }
                ("PUT", "/api/upload/chunk") => {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(40));
                    chunks.lock().unwrap().insert(req.query("index").unwrap().parse().unwrap(), req.body.clone());
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Reply::json(200, serde_json::json!({"ok": true}))
                }
                ("POST", p) if p.ends_with("/media/upload/complete") => {
                    let n = chunks.lock().unwrap().values().map(Vec::len).sum::<usize>();
                    Reply::json(200, serde_json::json!({"name": "B.mov", "path": "Team Projects/A Media/B.mov", "size": n}))
                }
                _ => Reply::json(404, serde_json::json!({"detail": "x"})),
            }
        })
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("B.mov");
    let data = body(5 * MIB as usize + 77);
    std::fs::write(&src, &data).unwrap();
    let mut last = 0;
    client(&server, Transfers::default())
        .upload_media(PID, &src, "B.mov", None, None, &mut |p| {
            last = p.done;
            true
        })
        .unwrap();
    let got: Vec<u8> = chunks.lock().unwrap().values().flatten().copied().collect();
    assert_eq!(got, data);
    assert_eq!(last, data.len() as u64);
    let most = most.load(Ordering::SeqCst);
    assert!(most > 1 && most <= UPLOAD_STREAMS, "{most} chunks at once");
}
