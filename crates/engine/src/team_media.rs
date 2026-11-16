//! Team media on this Mac (InFocus Cut): downloads originals on demand, makes proxies, and keeps
//! the cache within its rules.
//!
//! - Only media the editor needs is fetched: what the open sequences (nested ones included) and the
//!   Source monitor use ([`filmcraft_team::cache::wanted`], or [`TeamMedia::ensure_open`]). Opening
//!   a project downloads nothing.
//! - At most [`MAX_DOWNLOADS`] at once, each on its own worker thread (under `catch_unwind`). The
//!   [`Downloader`] (the app's Drive client) writes `<dest>.partial` and renames it to `dest` only
//!   when the file is complete and checked, so a cut download never becomes an original; the next
//!   try resumes the partial file with an HTTP Range request.
//! - [`TeamMedia::pause`] stops running downloads (their partial files stay) and starts none; the
//!   app pauses during an export.
//! - A finished original is linked to its item, and a proxy (ProRes 422 Proxy at ½ size, the
//!   existing proxy job in `proxies.rs`) is made next to it. Both links stay on this Mac: they go
//!   into the project and every undo snapshot without an undo step, and are never sent to the
//!   Drive.
//! - [`TeamMedia::clean_up`] applies [`filmcraft_team::cache::eviction_plan`]: a project's media
//!   leaves this Mac 30 days after it was last opened here, and over 200 GB originals go before
//!   proxies, never the media of an open sequence. Items whose files went point at the Drive again.
//!
//! Layout: `<cache>/<fingerprint>/original/<file name>` and `<cache>/<fingerprint>/proxy/`.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_project::{ItemId, MediaRef, Project};
use filmcraft_team::cache::{self, CACHE_CAP_BYTES, CacheIndex, CachedFile, Fingerprint, ORIGINAL_DIR, PROXY_DIR};
use serde_json::json;

use crate::{Session, panels};

/// Downloads running at once.
pub const MAX_DOWNLOADS: usize = 2;
/// The proxy preset for team media (ProRes 422 Proxy, ½ size).
pub const PROXY_PRESET: &str = "prores_proxy_half";

/// Why a download stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadError {
    /// The progress callback asked to stop; the partial file is kept for resuming.
    Paused,
    /// Anything else, with a message for the person.
    Failed(String),
}

