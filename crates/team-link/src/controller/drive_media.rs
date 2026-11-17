//! Footage put in the open project's media folder from the Drive website: the project list of
//! originals is fetched every [`LIST_REFRESH`], and a file no item uses on two fetches in a row
//! (so not a teammate's upload whose item is still on its way) is offered as "new on InFocus
//! Drive". Add to Project downloads those files (the same resumable, parallel downloads),
//! imports them into the project's bin and records them as Drive files, so the items go to
//! teammates with their `drive:/` paths and nothing is uploaded again.
//!
//! It is a button, not automatic: two Macs adding the same file at once would make two items,
//! and a file someone removed from the project would come back.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Instant;

use filmcraft_drive::{DRIVE_PREFIX, Expected, MediaFile, Person, media_folder};
use filmcraft_engine::Session;
use filmcraft_engine::panels::Level;
use filmcraft_project::{ItemId, MediaRef, Project};
use filmcraft_team::cache::CacheIndex;
use serde_json::json;

use super::{LIST_REFRESH, Outcome, TeamSessionController, failed};

/// Most files one Add to Project brings in.
const MAX_ADD: usize = 200;

/// What the controller knows about the open project's media folder.
#[derive(Default)]
pub(super) struct DriveMedia {
    /// When the list was last asked for.
    listed_at: Option<Instant>,
    /// Files no item used at the previous fetch.
    unused_before: BTreeSet<String>,
    /// Files no item used on the last two fetches: offered to add.
    pub(super) offered: Vec<MediaFile>,
    /// Add to Project is running.
    adding: bool,
    /// The feed's footage change count at the last fetch.
    changes_seen: u64,
    /// Set when the project closes (or this Mac signs out): a running Add to Project stops.
    stop: Arc<AtomicBool>,
}

