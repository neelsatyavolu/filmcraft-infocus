//! [`TeamUploads`]: uploads the member's own media into the team project's `<name> Media/`
//! folder, so teammates can download it.
//!
//! - What goes up: every media item of the open team project whose file is on this Mac only (an
//!   absolute path that exists and that the cache index doesn't know as a Drive file). That covers
//!   a project just made a team project and media imported into an open one.
//! - Where: `<name> Media/<bin path>/<file name>`, the item's bins in the project as folders. A
//!   name already on the Drive gets a number (`A001 (2).mov`); completing never overwrites.
//! - At most [`MAX_UPLOADS`] at once, each on its own thread (under `catch_unwind`), none while
//!   paused (an export). The queue and each session id are kept in `<cache>/uploads.json`, so an
//!   upload cut by a quit or a crash resumes where it stopped.
//! - Until it finishes the item plays from the member's file here; teammates see it as "uploading
//!   from <member>" ([`UploadNotes`]). When it finishes, the app records the file in the cache
//!   index (`TeamMedia::record_uploaded`) and sends the item again, now with its `drive:/` path.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_drive::{DRIVE_PREFIX, DriveClient, DriveError, UploadedMedia};
use filmcraft_project::{Bin, BinEntry, ItemId, MediaRef, Project};
use filmcraft_team::cache::CacheIndex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Uploads running at once.
pub const MAX_UPLOADS: usize = 2;
/// The upload queue inside the cache folder.
pub const UPLOADS_FILE: &str = "uploads.json";
/// The shared item's field naming the member uploading its file (with `local_only`).
pub const UPLOADING_BY: &str = "uploading_by";
const LOCAL_ONLY: &str = "local_only";
/// Names tried before an upload gives up (`A001.mov`, `A001 (2).mov`, …).
const MAX_NAME_TRIES: u32 = 50;
const MAX_BIN_DEPTH: usize = 64;

/// Who is uploading what, shared between the uploads, the link (which marks this member's
/// uploading items in what it sends) and the panel (which shows teammates' uploads).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UploadNotes {
    /// This member's Drive user name.
    pub me: String,
    /// Items this Mac is uploading the file of.
    pub mine: BTreeSet<u64>,
    /// Items a teammate is uploading the file of, with their name.
    pub theirs: BTreeMap<u64, String>,
}

impl UploadNotes {
    pub fn new(me: &str) -> Self {
        Self { me: me.to_string(), ..Self::default() }
    }

    /// Marks this member's uploading items among shared upserts (`uploading_by`); only items the
    /// Drive can't resolve yet (`local_only`) get it.
    pub fn mark(&self, upserts: &mut BTreeMap<ItemId, Value>) {
        for (id, v) in upserts.iter_mut() {
            if self.mine.contains(&id.0) && v.get(LOCAL_ONLY).and_then(Value::as_bool) == Some(true) {
                v[UPLOADING_BY] = Value::String(self.me.clone());
            }
        }
    }

    /// Reads teammates' uploads from the whole shared part.
    pub fn read_shared(&mut self, shared: &Value) {
        let Some(items) = shared.get("items").and_then(Value::as_object) else { return };
        self.theirs = items
            .iter()
            .filter(|(_, v)| v.get(LOCAL_ONLY).and_then(Value::as_bool) == Some(true))
            .filter_map(|(k, v)| Some((k.parse().ok()?, v.get(UPLOADING_BY)?.as_str()?.to_string())))
            .filter(|(_, by)| !by.is_empty() && *by != self.me)
            .collect();
    }
}