/// The app's way to fetch team media (the Drive client, on the worker thread).
pub trait Downloader: Send + Sync {
    /// Download the media at `drive_ref` (`drive:/…`) to `dest`, resuming `<dest>.partial`, and
    /// rename it to `dest` only once complete and checked (`size`, when known, must match).
    /// `progress(done, total)` returning false stops with [`DownloadError::Paused`]. Returns the
    /// size.
    fn download(&self, drive_ref: &str, dest: &Path, size: Option<u64>, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<u64, DownloadError>;
}

/// A download on its worker thread.
struct Active {
    item: ItemId,
    drive_ref: String,
    fp: Fingerprint,
    dest: PathBuf,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    result: Arc<Mutex<Option<Result<u64, DownloadError>>>>,
}

/// A proxy job for a downloaded original.
struct Proxying {
    job: u64,
    out: String,
    drive_ref: String,
    fp: Fingerprint,
}

/// This Mac's team media for one open team project.
pub struct TeamMedia {
    cache_dir: PathBuf,
    project: String,
    client: Arc<dyn Downloader>,
    index: CacheIndex,
    cap_bytes: u64,
    wanted: BTreeSet<ItemId>,
    active: Vec<Active>,
    proxying: BTreeMap<ItemId, Proxying>,
    failed: BTreeMap<ItemId, String>,
    paused: bool,
}

/// Unix seconds now.
fn now_unix() -> u64 {
    web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The file name of a `drive:/` path, safe as a single path component.
fn file_name(drive_ref: &str) -> String {
    let name = drive_ref.rsplit(['/', '\\']).next().unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." || name.chars().any(char::is_control) { "original".into() } else { name.to_string() }
}

impl TeamMedia {
    /// Team media of project `project` (its Drive id), cached in `cache_dir` (the app passes
    /// `~/Library/Caches/InFocus Cut/media`), fetched with `client`.
    pub fn new(cache_dir: PathBuf, project: &str, client: Arc<dyn Downloader>) -> Self {
        let index = CacheIndex::load(&cache_dir);
        Self {
            cache_dir,
            project: project.to_string(),
            client,
            index,
            cap_bytes: CACHE_CAP_BYTES,
            wanted: BTreeSet::new(),
            active: Vec::new(),
            proxying: BTreeMap::new(),
            failed: BTreeMap::new(),
            paused: false,
        }
    }

    /// Size cap (default [`CACHE_CAP_BYTES`]; a setting in the app).
    pub fn set_cap_bytes(&mut self, cap: u64) {
        self.cap_bytes = cap;
    }

    /// The cache index (for the app's `TeamLink::resolve` and `media_ref`).
    pub fn index(&self) -> &CacheIndex {
        &self.index
    }

    /// The project was opened on this Mac at `now` (Unix seconds): restarts its 30-day clock.
    pub fn opened(&mut self, now: u64) -> std::io::Result<()> {
        self.index.opened(&self.project, now);
        self.index.save(&self.cache_dir)
    }

    /// This Mac uploaded `items`' original from the member's own file at `path` to `drive_ref`:
    /// the file is recorded in the cache index (so it is never downloaded here, and never removed),
    /// which also makes `TeamLink::media_ref` give the items' Drive path from now on.
    pub fn record_uploaded(&mut self, s: &mut Session, items: &[ItemId], drive_ref: &str, path: &Path, bytes: u64) {
        let identity = items.iter().find_map(|i| s.project.item(*i).and_then(|it| it.as_media()).and_then(|m| m.identity));
        let fp = Fingerprint::for_media(identity.as_ref(), drive_ref);
        self.index.record_uploaded(&fp, &self.project, drive_ref, CachedFile { path: path.to_path_buf(), bytes }, now_unix());
        self.save(s);
    }

    /// The media items to have on this Mac (replaces the previous set). Items no longer wanted
    /// stop downloading; their partial files stay for later.
    pub fn ensure(&mut self, items: BTreeSet<ItemId>) {
        for a in &self.active {
            if !items.contains(&a.item) {
                a.stop.store(true, Ordering::Relaxed);
            }
        }
        self.wanted = items;
    }

    /// [`TeamMedia::ensure`] with what the session's open sequences and Source monitor use.
    pub fn ensure_open(&mut self, s: &Session) {
        self.ensure(cache::wanted(&s.project, &s.state.open_sequences, s.state.source_item));
    }

    /// Stop running downloads (kept for resuming) and start none, or carry on.
    pub fn pause(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            for a in &self.active {
                a.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Running downloads: (item, fraction done 0..=1).
    pub fn progress(&self) -> Vec<(ItemId, f32)> {
        self.active
            .iter()
            .map(|a| {
                let (done, total) = (a.done.load(Ordering::Relaxed), a.total.load(Ordering::Relaxed));
                (a.item, if total == 0 { 0.0 } else { (done as f64 / total as f64).clamp(0.0, 1.0) as f32 })
            })
            .collect()
    }

    /// Proxies this Mac is making for downloaded originals: (item, fraction done 0..=1). They
    /// leave the list when the proxy is attached (or failed; see the session log).
    pub fn proxy_progress(&self, s: &Session) -> Vec<(ItemId, f32)> {
        self.proxying.iter().map(|(item, px)| (*item, s.jobs.iter().find(|j| j.id == px.job).map_or(0.0, |j| j.progress.fraction().clamp(0.0, 1.0)))).collect()
    }

    /// Downloads that failed, with the reason. They are not tried again until [`Self::retry`].
    pub fn failures(&self) -> Vec<(ItemId, String)> {
        self.failed.iter().map(|(i, e)| (*i, e.clone())).collect()
    }

    /// Try failed downloads again.
    pub fn retry(&mut self) {
        self.failed.clear();
    }

    /// Call every frame: links finished downloads and proxies, then starts downloads (at most
    /// [`MAX_DOWNLOADS`] running, none while paused).
    pub fn poll(&mut self, s: &mut Session) {
        self.finish_downloads(s);
        self.finish_proxies(s);
        if self.paused {
            return;
        }
        let busy: BTreeSet<ItemId> = self.active.iter().map(|a| a.item).collect();
        let todo: Vec<ItemId> = self.wanted.iter().copied().filter(|i| !busy.contains(i) && !self.failed.contains_key(i)).collect();
        for item in todo {
            if self.active.len() >= MAX_DOWNLOADS {
                break;
            }
            let Some(drive_ref) = cache::drive_ref(&s.project, item).map(str::to_string) else { continue };
            let identity = s.project.item(item).and_then(|i| i.as_media()).and_then(|m| m.identity);
            let fp = Fingerprint::for_media(identity.as_ref(), &drive_ref);
            let dest = fp.dir(&self.cache_dir).join(ORIGINAL_DIR).join(file_name(&drive_ref));
            if self.active.iter().any(|a| a.dest == dest) {
                // another item's download is writing this file's `.partial`: this item is linked
                // when it finishes (or by the next poll)
                continue;
            }
            if dest.is_file() {
                // downloaded before (this project or another): link it without fetching again
                let bytes = std::fs::metadata(&dest).map_or(0, |m| m.len());
                self.linked(s, item, &drive_ref, &fp, &dest, bytes);
                continue;
            }
            if let Err(e) = self.start(item, drive_ref, fp, dest, identity.map(|i| i.size)) {
                self.failed.insert(item, e);
            }
        }
    }

    fn start(&mut self, item: ItemId, drive_ref: String, fp: Fingerprint, dest: PathBuf, size: Option<u64>) -> Result<(), String> {
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("can't make the cache folder: {e}"))?;
        }
        let a = Active {
            item,
            drive_ref: drive_ref.clone(),
            fp,
            dest: dest.clone(),
            done: Arc::default(),
            total: Arc::new(AtomicU64::new(size.unwrap_or(0))),
            stop: Arc::default(),
            result: Arc::default(),
        };
        let (client, done, total, stop, result) = (self.client.clone(), a.done.clone(), a.total.clone(), a.stop.clone(), a.result.clone());
        let run = move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                client.download(&drive_ref, &dest, size, &mut |d, t| {
                    done.store(d, Ordering::Relaxed);
                    total.store(t, Ordering::Relaxed);
                    !stop.load(Ordering::Relaxed)
                })
            }))
            .unwrap_or_else(|_| Err(DownloadError::Failed("the download stopped unexpectedly".into())));
            *result.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
        };
        std::thread::Builder::new().name("infocus-media-download".into()).spawn(run).map_err(|e| format!("can't start the download: {e}"))?;
        self.active.push(a);
        Ok(())
    }

    fn finish_downloads(&mut self, s: &mut Session) {
        let (done, running): (Vec<Active>, Vec<Active>) =
            std::mem::take(&mut self.active).into_iter().partition(|a| a.result.lock().unwrap_or_else(PoisonError::into_inner).is_some());
        self.active = running;
        for a in done {
            let outcome = a.result.lock().unwrap_or_else(PoisonError::into_inner).take();
            match outcome {
                // the downloader renamed `.partial` to `dest` after checking it
                Some(Ok(bytes)) if a.dest.is_file() => self.linked(s, a.item, &a.drive_ref, &a.fp, &a.dest, bytes),
                Some(Ok(_)) => {
                    self.failed.insert(a.item, "the download finished without a file".into());
                }
                Some(Err(DownloadError::Failed(e))) => {
                    let name = s.project.item(a.item).map_or_else(|| format!("item {}", a.item.0), |i| i.name.clone());
                    s.log.push(panels::Level::Warning, "team", format!("Download of {name} failed: {e}"));
                    self.failed.insert(a.item, e);
                }
                // paused or no longer wanted: started again when wanted and not paused
                Some(Err(DownloadError::Paused)) | None => {}
            }
        }
    }

    /// `item`'s original is at `dest`: link it on this Mac and make its proxy.
    fn linked(&mut self, s: &mut Session, item: ItemId, drive_ref: &str, fp: &Fingerprint, dest: &Path, bytes: u64) {
        let now = now_unix();
        self.index.record_original(fp, &self.project, drive_ref, CachedFile { path: dest.to_path_buf(), bytes }, now);
        let path = dest.to_string_lossy().into_owned();
        s.media.allow(&path);
        let from = MediaRef::File { path: drive_ref.to_string() };
        let to = MediaRef::File { path: path.clone() };
        s.patch_local(|p| relink(p, &from, &to));
        s.offline.missing.retain(|i| *i != item);
        let proxy_dir = fp.dir(&self.cache_dir).join(PROXY_DIR);
        let existing = self.index.entries.get(fp).and_then(|e| e.proxy.clone()).filter(|f| f.path.is_file());
        if let Some(proxy) = existing {
            attach_proxy(s, &to, &proxy.path);
        } else if let std::collections::btree_map::Entry::Vacant(slot) = self.proxying.entry(item) {
            match start_proxy(s, item, &proxy_dir) {
                Ok(Some((job, out))) => {
                    slot.insert(Proxying { job, out, drive_ref: drive_ref.to_string(), fp: fp.clone() });
                }
                Ok(None) => {}
                // audio-only media has no proxy; anything else plays from the original
                Err(e) => s.log.push(panels::Level::Info, "team", format!("No proxy for item {}: {e}", item.0)),
            }
        }
        self.save(s);
    }

    fn finish_proxies(&mut self, s: &mut Session) {
        let ids: Vec<ItemId> = self.proxying.keys().copied().collect();
        for item in ids {
            let Some(px) = self.proxying.get(&item) else { continue };
            let result = s.jobs.iter().find(|j| j.id == px.job).map(|j| j.result.lock().unwrap_or_else(PoisonError::into_inner).clone());
            let outcome = match result {
                Some(None) => continue, // still running
                Some(Some(r)) => r,
                None => Err("the proxy job was removed".into()),
            };
            let Some(px) = self.proxying.remove(&item) else { continue };
            let out = PathBuf::from(&px.out);
            match outcome {
                Ok(_) if out.is_file() => {
                    let bytes = std::fs::metadata(&out).map_or(0, |m| m.len());
                    self.index.record_proxy(&px.fp, &self.project, &px.drive_ref, CachedFile { path: out.clone(), bytes }, now_unix());
                    let original = self
                        .index
                        .entries
                        .get(&px.fp)
                        .and_then(|e| e.original.as_ref())
                        .map(|f| MediaRef::File { path: f.path.to_string_lossy().into_owned() });
                    if let Some(original) = original {
                        attach_proxy(s, &original, &out);
                    }
                    self.save(s);
                }
                Ok(_) => s.log.push(panels::Level::Warning, "team", format!("The proxy for item {} was not written", item.0)),
                Err(e) => s.log.push(panels::Level::Warning, "team", format!("The proxy for item {} failed: {e}", item.0)),
            }
        }
    }

    /// Removes what the cache rules say should go (see [`filmcraft_team::cache::eviction_plan`])
    /// at `now` (Unix seconds). Media of the session's open sequences and Source monitor, and
    /// media still downloading or wanted, stays. Items whose original went point at the Drive
    /// again; items whose proxy went lose it. Returns the removed files.
    pub fn clean_up(&mut self, s: &mut Session, now: u64) -> std::io::Result<Vec<PathBuf>> {
        let mut keep_items = cache::wanted(&s.project, &s.state.open_sequences, s.state.source_item);
        keep_items.extend(self.wanted.iter().copied());
        keep_items.extend(self.active.iter().map(|a| a.item));
        let mut in_use: BTreeSet<Fingerprint> = self.active.iter().map(|a| a.fp.clone()).chain(self.proxying.values().map(|p| p.fp.clone())).collect();
        for item in keep_items {
            let Some(MediaRef::File { path }) = s.project.item(item).and_then(|i| i.as_media()).map(|m| &m.media) else { continue };
            let found = self
                .index
                .entries
                .iter()
                .find(|(_, e)| e.drive_refs.values().any(|r| r == path) || e.original.as_ref().is_some_and(|f| f.path == Path::new(path)));
            if let Some((fp, _)) = found {
                in_use.insert(fp.clone());
            }
        }
        let plan = cache::eviction_plan(&self.index, self.cap_bytes, now, &in_use);
        let mut removed = BTreeSet::new();
        for path in plan {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    s.log.push(panels::Level::Warning, "team", format!("Could not remove {}: {e}", path.display()));
                    continue;
                }
            }
            if let Some(dir) = path.parent() {
                // empty folders only; a folder with anything left in it stays
                let _ = std::fs::remove_dir(dir);
                if let Some(fp_dir) = dir.parent() {
                    let _ = std::fs::remove_dir(fp_dir);
                }
            }
            removed.insert(path);
        }
        let unlink: Vec<(MediaRef, Option<MediaRef>, bool)> = self
            .index
            .entries
            .values()
            .flat_map(|e| {
                let back = e.drive_refs.get(&self.project).cloned().map(|path| MediaRef::File { path });
                let original = e.original.iter().filter(|f| removed.contains(&f.path)).map(move |f| (file_ref(&f.path), back.clone(), false));
                let proxy = e.proxy.iter().filter(|f| removed.contains(&f.path)).map(|f| (file_ref(&f.path), None, true));
                original.chain(proxy).collect::<Vec<_>>()
            })
            .collect();
        if !unlink.is_empty() {
            s.patch_local(|p| {
                for (gone, back, is_proxy) in &unlink {
                    unlink_file(p, gone, back.as_ref(), *is_proxy);
                }
            });
        }
        self.index.forget(&removed);
        self.index.save(&self.cache_dir)?;
        Ok(removed.into_iter().collect())
    }

    fn save(&self, s: &mut Session) {
        if let Err(e) = self.index.save(&self.cache_dir) {
            s.log.push(panels::Level::Warning, "team", format!("Could not save the media cache index: {e}"));
        }
    }
}