impl Drop for DriveMedia {
    fn drop(&mut self) {
        // the open project is gone: its downloads stop using the link and the stream permits
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Shortest gap between two fetches started by footage change events (a burst of uploads
/// makes one fetch, not one each).
const MEDIA_EVENT_GAP: std::time::Duration = std::time::Duration::from_secs(3);

/// A file fetched for Add to Project: its Drive path, where it is now and its size.
pub(super) struct Fetched {
    drive_ref: String,
    path: PathBuf,
    bytes: u64,
}

/// `drive:/Team Projects/<name> Media/<path>`.
fn drive_ref(folder: &str, path: &str) -> Option<String> {
    Some(format!("{DRIVE_PREFIX}{}/{path}", media_folder(folder)?))
}

/// The Drive paths the project's media items use (on the Drive, or on this Mac as a cached or
/// uploaded copy of a Drive file).
fn used(project: &Project, index: &CacheIndex, id: &str) -> BTreeSet<String> {
    project
        .items
        .values()
        .filter_map(|i| i.as_media())
        .filter_map(|m| match &m.media {
            MediaRef::File { path } if path.starts_with(DRIVE_PREFIX) => Some(path.clone()),
            MediaRef::File { path } => index.drive_ref_of(id, std::path::Path::new(path)).map(str::to_string),
            _ => None,
        })
        .collect()
}

impl TeamSessionController {
    /// Files in the open project's media folder that no item uses yet: (path in the media
    /// folder, size).
    pub fn drive_only(&self) -> Vec<(String, u64)> {
        self.open.as_ref().map_or_else(Vec::new, |o| o.drive_media.offered.iter().map(|f| (f.path.clone(), f.size)).collect())
    }

    /// Asks the Drive for the open project's originals again.
    pub fn refresh_media(&mut self) {
        let Some(client) = self.client.clone() else { return };
        let Some(o) = self.open.as_mut() else { return };
        o.drive_media.listed_at = Some(Instant::now());
        o.drive_media.changes_seen = o.status.media_changes();
        let id = o.info.id.clone();
        self.spawn_quiet("Checking the project's footage", move || {
            // who is online comes from the dashboard's overview; an older Drive without it is fine
            let presence = client.presence(&id).ok();
            client.list_media(&id).map_or_else(failed, |files| Outcome::MediaList(id, files, presence))
        });
    }

    /// Lists the originals every [`LIST_REFRESH`] while a project is open, and soon after the
    /// change feed says footage changed (a teammate's upload from the app, the dashboard or the
    /// Drive website).
    pub(super) fn refresh_media_now_and_then(&mut self) {
        let due = self.open.as_ref().is_some_and(|o| {
            let dm = &o.drive_media;
            let changed = o.status.media_changes() != dm.changes_seen && dm.listed_at.is_none_or(|at| at.elapsed() >= MEDIA_EVENT_GAP);
            !dm.adding && (changed || dm.listed_at.is_none_or(|at| at.elapsed() >= LIST_REFRESH))
        });
        if due && self.jobs.is_empty() {
            self.refresh_media();
        }
    }

    /// The list came back: work out which files no item uses.
    pub(super) fn media_listed(&mut self, s: &Session, id: &str, files: Vec<MediaFile>, presence: Option<Vec<Person>>) {
        if let Some(people) = presence {
            self.presence = Some((id.to_string(), people));
            self.apply_presence();
        }
        // the list is the freshest count of the project's footage: the panel shows the same
        // numbers as the dashboard without waiting for the next project list
        let (count, bytes) = (files.len() as u64, files.iter().fold(0u64, |acc, f| acc.saturating_add(f.size)));
        for p in self.projects.iter_mut().filter(|p| p.id == id) {
            p.media_count = count;
            p.media_bytes = bytes;
        }
        let Some(o) = self.open.as_mut().filter(|o| o.info.id == id) else { return };
        let used = used(&s.project, o.media.index(), id);
        let folder = o.info.folder.clone();
        let unused: Vec<MediaFile> = files.into_iter().filter(|f| drive_ref(&folder, &f.path).is_some_and(|r| !used.contains(&r))).collect();
        let dm = &mut o.drive_media;
        dm.offered = unused.iter().filter(|f| dm.unused_before.contains(&f.path)).cloned().collect();
        dm.unused_before = unused.into_iter().map(|f| f.path).collect();
    }

    /// Copies who is online and when each member last opened the project into the list.
    pub(super) fn apply_presence(&mut self) {
        let Some((id, people)) = &self.presence else { return };
        for p in self.projects.iter_mut().filter(|p| p.id == *id) {
            for m in &mut p.members {
                if let Some(seen) = people.iter().find(|q| q.username == m.username) {
                    m.online = seen.online;
                    m.last_seen = seen.last_seen;
                }
            }
        }
    }

    /// "Add to Project": downloads the offered files, then imports them (see the module docs).
    pub fn add_drive_footage(&mut self) {
        let Some(client) = self.signed_in_client() else { return };
        let Some(o) = self.open.as_mut() else { return };
        if o.drive_media.adding || o.drive_media.offered.is_empty() {
            return;
        }
        o.drive_media.adding = true;
        let stop = o.drive_media.stop.clone();
        let (id, folder) = (o.info.id.clone(), o.info.folder.clone());
        let jobs: Vec<(MediaFile, String, PathBuf)> = o
            .drive_media
            .offered
            .iter()
            .take(MAX_ADD)
            .filter_map(|f| {
                let r = drive_ref(&folder, &f.path)?;
                let dest = o.media.fetch_dest(&r);
                Some((f.clone(), r, dest))
            })
            .collect();
        self.spawn("Adding footage from InFocus Drive", move || {
            let mut done = Vec::new();
            for (file, drive_ref, dest) in jobs {
                if stop.load(Ordering::Relaxed) {
                    return Outcome::DriveFetched(id, done, None);
                }
                if let Some(dir) = dest.parent()
                    && let Err(e) = std::fs::create_dir_all(dir)
                {
                    return Outcome::Failed { message: format!("Could not make the media cache folder: {e}"), signed_out: false };
                }
                let expected = Expected { size: Some(file.size).filter(|s| *s > 0), sha256: None };
                match client.download_media(&id, &file.path, &dest, &expected, &mut |_, _| !stop.load(Ordering::Relaxed)) {
                    Ok(bytes) => done.push(Fetched { drive_ref, path: dest, bytes }),
                    Err(_) if stop.load(Ordering::Relaxed) => return Outcome::DriveFetched(id, done, None),
                    Err(e) => {
                        log::warn!("could not fetch a file from the media folder: {e}");
                        let message = match failed(e) {
                            Outcome::Failed { message, .. } => message,
                            _ => String::new(),
                        };
                        return Outcome::DriveFetched(id, done, Some(message));
                    }
                }
            }
            Outcome::DriveFetched(id, done, None)
        });
    }

    /// The fetched files are on this Mac: import them and record them as Drive files.
    pub(super) fn drive_fetched(&mut self, s: &mut Session, id: &str, fetched: Vec<Fetched>, error: Option<String>) {
        let Some(o) = self.open.as_mut().filter(|o| o.info.id == id) else { return };
        o.drive_media.adding = false;
        if let Some(e) = error {
            self.error = Some(format!("Some footage didn't download: {e}"));
        }
        let Some(o) = self.open.as_mut() else { return };
        if fetched.is_empty() || s.team.is_none() {
            return;
        }
        let paths: Vec<String> = fetched.iter().map(|f| f.path.to_string_lossy().into_owned()).collect();
        if let Err(e) = s.execute("file.import", json!({ "paths": paths })) {
            self.error = Some(format!("Could not add the footage: {e}"));
            return;
        }
        let mut pushed: Vec<ItemId> = Vec::new();
        for f in &fetched {
            let local = f.path.to_string_lossy();
            let items: Vec<ItemId> = s
                .project
                .items
                .iter()
                .filter(|(_, i)| i.as_media().is_some_and(|m| matches!(&m.media, MediaRef::File { path } if *path == local)))
                .map(|(id, _)| *id)
                .collect();
            o.media.record_fetched(s, &items, &f.drive_ref, &f.path, f.bytes);
            pushed.extend(items);
        }
        *o.index.lock().unwrap_or_else(PoisonError::into_inner) = o.media.index().clone();
        // the link now gives these items their `drive:/` path
        s.team_push_items(&pushed);
        let added: BTreeSet<&str> = fetched.iter().map(|f| f.drive_ref.as_str()).collect();
        let folder = o.info.folder.clone();
        o.drive_media.offered.retain(|f| drive_ref(&folder, &f.path).is_none_or(|r| !added.contains(r.as_str())));
        s.log.push(Level::Info, "team", format!("Added {} files from InFocus Drive to the project", pushed.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_refs_live_in_the_media_folder() {
        assert_eq!(drive_ref("Team Projects/Robotics.fcteam", "Day 1/A.mov").as_deref(), Some("drive:/Team Projects/Robotics Media/Day 1/A.mov"));
        assert_eq!(drive_ref("Team Projects/Robotics", "A.mov"), None);
    }

    #[test]
    fn closing_the_project_stops_add_to_project() {
        let dm = DriveMedia::default();
        let stop = dm.stop.clone();
        assert!(!stop.load(Ordering::Relaxed));
        drop(dm);
        assert!(stop.load(Ordering::Relaxed), "a running Add to Project sees the stop and quits");
    }
}
