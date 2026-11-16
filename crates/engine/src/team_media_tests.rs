//! Team media (`team_media.rs`): on-demand downloads, linking, proxies, pause and cleanup,
//! against fake downloaders (no network).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use filmcraft_media::{DemoScene, MediaSource};
use filmcraft_project::{ItemId, ItemKind, Label, MediaClip, MediaIdentity, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_team::cache::{CacheIndex, Fingerprint, PROJECT_IDLE_SECS};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};

use crate::Session;
use crate::media_test_util::{make_movie, session_with, tmp_dir};
use crate::team_media::{DownloadError, Downloader, MAX_DOWNLOADS, TeamMedia};

const PID: &str = "0123456789abcdef0123456789abcdef";

fn drive_ref(n: usize) -> String {
    format!("drive:/Team Projects/R Media/clip{n}.mov")
}

fn media_path(s: &Session, item: ItemId) -> String {
    match &s.project.item(item).unwrap().as_media().unwrap().media {
        MediaRef::File { path } => path.clone(),
        MediaRef::Generator(_) => String::new(),
    }
}

/// Polls until `done` holds (up to 60 s).
fn poll_until(m: &mut TeamMedia, s: &mut Session, done: impl Fn(&TeamMedia, &Session) -> bool) {
    for _ in 0..6000 {
        m.poll(s);
        crate::proxies::poll(s);
        if done(m, s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out");
}

/// Points `item` at `path` (undo snapshots from before the import don't have it).
fn set_media(p: &mut Project, item: ItemId, path: &str) {
    if let Some(m) = p.item_mut(item).and_then(|i| i.as_media_mut()) {
        m.media = MediaRef::File { path: path.into() };
    }
}

/// A session whose sequence uses `n` team media items (not on this Mac yet), the sequence open.
#[allow(clippy::field_reassign_with_default)] // Session has private fields: no struct literal
fn team_session(n: usize) -> (Session, Vec<ItemId>, ItemId) {
    let info = filmcraft_media::generators::GeneratorSource::demo(DemoScene::OceanSunset).info().clone();
    let mut p = Project::new("Robotics");
    let seq = p.new_sequence("Package", SequenceSettings::default(), 1, 1, None);
    let mut items = Vec::new();
    for k in 0..n {
        let clip = MediaClip {
            media: MediaRef::File { path: drive_ref(k) },
            info: info.clone(),
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: vec![],
            offline: false,
            proxy: None,
            identity: Some(MediaIdentity { size: 10 + k as u64, fingerprint: 0xabc0 + k as u64 }),
        };
        let item = p.add_item(&format!("clip{k}.mov"), Label::Iris, ItemKind::Media(Box::new(clip)), None);
        let rate = p.sequence(seq).unwrap().settings.frame_rate;
        let ti = p.make_track_item(item, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick(TICKS_PER_SECOND)), rate).unwrap();
        p.sequence_mut(seq).unwrap().video_tracks[0].items.push(ti);
        items.push(item);
    }
    let mut s = Session::default();
    s.project = Arc::new(p);
    s.state.open_sequences = vec![seq];
    s.state.active_sequence = Some(seq);
    (s, items, seq)
}

/// Writes `bytes` like the Drive client does: into `<dest>.partial`, renamed when complete.
fn write_like_client(dest: &Path, bytes: &[u8]) -> u64 {
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".partial");
    std::fs::write(&partial, bytes).unwrap();
    std::fs::rename(&partial, dest).unwrap();
    bytes.len() as u64
}

/// Hands out a fixed body for every download.
struct Copies(Vec<u8>, AtomicUsize);

impl Downloader for Copies {
    fn download(&self, _: &str, dest: &Path, _: Option<u64>, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<u64, DownloadError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        progress(0, self.0.len() as u64);
        Ok(write_like_client(dest, &self.0))
    }
}