/// One file waiting to go up (or going up), as kept in [`UPLOADS_FILE`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingUpload {
    /// The team project's id.
    pub project: String,
    /// The member's file on this Mac.
    pub src: PathBuf,
    /// Its name in the media folder (a number added after a clash).
    pub name: String,
    /// Its folder inside the media folder (the item's bins).
    #[serde(default)]
    pub dir: Option<String>,
    /// The Drive's session, to resume.
    #[serde(default)]
    pub upload_id: Option<String>,
    /// The media items using the file.
    #[serde(default)]
    pub items: BTreeSet<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct UploadsFile {
    #[serde(default)]
    uploads: Vec<PendingUpload>,
}

/// A finished upload: `items`' file `src` is now at `drive_ref` on the Drive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishedUpload {
    pub items: Vec<ItemId>,
    pub src: PathBuf,
    pub drive_ref: String,
    pub bytes: u64,
}

enum Outcome {
    Done(UploadedMedia),
    Stopped,
    Failed(String),
}

/// The name and session a running upload is at (the worker updates it; `poll` saves it).
#[derive(Clone, Default, PartialEq, Eq)]
struct Progress {
    name: String,
    upload_id: Option<String>,
}

struct Active {
    src: PathBuf,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    at: Arc<Mutex<Progress>>,
    result: Arc<Mutex<Option<Outcome>>>,
}

/// This Mac's uploads for one open team project.
pub struct TeamUploads {
    cache_dir: PathBuf,
    project: String,
    client: Arc<DriveClient>,
    pending: Vec<PendingUpload>,
    active: Vec<Active>,
    failed: BTreeMap<PathBuf, String>,
    paused: bool,
    notes: Arc<Mutex<UploadNotes>>,
}

impl TeamUploads {
    /// The uploads of `project` (its Drive id), with what an earlier run left in `cache_dir`.
    pub fn new(cache_dir: PathBuf, project: &str, client: Arc<DriveClient>, notes: Arc<Mutex<UploadNotes>>) -> Self {
        let pending = load(&cache_dir).uploads.into_iter().filter(|u| u.project == project).collect();
        let up = Self { cache_dir, project: project.to_string(), client, pending, active: Vec::new(), failed: BTreeMap::new(), paused: false, notes };
        up.note_mine();
        up
    }

    /// Files an earlier run did not finish, with their items: the app points the items back at
    /// the file on this Mac (the Drive has only its name).
    pub fn unfinished(&self) -> Vec<(Vec<ItemId>, PathBuf)> {
        self.pending.iter().map(|u| (u.items.iter().map(|i| ItemId(*i)).collect(), u.src.clone())).collect()
    }

    /// Queues the media of `project` that is on this Mac only. Returns the items newly queued
    /// (the app sends them again so teammates see them as uploading).
    pub fn scan(&mut self, project: &Project, index: &CacheIndex) -> Vec<ItemId> {
        let mut new = Vec::new();
        let before = self.pending.clone();
        for u in &mut self.pending {
            let src = u.src.clone();
            u.items.retain(|id| local_file(project, ItemId(*id)) == Some(src.as_path()));
        }
        for item in project.items.values() {
            let Some(src) = local_file(project, item.id) else { continue };
            if index.drive_ref_of(&self.project, src).is_some() {
                continue;
            }
            if let Some(u) = self.pending.iter_mut().find(|u| u.src == src) {
                if u.items.insert(item.id.0) {
                    new.push(item.id);
                }
                continue;
            }
            let Some(name) = upload_name(src).filter(|_| src.is_file()) else { continue };
            let dir = bin_dir(&project.root, item.id);
            self.pending.push(PendingUpload {
                project: self.project.clone(),
                src: src.to_path_buf(),
                name,
                dir,
                upload_id: None,
                items: BTreeSet::from([item.id.0]),
            });
            new.push(item.id);
        }
        let running: BTreeSet<PathBuf> = self.active.iter().map(|a| a.src.clone()).collect();
        self.pending.retain(|u| !u.items.is_empty() || running.contains(&u.src));
        if self.pending != before {
            self.save();
            self.note_mine();
        }
        new
    }

