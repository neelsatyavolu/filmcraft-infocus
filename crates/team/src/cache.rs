//! This Mac's cache of team media: what is downloaded where, what to remove, and what to fetch.
//!
//! Layout: `<cache>/<fingerprint>/original/<file name>` (the downloaded original) and
//! `<cache>/<fingerprint>/proxy/` (this Mac's proxy), indexed by `<cache>/index.json`. The app
//! uses `~/Library/Caches/InFocus Cut/media` as `<cache>`.
//!
//! Rules ([`eviction_plan`]): media of a project not opened on this Mac for 30 days goes; above the
//! size cap (200 GB) originals go before proxies, least recently used first; media of an open
//! sequence never goes. [`wanted`] says which media an editor needs: only what the open sequences
//! (nested ones included) and the Source monitor use.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use filmcraft_project::{ItemId, ItemKind, MediaIdentity, MediaRef, Project};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::doc::{DRIVE_PREFIX, LocalMedia};

/// Default size cap of the media cache (200 GB).
pub const CACHE_CAP_BYTES: u64 = 200_000_000_000;
/// A project's media leaves this Mac this long after it was last opened here (30 days).
pub const PROJECT_IDLE_SECS: u64 = 30 * 24 * 60 * 60;
/// The index file inside the cache folder.
pub const INDEX_FILE: &str = "index.json";
/// Folder of the downloaded original inside a fingerprint folder.
pub const ORIGINAL_DIR: &str = "original";
/// Folder of this Mac's proxy inside a fingerprint folder.
pub const PROXY_DIR: &str = "proxy";

const MAX_FINGERPRINT_LEN: usize = 64;
const MAX_WALK_DEPTH: usize = 256;

/// A cache key: a media file's identity (`<content hash>-<size>`), or for media without one a hash
/// of its Drive path (`p-<hash>`). Only lowercase hex digits, digits and `-`, so it is always a
/// safe folder name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Fingerprint(String);

impl Fingerprint {
    /// The key of a file with this identity.
    pub fn of(identity: &MediaIdentity) -> Self {
        Self(format!("{:016x}-{}", identity.fingerprint, identity.size))
    }

    /// The key of a media file known only by its `drive:/` path (FNV-1a 64 of the path).
    pub fn of_drive_path(drive_ref: &str) -> Self {
        let hash = drive_ref.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
        Self(format!("p-{hash:016x}"))
    }

    /// The key of a media item: its identity when it has one, else its Drive path.
    pub fn for_media(identity: Option<&MediaIdentity>, drive_ref: &str) -> Self {
        identity.map_or_else(|| Self::of_drive_path(drive_ref), Self::of)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `<cache>/<fingerprint>`.
    pub fn dir(&self, cache: &Path) -> PathBuf {
        cache.join(&self.0)
    }
}

impl TryFrom<String> for Fingerprint {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        let ok = !s.is_empty()
            && s.len() <= MAX_FINGERPRINT_LEN
            && s.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-')
            && !s.starts_with('-');
        if ok { Ok(Self(s)) } else { Err(format!("bad cache key {s:?}")) }
    }
}

impl From<Fingerprint> for String {
    fn from(f: Fingerprint) -> String {
        f.0
    }
}

/// One cached file and its size.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedFile {
    pub path: PathBuf,
    pub bytes: u64,
}

/// What this Mac has of one media file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Its `drive:/` path in each team project that uses it (to give its items their Drive path,
    /// and to undo a link when the file goes): the same file is a different path in each
    /// project's media folder.
    #[serde(default)]
    pub drive_refs: BTreeMap<String, String>,
    /// An older index's single `drive:/` path; [`CacheIndex::load`] moves it into `drive_refs`.
    #[serde(default, rename = "drive_ref", skip_serializing)]
    pub legacy_drive_ref: Option<String>,
    #[serde(default)]
    pub original: Option<CachedFile>,
    #[serde(default)]
    pub proxy: Option<CachedFile>,
    /// Unix seconds of the last download, link or use.
    #[serde(default)]
    pub last_used: u64,
    /// Team project ids that use it.
    #[serde(default)]
    pub projects: BTreeSet<String>,
    /// The original is the member's own file that this Mac uploaded (it lives where they imported
    /// it from, outside the cache): it is never removed and does not count towards the size cap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
}

impl CacheEntry {
    fn bytes(&self) -> u64 {
        self.removable_original().map_or(0, |f| f.bytes).saturating_add(self.proxy.as_ref().map_or(0, |f| f.bytes))
    }