#[test]
fn downloaded_original_is_linked_and_proxied() {
    let root = tmp_dir("team-media-link");
    let movie = root.join("a.mov");
    make_movie(&movie, DemoScene::OceanSunset, 64, 36, 6);
    let (mut s, items, _) = session_with(&[&movie]);
    let item = items[0];
    let identity = s.project.item(item).unwrap().as_media().unwrap().identity;
    let r = "drive:/Team Projects/R Media/Day 2/a.mov";
    s.patch_local(|p| set_media(p, item, r));
    let undo = s.history.undo.len();
    let cache = root.join("cache");
    let fake = Arc::new(Copies(std::fs::read(&movie).unwrap(), AtomicUsize::new(0)));
    let mut m = TeamMedia::new(cache.clone(), PID, fake.clone());
    m.ensure_open(&s);
    poll_until(&mut m, &mut s, |_, s| !media_path(s, item).starts_with("drive:/"));
    let fp = Fingerprint::for_media(identity.as_ref(), r);
    let original = cache.join(fp.as_str()).join("original").join("a.mov");
    assert_eq!(media_path(&s, item), original.to_string_lossy());
    assert_eq!(s.history.undo.len(), undo, "linking is not an undo step");
    // the proxy is made on this Mac as soon as the original is here, and shows as in progress
    assert!(
        m.proxy_progress(&s).iter().any(|(i, f)| *i == item && (0.0..=1.0).contains(f)) || s.project.item(item).unwrap().as_media().unwrap().proxy.is_some(),
        "a proxy job starts for the downloaded original"
    );
    poll_until(&mut m, &mut s, |_, s| s.project.item(item).unwrap().as_media().unwrap().proxy.is_some());
    assert!(m.proxy_progress(&s).is_empty(), "a finished proxy leaves the list");
    let Some(MediaRef::File { path: proxy }) = s.project.item(item).unwrap().as_media().unwrap().proxy.clone() else { panic!() };
    assert!(Path::new(&proxy).starts_with(cache.join(fp.as_str()).join("proxy")), "{proxy}");
    assert!(Path::new(&proxy).is_file());
    assert_eq!(s.history.undo.len(), undo, "attaching the proxy is not an undo step");
    let idx = CacheIndex::load(&cache);
    assert_eq!(idx.resolve(r).original, Some(original.clone()));
    assert_eq!(idx.resolve(r).proxy, Some(PathBuf::from(&proxy)));
    assert_eq!(fake.1.load(Ordering::SeqCst), 1);
    // A second project session that uses the same file links the cached copy without fetching it.
    s.patch_local(|p| set_media(p, item, r));
    let mut again = TeamMedia::new(cache.clone(), PID, fake.clone());
    again.ensure_open(&s);
    again.poll(&mut s);
    assert_eq!(media_path(&s, item), original.to_string_lossy());
    assert_eq!(fake.1.load(Ordering::SeqCst), 1, "no second download");
    let _ = std::fs::remove_dir_all(&root);
}

/// Writes half a file to `.partial`, then fails like a dropped connection.
struct Drops;

impl Downloader for Drops {
    fn download(&self, _: &str, dest: &Path, _: Option<u64>, _: &mut dyn FnMut(u64, u64) -> bool) -> Result<u64, DownloadError> {
        let mut partial = dest.as_os_str().to_owned();
        partial.push(".partial");
        std::fs::write(&partial, b"half").unwrap();
        Err(DownloadError::Failed("connection closed after 4 of 10 bytes".into()))
    }
}

