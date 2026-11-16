//! Uploads of the member's own media against an in-process Drive: queueing, folders from bins,
//! name clashes, resuming after a restart, and the controller sending the `drive:/` path.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use filmcraft_drive::mock::{MockServer, Reply, Req};
use filmcraft_project::{Bin, BinEntry, BinId, ItemId, ItemKind, Label, MediaClip, MediaRef, Project};
use filmcraft_team::cache::CacheIndex;
use serde_json::json;

use crate::tests::{PID, client};
use crate::*;

pub(crate) const MIB: u64 = 1024 * 1024;

/// The Drive's media folder and chunked uploads. `taken` names are refused once at complete (a
/// teammate's file got there first); `slow_chunks` delays every chunk after the first.
#[derive(Default)]
pub(crate) struct Drive {
    /// Paths in the media folder, with their bytes.
    pub(crate) files: BTreeMap<String, Vec<u8>>,
    /// Upload sessions: id → (path in the media folder, size, chunks).
    pub(crate) sessions: BTreeMap<String, (String, u64, BTreeMap<u64, Vec<u8>>)>,
    pub(crate) taken: BTreeSet<String>,
    pub(crate) next: u32,
    pub(crate) slow_chunks: bool,
}

pub(crate) fn drive(state: Arc<Mutex<Drive>>) -> MockServer {
    drive_with(state, |_| None)
}

/// [`drive`] plus routes `extra` answers first.
fn drive_with(state: Arc<Mutex<Drive>>, extra: impl Fn(&Req) -> Option<Reply> + Send + Sync + 'static) -> MockServer {
    MockServer::start(move |req: &Req| {
        if let Some(reply) = extra(req) {
            return reply;
        }
        let mut d = state.lock().unwrap();
        let project = format!("/api/team/projects/{PID}");
        let session = |id: &str, (_, size, chunks): &(String, u64, BTreeMap<u64, Vec<u8>>)| json!({"upload_id": id, "chunk_size": MIB, "total_chunks": size.div_ceil(MIB), "size": size, "received": chunks.keys().collect::<Vec<_>>()});
        match (req.method.as_str(), req.path()) {
            ("GET", p) if p == format!("{project}/media") && req.query("path").is_none() => {
                let files: Vec<_> = d.files.iter().map(|(path, bytes)| json!({"path": path, "size": bytes.len(), "mtime": 1.0})).collect();
                Reply::json(200, json!({ "files": files }))
            }
            ("GET", p) if p == format!("{project}/media") => {
                let path = req.query("path").unwrap();
                if d.files.contains_key(&path) {
                    Reply::Full(206, vec![("Content-Range".into(), "bytes 0-0/1".into())], vec![0])
                } else {
                    Reply::json(404, json!({"detail": "Not found"}))
                }
            }
            ("POST", p) if p == format!("{project}/media/upload/init") => {
                let b = req.json();
                let rel = match b["path"].as_str() {
                    Some(dir) => format!("{dir}/{}", b["name"].as_str().unwrap()),
                    None => b["name"].as_str().unwrap().to_string(),
                };
                d.next += 1;
                let id = format!("up_{}", d.next);
                let s = (rel, b["size"].as_u64().unwrap(), BTreeMap::new());
                let body = session(&id, &s);
                d.sessions.insert(id, s);
                Reply::json(200, body)
            }
            ("GET", "/api/upload/status") => {
                let id = req.query("upload_id").unwrap();
                match d.sessions.get(&id) {
                    Some(s) => Reply::json(200, session(&id, s)),
                    None => Reply::json(404, json!({"detail": "Upload session not found"})),
                }
            }
            ("PUT", "/api/upload/chunk") => {
                let id = req.query("upload_id").unwrap();
                let index: u64 = req.query("index").unwrap().parse().unwrap();
                let slow = d.slow_chunks && index > 0;
                d.sessions.get_mut(&id).unwrap().2.insert(index, req.body.clone());
                let ok = Reply::json(200, json!({"ok": true}));
                if slow { Reply::Slow(Duration::from_millis(300), Box::new(ok)) } else { ok }
            }
            ("POST", "/api/upload/abort") => {
                let body = String::from_utf8(req.body.clone()).unwrap();
                d.sessions.remove(body.trim_start_matches("upload_id="));
                Reply::json(200, json!({"ok": true}))
            }
            ("POST", p) if p == format!("{project}/media/upload/complete") => {
                assert_eq!(req.json()["expect_mtime_ns"], -1);
                let id = req.json()["upload_id"].as_str().unwrap().to_string();
                let (rel, size, chunks) = d.sessions.get(&id).cloned().unwrap();
                if d.taken.remove(&rel) || d.files.contains_key(&rel) {
                    d.files.insert(rel, b"teammate".to_vec());
                    return Reply::json(409, json!({"error": "conflict", "message": "A file with that name already exists"}));
                }
                d.sessions.remove(&id);
                d.files.insert(rel.clone(), chunks.values().flatten().copied().collect());
                Reply::json(
                    200,
                    json!({"name": rel.rsplit('/').next().unwrap(), "path": format!("Team Projects/Robotics Media/{rel}"), "size": size, "mtime_ns": 1}),
                )
            }
            _ => Reply::json(404, json!({"detail": "x"})),
        }
    })
}