    /// Call every frame: collects finished uploads and starts queued ones (at most
    /// [`MAX_UPLOADS`] running, none while paused).
    pub fn poll(&mut self) -> Vec<FinishedUpload> {
        let mut finished = Vec::new();
        let mut changed = self.note_progress();
        let (done, running): (Vec<Active>, Vec<Active>) =
            std::mem::take(&mut self.active).into_iter().partition(|a| a.result.lock().unwrap_or_else(PoisonError::into_inner).is_some());
        self.active = running;
        for a in done {
            let outcome = a.result.lock().unwrap_or_else(PoisonError::into_inner).take();
            match outcome {
                Some(Outcome::Done(m)) => {
                    let items = self.pending.iter().find(|u| u.src == a.src).map(|u| u.items.iter().map(|i| ItemId(*i)).collect()).unwrap_or_default();
                    self.pending.retain(|u| u.src != a.src);
                    changed = true;
                    let path = m.path.trim_start_matches('/');
                    finished.push(FinishedUpload { items, src: a.src, drive_ref: format!("{DRIVE_PREFIX}{path}"), bytes: m.size });
                }
                Some(Outcome::Failed(e)) => {
                    self.failed.insert(a.src, e);
                }
                // paused or left: it resumes later
                Some(Outcome::Stopped) | None => {}
            }
        }
        if changed {
            self.save();
            self.note_mine();
        }
        if !self.paused {
            self.start_queued();
        }
        finished
    }

    fn start_queued(&mut self) {
        let todo: Vec<PendingUpload> = self
            .pending
            .iter()
            .filter(|u| !self.failed.contains_key(&u.src) && !self.active.iter().any(|a| a.src == u.src))
            .take(MAX_UPLOADS.saturating_sub(self.active.len()))
            .cloned()
            .collect();
        for u in todo {
            if let Err(e) = self.start(u.clone()) {
                self.failed.insert(u.src, e);
            }
        }
    }

    fn start(&mut self, u: PendingUpload) -> Result<(), String> {
        let a = Active {
            src: u.src.clone(),
            done: Arc::default(),
            total: Arc::default(),
            stop: Arc::default(),
            at: Arc::new(Mutex::new(Progress { name: u.name.clone(), upload_id: u.upload_id.clone() })),
            result: Arc::default(),
        };
        let job = Job {
            client: self.client.clone(),
            project: self.project.clone(),
            u,
            done: a.done.clone(),
            total: a.total.clone(),
            stop: a.stop.clone(),
            at: a.at.clone(),
        };
        let result = a.result.clone();
        let run = move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| job.run())).unwrap_or_else(|_| Outcome::Failed("the upload stopped unexpectedly".into()));
            *result.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
        };
        std::thread::Builder::new().name("infocus-media-upload".into()).spawn(run).map_err(|e| format!("can't start the upload: {e}"))?;
        self.active.push(a);
        Ok(())
    }

    /// Copies running uploads' names and session ids into the queue. True when one changed.
    fn note_progress(&mut self) -> bool {
        let mut changed = false;
        for a in &self.active {
            let at = a.at.lock().unwrap_or_else(PoisonError::into_inner).clone();
            if let Some(u) = self.pending.iter_mut().find(|u| u.src == a.src)
                && (u.name != at.name || u.upload_id != at.upload_id)
            {
                u.name = at.name;
                u.upload_id = at.upload_id;
                changed = true;
            }
        }
        changed
    }

    /// Stop running uploads (their sessions stay for resuming) and start none, or carry on.
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

    /// Something is queued or going up.
    pub fn busy(&self) -> bool {
        !self.active.is_empty() || self.pending.iter().any(|u| !self.failed.contains_key(&u.src))
    }

    /// Running uploads: (file name on the Drive, fraction done 0..=1).
    pub fn progress(&self) -> Vec<(String, f32)> {
        self.active
            .iter()
            .map(|a| {
                let (done, total) = (a.done.load(Ordering::Relaxed), a.total.load(Ordering::Relaxed));
                let name = a.at.lock().unwrap_or_else(PoisonError::into_inner).name.clone();
                (name, if total == 0 { 0.0 } else { (done as f64 / total as f64).clamp(0.0, 1.0) as f32 })
            })
            .collect()
    }

    /// Uploads that failed: (file name, why). Not tried again until [`Self::retry`].
    pub fn failures(&self) -> Vec<(String, String)> {
        self.failed.iter().map(|(src, e)| (display_name(src), e.clone())).collect()
    }

    pub fn retry(&mut self) {
        self.failed.clear();
    }

    /// The queue as kept on disk for this project.
    pub fn pending(&self) -> &[PendingUpload] {
        &self.pending
    }

    fn note_mine(&self) {
        let mine = self.pending.iter().flat_map(|u| u.items.iter().copied()).collect();
        self.notes.lock().unwrap_or_else(PoisonError::into_inner).mine = mine;
    }

    /// Writes this project's queue, keeping other projects' entries.
    fn save(&self) {
        let mut file = load(&self.cache_dir);
        file.uploads.retain(|u| u.project != self.project);
        file.uploads.extend(self.pending.iter().cloned());
        if let Err(e) = write(&self.cache_dir, &file) {
            log::warn!("could not save the upload queue: {e}");
        }
    }
}