#[test]
fn partial_never_promoted() {
    let root = tmp_dir("team-media-partial");
    let (mut s, items, _) = team_session(1);
    let mut m = TeamMedia::new(root.clone(), PID, Arc::new(Drops));
    m.ensure_open(&s);
    poll_until(&mut m, &mut s, |m, _| !m.failures().is_empty());
    assert_eq!(media_path(&s, items[0]), drive_ref(0), "a failed download is never linked");
    let fp = Fingerprint::for_media(s.project.item(items[0]).unwrap().as_media().unwrap().identity.as_ref(), &drive_ref(0));
    let dest = root.join(fp.as_str()).join("original").join("clip0.mov");
    assert!(!dest.exists(), "no original from a cut download");
    assert!(dest.with_file_name("clip0.mov.partial").exists(), "the partial file stays for resuming");
    // not tried again on every frame, only after retry
    m.poll(&mut s);
    assert!(m.progress().is_empty());
    m.retry();
    m.poll(&mut s);
    assert_eq!(m.progress().len(), 1);
    poll_until(&mut m, &mut s, |m, _| !m.failures().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// Holds every download until the gate opens, or stops when asked to.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
    started: AtomicUsize,
    calls: Mutex<Vec<String>>,
}

impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

impl Downloader for Gate {
    fn download(&self, drive_ref: &str, dest: &Path, _: Option<u64>, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<u64, DownloadError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.calls.lock().unwrap().push(drive_ref.to_string());
        let mut open = self.open.lock().unwrap();
        while !*open {
            if !progress(1, 10) {
                return Err(DownloadError::Paused);
            }
            open = self.cv.wait_timeout(open, Duration::from_millis(5)).unwrap().0;
        }
        drop(open);
        Ok(write_like_client(dest, b"0123456789"))
    }
}

#[test]
fn at_most_max_downloads_at_once() {
    let root = tmp_dir("team-media-two");
    let (mut s, items, _) = team_session(MAX_DOWNLOADS + 1);
    let gate = Arc::new(Gate::default());
    let mut m = TeamMedia::new(root.clone(), PID, gate.clone());
    m.ensure_open(&s);
    m.poll(&mut s);
    assert_eq!(m.progress().len(), MAX_DOWNLOADS);
    poll_until(&mut m, &mut s, |_, _| gate.started.load(Ordering::SeqCst) == MAX_DOWNLOADS);
    std::thread::sleep(Duration::from_millis(50));
    m.poll(&mut s);
    assert_eq!(gate.started.load(Ordering::SeqCst), MAX_DOWNLOADS, "the last one waits");
    gate.release();
    poll_until(&mut m, &mut s, |_, s| items.iter().all(|&i| !media_path(s, i).starts_with("drive:/")));
    assert_eq!(gate.started.load(Ordering::SeqCst), MAX_DOWNLOADS + 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn front_media_downloads_first() {
    let root = tmp_dir("team-media-first");
    let (mut s, items, _) = team_session(MAX_DOWNLOADS + 2);
    let gate = Arc::new(Gate::default());
    let mut m = TeamMedia::new(root.clone(), PID, gate.clone());
    m.ensure(items.iter().copied().collect());
    let last = *items.last().unwrap();
    m.ensure_first([last].into_iter().collect());
    m.poll(&mut s);
    let running: Vec<ItemId> = m.progress().into_iter().map(|(i, _)| i).collect();
    assert_eq!(running.len(), MAX_DOWNLOADS);
    assert!(running.contains(&last), "the front sequence's clip is among the first: {running:?}");
    gate.release();
    poll_until(&mut m, &mut s, |_, s| items.iter().all(|&i| !media_path(s, i).starts_with("drive:/")));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn paused_queue_starts_nothing() {
    let root = tmp_dir("team-media-pause");
    let (mut s, _, _) = team_session(2);
    let gate = Arc::new(Gate::default());
    let mut m = TeamMedia::new(root.clone(), PID, gate.clone());
    m.pause(true);
    m.ensure_open(&s);
    m.poll(&mut s);
    std::thread::sleep(Duration::from_millis(30));
    assert!(m.progress().is_empty());
    assert_eq!(gate.started.load(Ordering::SeqCst), 0);
    m.pause(false);
    m.poll(&mut s);
    assert_eq!(m.progress().len(), 2);
    // pausing again stops the running downloads; their partial files would stay
    m.pause(true);
    poll_until(&mut m, &mut s, |m, _| m.progress().is_empty());
    assert!(m.failures().is_empty(), "a pause is not a failure");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn only_media_of_open_sequences_is_fetched() {
    let root = tmp_dir("team-media-open");
    let (mut s, _, _) = team_session(2);
    s.state.open_sequences.clear();
    let gate = Arc::new(Gate::default());
    gate.release();
    let mut m = TeamMedia::new(root.clone(), PID, gate.clone());
    m.ensure_open(&s);
    m.poll(&mut s);
    assert!(m.progress().is_empty(), "opening the project downloads nothing");
    assert_eq!(gate.started.load(Ordering::SeqCst), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn thirty_days_unopened_removes_media_and_points_back_at_the_drive() {
    let root = tmp_dir("team-media-30");
    let (mut s, items, seq) = team_session(2);
    let fake = Arc::new(Copies(b"0123456789".to_vec(), AtomicUsize::new(0)));
    let mut m = TeamMedia::new(root.clone(), PID, fake);
    let now = 1_800_000_000;
    m.opened(now).unwrap();
    m.ensure_open(&s);
    poll_until(&mut m, &mut s, |_, s| items.iter().all(|&i| !media_path(s, i).starts_with("drive:/")));
    let originals: Vec<String> = items.iter().map(|&i| media_path(&s, i)).collect();
    let later = now + PROJECT_IDLE_SECS + 60;
    // the sequence is still open: nothing goes
    assert!(m.clean_up(&mut s, later).unwrap().is_empty());
    s.state.open_sequences.clear();
    m.ensure(BTreeSet::new());
    // opened 29 days ago: kept
    assert!(m.clean_up(&mut s, now + PROJECT_IDLE_SECS - 86_400).unwrap().is_empty());
    let undo = s.history.undo.len();
    let removed = m.clean_up(&mut s, later).unwrap();
    assert_eq!(removed.len(), 2);
    for (k, path) in originals.iter().enumerate() {
        assert!(!Path::new(path).exists());
        assert_eq!(media_path(&s, items[k]), drive_ref(k), "the item points at the Drive again");
    }
    assert_eq!(s.history.undo.len(), undo);
    assert!(CacheIndex::load(&root).entries.is_empty());
    let _ = seq;
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn over_the_cap_originals_go_first_and_open_media_stays() {
    let root = tmp_dir("team-media-cap");
    let (mut s, items, seq) = team_session(3);
    let fake = Arc::new(Copies(b"0123456789".to_vec(), AtomicUsize::new(0)));
    let mut m = TeamMedia::new(root.clone(), PID, fake);
    let now = 1_800_000_000;
    m.opened(now).unwrap();
    m.ensure_open(&s);
    poll_until(&mut m, &mut s, |_, s| items.iter().all(|&i| !media_path(s, i).starts_with("drive:/")));
    // only clip0 stays in the open sequence
    let mut p = (*s.project).clone();
    p.sequence_mut(seq).unwrap().video_tracks[0].items.retain(|t| t.item == items[0]);
    s.project = Arc::new(p);
    m.ensure_open(&s);
    m.set_cap_bytes(10);
    let removed = m.clean_up(&mut s, now + 60).unwrap();
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert!(!media_path(&s, items[0]).starts_with("drive:/"), "media of the open sequence stays");
    assert_eq!(media_path(&s, items[1]), drive_ref(1));
    assert_eq!(media_path(&s, items[2]), drive_ref(2));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn two_items_of_one_file_download_it_once() {
    // two items using the same Drive file (imported twice): one download into one `.partial`
    let root = tmp_dir("team-media-same-file");
    let (mut s, items, _) = team_session(2);
    let mut p = (*s.project).clone();
    let first = p.item(items[0]).unwrap().as_media().unwrap().clone();
    if let Some(m) = p.item_mut(items[1]).and_then(|i| i.as_media_mut()) {
        m.media = first.media.clone();
        m.identity = first.identity;
    }
    s.project = Arc::new(p);
    let gate = Arc::new(Gate::default());
    let mut m = TeamMedia::new(root.clone(), PID, gate.clone());
    m.ensure_open(&s);
    m.poll(&mut s);
    assert_eq!(m.progress().len(), 1, "the second item waits for the first one's download");
    gate.release();
    poll_until(&mut m, &mut s, |_, s| items.iter().all(|&i| !media_path(s, i).starts_with("drive:/")));
    assert_eq!(gate.started.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&root);
}