pub(crate) fn media_item(path: &str) -> ItemKind {
    use filmcraft_media::MediaSource;
    let info = filmcraft_media::generators::GeneratorSource::demo(filmcraft_media::DemoScene::OceanSunset).info().clone();
    ItemKind::Media(Box::new(MediaClip {
        media: MediaRef::File { path: path.into() },
        info,
        interpret: Default::default(),
        mark_in: None,
        mark_out: None,
        markers: Vec::new(),
        offline: false,
        proxy: None,
        identity: None,
    }))
}

pub(crate) fn footage(dir: &Path, name: &str, len: usize) -> (PathBuf, Vec<u8>) {
    let path = dir.join(name);
    let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &data).unwrap();
    (path, data)
}

/// Queues `p`'s media on this Mac: the scan, then the files it checked off the UI thread.
pub(crate) fn queue_all(up: &mut TeamUploads, p: &Project, index: &CacheIndex) -> BTreeSet<ItemId> {
    let mut out: BTreeSet<ItemId> = up.scan(p, index).into_iter().collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        out.extend(up.take_queued());
        if !up.checking() {
            return out;
        }
        assert!(Instant::now() < deadline, "the files were never checked");
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub(crate) fn poll_finished(up: &mut TeamUploads, n: usize) -> Vec<FinishedUpload> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    while out.len() < n {
        assert!(Instant::now() < deadline, "uploads never finished: {:?}", up.failures());
        out.extend(up.poll());
        std::thread::sleep(Duration::from_millis(20));
    }
    out
}

#[test]
fn local_media_goes_up_into_bin_folders_under_free_names() {
    let state = Arc::new(Mutex::new(Drive::default()));
    // a teammate already has Footage/Day 2/a.mov; c.mov is taken while ours goes up
    state.lock().unwrap().files.insert("Footage/Day 2/a.mov".into(), b"x".to_vec());
    state.lock().unwrap().taken.insert("c.mov".into());
    let server = drive(state.clone());
    let disk = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let (a, a_bytes) = footage(disk.path(), "a.mov", 2 * MIB as usize + 10);
    let (c, _) = footage(disk.path(), "c.mov", 100);

    let mut p = Project::new("Robotics");
    let ia = p.add_item("a", Label::Rose, media_item(&a.to_string_lossy()), None);
    let ib = p.add_item("a again", Label::Rose, media_item(&a.to_string_lossy()), None);
    let ic = p.add_item("c", Label::Rose, media_item(&c.to_string_lossy()), None);
    let on_drive = p.add_item("d", Label::Rose, media_item("drive:/Team Projects/Robotics Media/d.mov"), None);
    let teammates = p.add_item("e", Label::Rose, media_item("e.mov"), None);
    let gone = p.add_item("f", Label::Rose, media_item(&disk.path().join("missing.mov").to_string_lossy()), None);
    let day2 = Bin { id: BinId(9001), name: "Day 2".into(), children: vec![BinEntry::Item(ia)] };
    p.root.children.retain(|e| *e != BinEntry::Item(ia));
    p.root.children.push(BinEntry::Bin(Bin { id: BinId(9000), name: "Footage".into(), children: vec![BinEntry::Bin(day2)] }));

    let notes = Arc::new(Mutex::new(UploadNotes::new("student1")));
    let mut up = TeamUploads::new(cache.path().to_path_buf(), PID, client(&server.url, "ifd_test"), notes.clone());
    let queued = queue_all(&mut up, &p, &CacheIndex::default());
    assert_eq!(queued, BTreeSet::from([ia, ib, ic]), "only media on this Mac goes up");
    for skipped in [on_drive, teammates, gone] {
        assert!(!queued.contains(&skipped));
    }
    assert_eq!(notes.lock().unwrap().mine, BTreeSet::from([ia.0, ib.0, ic.0]));
    assert_eq!(up.pending().len(), 2, "one upload per file");
    assert!(queue_all(&mut up, &p, &CacheIndex::default()).is_empty(), "queued once");

    let mut done = poll_finished(&mut up, 2);
    done.sort_by(|x, y| x.src.cmp(&y.src));
    assert_eq!(done[0].drive_ref, "drive:/Team Projects/Robotics Media/Footage/Day 2/a (2).mov", "the teammate's file keeps its name");
    assert_eq!(done[0].items.iter().copied().collect::<BTreeSet<_>>(), BTreeSet::from([ia, ib]));
    assert_eq!(done[0].bytes, a_bytes.len() as u64);
    assert_eq!(done[1].drive_ref, "drive:/Team Projects/Robotics Media/c (2).mov", "a clash at complete takes the next number");
    let d = state.lock().unwrap();
    assert_eq!(d.files["Footage/Day 2/a (2).mov"], a_bytes);
    assert_eq!(d.files["Footage/Day 2/a.mov"], b"x", "never overwritten");
    assert!(d.sessions.is_empty(), "the refused session was dropped");
    drop(d);
    assert!(up.pending().is_empty() && notes.lock().unwrap().mine.is_empty());
    let saved = std::fs::read_to_string(cache.path().join(UPLOADS_FILE)).unwrap();
    assert!(!saved.contains(PID), "{saved}");
}