/// Points `items` at the member's own file `path` on this Mac without an undo step and without
/// sending it to the Drive (after a restart, for media this Mac is still uploading: the Drive has
/// only its file name). Items already on a local file or a `drive:/` path are left alone.
pub fn relink_local(s: &mut Session, items: &[ItemId], path: &Path) {
    let to = file_ref(path);
    let stale: Vec<ItemId> = items
        .iter()
        .copied()
        .filter(|i| {
            s.project
                .item(*i)
                .and_then(|it| it.as_media())
                .is_some_and(|m| matches!(&m.media, MediaRef::File { path: p } if !p.starts_with(filmcraft_team::DRIVE_PREFIX) && !Path::new(p).is_absolute()))
        })
        .collect();
    if stale.is_empty() {
        return;
    }
    s.media.allow(&to_string(path));
    s.patch_local(|p| {
        for id in &stale {
            if let Some(m) = p.items.get_mut(id).and_then(|it| it.as_media_mut()) {
                m.media = to.clone();
                m.offline = false;
            }
        }
    });
    s.offline.missing.retain(|i| !stale.contains(i));
}

fn to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Starts the existing proxy job for `item` into `dir`, not attaching (this module attaches it
/// locally when the job is done). Returns (job id, output path).
fn start_proxy(s: &mut Session, item: ItemId, dir: &Path) -> Result<Option<(u64, String)>, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let v = crate::proxies::create(s, &json!({"items": [item.0], "preset": PROXY_PRESET, "destination": dir, "attach": false})).map_err(|e| e.to_string())?;
    let out = v["outputs"][0]["path"].as_str().unwrap_or_default().to_string();
    Ok(v["job"].as_u64().filter(|_| !out.is_empty()).map(|job| (job, out)))
}