impl Drop for TeamUploads {
    /// Leaving the project stops its uploads; they resume when it is opened again.
    fn drop(&mut self) {
        for a in &self.active {
            a.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// One upload on its worker thread.
struct Job {
    client: Arc<DriveClient>,
    project: String,
    u: PendingUpload,
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    at: Arc<Mutex<Progress>>,
}

impl Job {
    /// Uploads under the first free name: a name already on the Drive gets the next number.
    fn run(self) -> Outcome {
        let base = self.u.name.clone();
        let (mut name, mut resume) = (base.clone(), self.u.upload_id.clone());
        let dir = self.u.dir.as_deref();
        for n in 1..=MAX_NAME_TRIES {
            if n > 1 {
                name = numbered(&base, n);
                resume = None;
                self.set(&name, None);
            }
            if resume.is_none() {
                match self.client.media_exists(&self.project, &in_dir(dir, &name)) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => return Outcome::Failed(e.to_string()),
                }
            }
            let r = self.client.upload_media(&self.project, &self.u.src, &name, dir, resume.as_deref(), &mut |p| {
                self.set(&name, Some(p.upload_id));
                self.done.store(p.done, Ordering::Relaxed);
                self.total.store(p.total, Ordering::Relaxed);
                !self.stop.load(Ordering::Relaxed)
            });
            match r {
                Ok(m) => return Outcome::Done(m),
                Err(DriveError::Cancelled) => return Outcome::Stopped,
                // a teammate's file got the name first
                Err(DriveError::NameTaken(_)) => {}
                Err(e) => return Outcome::Failed(e.to_string()),
            }
        }
        Outcome::Failed(format!("no free name for {base} in the project's media folder"))
    }

    fn set(&self, name: &str, upload_id: Option<&str>) {
        *self.at.lock().unwrap_or_else(PoisonError::into_inner) = Progress { name: name.to_string(), upload_id: upload_id.map(str::to_string) };
    }
}

/// The member's own file behind `item`: a media item whose path is absolute and not on the Drive.
fn local_file(project: &Project, item: ItemId) -> Option<&Path> {
    match &project.item(item)?.as_media()?.media {
        MediaRef::File { path } if !path.starts_with(DRIVE_PREFIX) && Path::new(path).is_absolute() => Some(Path::new(path)),
        _ => None,
    }
}

/// The items of `project` whose file is on this Mac and would be uploaded (for marking a project
/// that is being made a team project).
pub fn uploadable(project: &Project) -> Vec<ItemId> {
    project.items.keys().copied().filter(|id| local_file(project, *id).is_some_and(Path::is_file)).collect()
}

/// Marks `ids` in a whole project JSON (`items`) as uploading from `me`.
pub fn mark_project(project: &mut Value, ids: &[ItemId], me: &str) {
    let Some(items) = project.get_mut("items").and_then(Value::as_object_mut) else { return };
    for id in ids {
        if let Some(v) = items.get_mut(&id.0.to_string())
            && v.get(LOCAL_ONLY).and_then(Value::as_bool) == Some(true)
        {
            v[UPLOADING_BY] = Value::String(me.to_string());
        }
    }
}

fn display_name(src: &Path) -> String {
    src.file_name().map_or_else(|| src.to_string_lossy().into_owned(), |n| n.to_string_lossy().into_owned())
}

/// A name or folder the Drive accepts: no `/`, `\` or control characters, not `.` or `..`, and
/// not ending in `.fcteam` (reserved for team projects). None when nothing is left.
fn clean_part(part: &str) -> Option<String> {
    let mut s: String = part.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '-' } else { c }).collect::<String>().trim().to_string();
    if s.is_empty() {
        return None;
    }
    if s == "." || s == ".." {
        s = s.replace('.', "_");
    }
    if s.to_ascii_lowercase().ends_with(".fcteam") {
        s.push('_');
    }
    Some(s)
}