#[test]
fn an_upload_cut_short_resumes_after_a_restart() {
    let state = Arc::new(Mutex::new(Drive { slow_chunks: true, ..Default::default() }));
    let server = drive(state.clone());
    let disk = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let (a, a_bytes) = footage(disk.path(), "a.mov", 6 * MIB as usize);
    let mut p = Project::new("Robotics");
    let ia = p.add_item("a", Label::Rose, media_item(&a.to_string_lossy()), None);
    // another project's queue entry survives this project's saves
    std::fs::write(cache.path().join(UPLOADS_FILE), json!({"uploads": [{"project": "other", "src": "/x/y.mov", "name": "y.mov", "items": [5]}]}).to_string())
        .unwrap();

    let notes = Arc::new(Mutex::new(UploadNotes::new("student1")));
    let mut up = TeamUploads::new(cache.path().to_path_buf(), PID, client(&server.url, "ifd_test"), notes.clone());
    queue_all(&mut up, &p, &CacheIndex::default());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !up.progress().iter().any(|(_, f)| *f > 0.0) {
        assert!(Instant::now() < deadline, "no progress");
        assert!(up.poll().is_empty());
        std::thread::sleep(Duration::from_millis(10));
    }
    // quit mid-upload: what is running stops, the session id is on disk
    up.pause(true);
    let deadline = Instant::now() + Duration::from_secs(10);
    while up.busy() && up.progress().len() == 1 {
        assert!(Instant::now() < deadline);
        up.poll();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(up.progress().is_empty() && up.failures().is_empty(), "paused, not failed");
    drop(up);
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(cache.path().join(UPLOADS_FILE)).unwrap()).unwrap();
    let entry = saved["uploads"].as_array().unwrap().iter().find(|u| u["project"] == PID).unwrap().clone();
    assert_eq!(entry["upload_id"], "up_1");
    assert!(saved["uploads"].as_array().unwrap().iter().any(|u| u["project"] == "other"));
    let sent_before = server.requests().iter().filter(|r| r.path() == "/api/upload/chunk").count();
    // chunks already on their way finish (up to the client's parallel streams); the rest wait
    assert!((1..6).contains(&sent_before), "{sent_before}");

    // the next run: the items point at the file here again, and only missing chunks go up
    let mut up = TeamUploads::new(cache.path().to_path_buf(), PID, client(&server.url, "ifd_test"), notes);
    assert_eq!(up.unfinished(), vec![(vec![ia], a.clone())]);
    queue_all(&mut up, &p, &CacheIndex::default());
    let done = poll_finished(&mut up, 1);
    assert_eq!(done[0].drive_ref, "drive:/Team Projects/Robotics Media/a.mov");
    let reqs = server.requests();
    let chunks: Vec<String> = reqs.iter().filter(|r| r.path() == "/api/upload/chunk").map(|r| r.query("index").unwrap()).collect();
    assert_eq!(chunks.len(), 6, "each chunk sent once: {chunks:?}");
    assert!(reqs.iter().any(|r| r.path() == "/api/upload/status" && r.query("upload_id").as_deref() == Some("up_1")));
    assert_eq!(reqs.iter().filter(|r| r.path().ends_with("/media/upload/init")).count(), 1, "the session was resumed, not restarted");
    assert_eq!(state.lock().unwrap().files["a.mov"], a_bytes);
}

#[test]
fn notes_mark_this_members_uploads_and_read_teammates() {
    let mut notes = UploadNotes::new("student1");
    notes.mine.insert(4);
    let mut upserts =
        BTreeMap::from([(ItemId(4), json!({"id": 4, "local_only": true})), (ItemId(5), json!({"id": 5, "local_only": true})), (ItemId(6), json!({"id": 6}))]);
    notes.mine.insert(6);
    notes.mark(&mut upserts);
    assert_eq!(upserts[&ItemId(4)][UPLOADING_BY], "student1");
    assert!(upserts[&ItemId(5)].get(UPLOADING_BY).is_none(), "not this Mac's upload");
    assert!(upserts[&ItemId(6)].get(UPLOADING_BY).is_none(), "already on the Drive");
    notes.read_shared(&json!({"items": {
        "4": {"local_only": true, "uploading_by": "student1"},
        "7": {"local_only": true, "uploading_by": "student2"},
        "8": {"uploading_by": "student2"},
        "9": {"local_only": true}
    }}));
    assert_eq!(notes.theirs, BTreeMap::from([(7, "student2".to_string())]));
}