fn file_ref(path: &Path) -> MediaRef {
    MediaRef::File { path: path.to_string_lossy().into_owned() }
}

/// Points every media item at `from` to `to` (and back online).
fn relink(p: &mut Project, from: &MediaRef, to: &MediaRef) {
    for item in p.items.values_mut() {
        if let Some(m) = item.as_media_mut()
            && m.media == *from
        {
            m.media = to.clone();
            m.offline = false;
        }
    }
}

/// Attaches the proxy at `proxy` to every item whose original is `original`.
fn attach_proxy(s: &mut Session, original: &MediaRef, proxy: &Path) {
    let path = proxy.to_string_lossy().into_owned();
    s.media.allow(&path);
    let proxy = MediaRef::File { path };
    s.patch_local(|p| {
        for item in p.items.values_mut() {
            if let Some(m) = item.as_media_mut()
                && m.media == *original
            {
                m.proxy = Some(proxy.clone());
            }
        }
    });
}

/// After a cached file was removed: an item whose original it was points at `back` (its Drive
/// path); an item whose proxy it was loses the proxy.
fn unlink_file(p: &mut Project, gone: &MediaRef, back: Option<&MediaRef>, is_proxy: bool) {
    for item in p.items.values_mut() {
        let Some(m) = item.as_media_mut() else { continue };
        if is_proxy {
            if m.proxy.as_ref() == Some(gone) {
                m.proxy = None;
            }
        } else if m.media == *gone
            && let Some(back) = back
        {
            m.media = back.clone();
            m.proxy = None;
        }
    }
}