    /// The original, unless it is the member's own uploaded file.
    fn removable_original(&self) -> Option<&CachedFile> {
        self.original.as_ref().filter(|_| !self.external)
    }
}

/// The cache index (`<cache>/index.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheIndex {
    #[serde(default)]
    pub entries: BTreeMap<Fingerprint, CacheEntry>,
    /// Unix seconds each team project was last opened on this Mac.
    #[serde(default)]
    pub project_last_open: BTreeMap<String, u64>,
}

impl CacheIndex {
    /// The index in `cache`, or an empty one when there is none or it is damaged (the files it
    /// listed are then only removed by hand; nothing is lost from the project).
    pub fn load(cache: &Path) -> Self {
        let mut idx: Self = std::fs::read(cache.join(INDEX_FILE)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        idx.migrate();
        idx
    }

    /// An older index kept one `drive:/` path per file, the last project's: it is taken for every
    /// project the file lists. Where that was another project's path, that project's items
    /// don't resolve to it, so the file is downloaded (or uploaded) again for it, which records
    /// the right path.
    fn migrate(&mut self) {
        for e in self.entries.values_mut() {
            if let Some(r) = e.legacy_drive_ref.take()
                && e.drive_refs.is_empty()
            {
                e.drive_refs = e.projects.iter().map(|p| (p.clone(), r.clone())).collect();
            }
        }
    }

    /// Writes the index to `cache` (a temporary file, then a rename, so a crash never leaves half
    /// an index).
    pub fn save(&self, cache: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(cache)?;
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let tmp = cache.join(format!("{INDEX_FILE}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, cache.join(INDEX_FILE))
    }

    /// Bytes of every cached file.
    pub fn total_bytes(&self) -> u64 {
        self.entries.values().fold(0u64, |acc, e| acc.saturating_add(e.bytes()))
    }

    /// `project` was opened on this Mac at `now`.
    pub fn opened(&mut self, project: &str, now: u64) {
        self.project_last_open.insert(project.to_string(), now);
    }

    fn entry(&mut self, fp: &Fingerprint, project: &str, drive_ref: &str, now: u64) -> &mut CacheEntry {
        let e = self.entries.entry(fp.clone()).or_default();
        e.projects.insert(project.to_string());
        e.drive_refs.insert(project.to_string(), drive_ref.to_string());
        e.last_used = e.last_used.max(now);
        e
    }

    /// A downloaded original.
    pub fn record_original(&mut self, fp: &Fingerprint, project: &str, drive_ref: &str, file: CachedFile, now: u64) {
        let e = self.entry(fp, project, drive_ref, now);
        e.original = Some(file);
        e.external = false;
    }

    /// An original this Mac uploaded from the member's own file at `file.path`: the file stands in
    /// for a download (so it is never fetched again) but is never removed.
    pub fn record_uploaded(&mut self, fp: &Fingerprint, project: &str, drive_ref: &str, file: CachedFile, now: u64) {
        let e = self.entry(fp, project, drive_ref, now);
        e.original = Some(file);
        e.external = true;
    }

    /// A finished proxy.
    pub fn record_proxy(&mut self, fp: &Fingerprint, project: &str, drive_ref: &str, file: CachedFile, now: u64) {
        self.entry(fp, project, drive_ref, now).proxy = Some(file);
    }

    /// Marks media as used at `now` (it then goes last when space runs out).
    pub fn touch(&mut self, fp: &Fingerprint, now: u64) {
        if let Some(e) = self.entries.get_mut(fp) {
            e.last_used = e.last_used.max(now);
        }
    }

    /// Forgets removed files; an entry with neither file left goes too.
    pub fn forget(&mut self, removed: &BTreeSet<PathBuf>) {
        for e in self.entries.values_mut() {
            if e.original.as_ref().is_some_and(|f| removed.contains(&f.path)) {
                e.original = None;
            }
            if e.proxy.as_ref().is_some_and(|f| removed.contains(&f.path)) {
                e.proxy = None;
            }
        }
        self.entries.retain(|_, e| e.original.is_some() || e.proxy.is_some());
    }

    /// This Mac's copies of a `drive:/` media file.
    pub fn resolve(&self, drive_ref: &str) -> LocalMedia {
        self.entries.values().find(|e| e.drive_refs.values().any(|r| r == drive_ref)).map_or_else(LocalMedia::default, |e| LocalMedia {
            original: e.original.as_ref().map(|f| f.path.clone()),
            proxy: e.proxy.as_ref().map(|f| f.path.clone()),
        })
    }

    /// The `drive:/` path in team project `project` of a cached original or proxy (None: not on
    /// the Drive for that project, even when another project has it).
    pub fn drive_ref_of(&self, project: &str, local: &Path) -> Option<&str> {
        self.entries
            .values()
            .filter(|e| e.original.as_ref().is_some_and(|f| f.path == local) || e.proxy.as_ref().is_some_and(|f| f.path == local))
            .find_map(|e| e.drive_refs.get(project).map(String::as_str))
    }
}

/// Files to remove, in order: everything of media whose projects were all last opened on this
/// Mac 30 days or more before `now`; then, while the cache is over `cap_bytes`, originals
/// (least recently used first), then proxies. Media in `in_use` (used by an open sequence or the
/// Source monitor) is never removed, and neither is an original this Mac uploaded from the
/// member's own file ([`CacheIndex::record_uploaded`]).
pub fn eviction_plan(idx: &CacheIndex, cap_bytes: u64, now: u64, in_use: &BTreeSet<Fingerprint>) -> Vec<PathBuf> {
    let idle = |e: &CacheEntry| {
        let last = |p: &String| idx.project_last_open.get(p).copied().unwrap_or(e.last_used);
        let newest = e.projects.iter().map(last).max().unwrap_or(e.last_used);
        now.saturating_sub(newest) >= PROJECT_IDLE_SECS
    };
    let mut plan = Vec::new();
    let mut total = idx.total_bytes();
    let mut keep: Vec<(&Fingerprint, &CacheEntry)> = Vec::new();
    for (fp, e) in &idx.entries {
        if in_use.contains(fp) {
            continue;
        }
        if idle(e) {
            for f in e.removable_original().into_iter().chain(e.proxy.iter()) {
                plan.push(f.path.clone());
                total = total.saturating_sub(f.bytes);
            }
        } else {
            keep.push((fp, e));
        }
    }
    keep.sort_by_key(|(fp, e)| (e.last_used, *fp));
    let originals_then_proxies: [fn(&CacheEntry) -> Option<CachedFile>; 2] = [|e| e.removable_original().cloned(), |e| e.proxy.clone()];
    for pick in originals_then_proxies {
        for (_, e) in &keep {
            if total <= cap_bytes {
                return plan;
            }
            if let Some(f) = pick(e) {
                total = total.saturating_sub(f.bytes);
                plan.push(f.path);
            }
        }
    }
    plan
}

/// The media items an editor needs on this Mac: the file media used by `open_sequences` and by
/// `source` (the Source monitor's item), through nested sequences, multicam sources and subclips
/// at any depth.
pub fn wanted(project: &Project, open_sequences: &[ItemId], source: Option<ItemId>) -> BTreeSet<ItemId> {
    let mut out = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut todo: Vec<ItemId> = open_sequences.iter().copied().chain(source).collect();
    while let Some(id) = todo.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(item) = project.item(id) else { continue };
        match &item.kind {
            ItemKind::Media(m) => {
                if matches!(m.media, MediaRef::File { .. }) {
                    out.insert(id);
                }
            }
            ItemKind::Subclip { parent, .. } => todo.push(*parent),
            ItemKind::Sequence(_) => {
                if let Ok(v) = serde_json::to_value(item) {
                    referenced(&v, &mut todo, 0);
                }
            }
            _ => {}
        }
    }
    out
}

/// Every id a sequence part names as a clip's `item` or a `parent`, the references the Drive
/// itself checks.
fn referenced(v: &Value, out: &mut Vec<ItemId>, depth: usize) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                if let (true, Some(id)) = (k == "item" || k == "parent", c.as_u64()) {
                    out.push(ItemId(id));
                }
                referenced(c, out, depth + 1);
            }
        }
        Value::Array(a) => a.iter().for_each(|c| referenced(c, out, depth + 1)),
        _ => {}
    }
}

/// The `drive:/` path of a media item that still has to be downloaded (its file is on the Drive,
/// not on this Mac yet).
pub fn drive_ref(project: &Project, item: ItemId) -> Option<&str> {
    match &project.item(item)?.as_media()?.media {
        MediaRef::File { path } if path.starts_with(DRIVE_PREFIX) => Some(path),
        _ => None,
    }
}