#[test]
fn a_new_team_project_uploads_its_media_and_shares_the_drive_path() {
    use filmcraft_drive::{MemoryStore, TokenStore};
    use filmcraft_engine::Session;
    use serde_json::Value;

    let state = Arc::new(Mutex::new(Drive::default()));
    let stored: Arc<Mutex<Option<Value>>> = Arc::default();
    let posts: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (st, ps) = (stored.clone(), posts.clone());
    let server = drive_with(state.clone(), move |req: &Req| {
        let project = format!("/api/team/projects/{PID}");
        Some(match (req.method.as_str(), req.path()) {
            ("GET", "/api/me") => Reply::json(200, json!({"authenticated": true, "nas_username": "student1"})),
            ("GET", "/api/team/projects") => {
                Reply::json(200, json!([{"id": PID, "name": "Robotics", "folder": "Team Projects/Robotics.fcteam", "locks": {}, "last_open": 1.0}]))
            }
            ("POST", "/api/team/projects") => {
                *st.lock().unwrap() = Some(req.json()["project"].clone());
                Reply::json(200, json!({"id": PID, "slot": 2}))
            }
            ("GET", p) if p == project => Reply::json(200, crate::controller_tests::project_data(st.lock().unwrap().as_ref().unwrap())),
            ("GET", p) if p.ends_with("/events") => Reply::json(503, json!({"detail": "down"})),
            ("POST", p) if p == format!("{project}/shared") => {
                let mut ps = ps.lock().unwrap();
                ps.push(req.json());
                Reply::json(200, json!({"rev": 1 + ps.len()}))
            }
            ("POST", p) if p.ends_with("/lock") => Reply::json(200, json!({"token": "t", "expires_at": 9.0})),
            _ => return None,
        })
    });
    let disk = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let (clip, _) = footage(disk.path(), "clip.mov", 5000);
    let store = Arc::new(MemoryStore::new());
    store.save("ifd_test").unwrap();
    let mut c = TeamSessionController::new(crate::controller_tests::config(&server.url, store, cache.path()));
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let mut p = (*s.project).clone();
    let item = p.add_item("clip", Label::Rose, media_item(&clip.to_string_lossy()), None);
    s.project = Arc::new(p);
    crate::controller_tests::poll_until(&mut c, &mut s, |c, _| c.phase() == Phase::SignedIn);

    c.create_from_current(&s, "Robotics");
    crate::controller_tests::poll_until(&mut c, &mut s, |_, s| s.team.is_some());
    let sent = stored.lock().unwrap().clone().unwrap();
    let key = item.0.to_string();
    assert_eq!(sent["items"][&key]["local_only"], true);
    assert_eq!(sent["items"][&key][UPLOADING_BY], "student1", "teammates see it as uploading from this member");
    assert_eq!(sent["items"][&key]["kind"]["Media"]["media"]["File"]["path"], "clip.mov", "no local path leaves the Mac");

    let drive_ref = "drive:/Team Projects/Robotics Media/clip.mov";
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        s.poll_team(web_time::Instant::now());
        c.poll(&mut s);
        let shared_path = |b: &Value| b["upserts"][&key]["kind"]["Media"]["media"]["File"]["path"].as_str().map(str::to_string);
        if posts.lock().unwrap().iter().any(|b| shared_path(b).as_deref() == Some(drive_ref)) {
            break;
        }
        assert!(Instant::now() < deadline, "the Drive path was never shared: {:?} {:?}", posts.lock().unwrap(), c.uploads().map(|u| u.failures()));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(posts.lock().unwrap().iter().all(|b| b["upserts"][&key].get(UPLOADING_BY).is_none() || b["upserts"][&key]["local_only"] == true));
    assert_eq!(state.lock().unwrap().files["clip.mov"].len(), 5000);
    let here = s.project.item(item).and_then(|i| i.as_media()).map(|m| m.media.clone());
    assert_eq!(here, Some(MediaRef::File { path: clip.to_string_lossy().into_owned() }), "this Mac keeps playing its own file");
    let media = c.media().unwrap();
    assert_eq!(media.index().drive_ref_of(PID, &clip), Some(drive_ref), "never downloaded again here");
    assert!(c.uploads().unwrap().pending().is_empty());
}