/// The file's name in the media folder.
fn upload_name(src: &Path) -> Option<String> {
    clean_part(&src.file_name()?.to_string_lossy())
}

/// `A001.mov` → `A001 (2).mov`.
fn numbered(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

fn in_dir(dir: Option<&str>, name: &str) -> String {
    dir.map_or_else(|| name.to_string(), |d| format!("{d}/{name}"))
}

/// The item's bins as a folder path (`Footage/Day 2`), None at the top level.
fn bin_dir(root: &Bin, item: ItemId) -> Option<String> {
    let mut path = Vec::new();
    if !find_bin_path(root, item, &mut path, 0) {
        return None;
    }
    let parts: Vec<String> = path.iter().filter_map(|n| clean_part(n)).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn find_bin_path(bin: &Bin, item: ItemId, path: &mut Vec<String>, depth: usize) -> bool {
    if depth > MAX_BIN_DEPTH {
        return false;
    }
    for c in &bin.children {
        match c {
            BinEntry::Item(id) if *id == item => return true,
            BinEntry::Item(_) => {}
            BinEntry::Bin(sub) => {
                path.push(sub.name.clone());
                if find_bin_path(sub, item, path, depth + 1) {
                    return true;
                }
                path.pop();
            }
        }
    }
    false
}

fn load(cache: &Path) -> UploadsFile {
    std::fs::read(cache.join(UPLOADS_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// A temporary file, then a rename, so a crash never leaves half a queue.
fn write(cache: &Path, file: &UploadsFile) -> std::io::Result<()> {
    std::fs::create_dir_all(cache)?;
    let bytes = serde_json::to_vec(file).map_err(std::io::Error::other)?;
    let tmp = cache.join(format!("{UPLOADS_FILE}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, cache.join(UPLOADS_FILE))
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn names_and_folders_the_drive_accepts() {
        assert_eq!(numbered("A001.mov", 2), "A001 (2).mov");
        assert_eq!(numbered("README", 3), "README (3)");
        assert_eq!(numbered(".hidden", 2), ".hidden (2)");
        assert_eq!(clean_part("Day\\2/x\u{7}"), Some("Day-2-x-".into()));
        assert_eq!(clean_part("  "), None);
        assert_eq!(clean_part(".."), Some("__".into()));
        assert_eq!(clean_part("Robotics.FCTEAM"), Some("Robotics.FCTEAM_".into()));
        assert_eq!(in_dir(Some("Day 2"), "A.mov"), "Day 2/A.mov");
    }
}
