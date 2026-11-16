//! [`TeamSessionController`]: the Team Projects panel's model. Every network call runs on a
//! background thread; the UI calls [`TeamSessionController::poll`] once per frame, which finishes
//! what came back (attaching a project to the session happens there, on the UI thread) and keeps
//! the open project's media downloads going.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use filmcraft_drive::{
    AppInfo, Cleanup, DriveClient, DriveConfig, DriveError, KeychainStore, LoginOptions, ProjectData, ProjectSummary, TokenStore, local_device_name, login,
    logout,
};
use filmcraft_engine::Session;
use filmcraft_engine::panels::Level;
use filmcraft_engine::team::TeamState;
use filmcraft_engine::team_media::{TeamMedia, relink_local};
use filmcraft_project::{ItemId, MediaRef};
use filmcraft_team::cache::{self, CacheIndex};
use filmcraft_team::{DRIVE_PREFIX, TeamDoc};
use serde_json::Value;

use crate::downloader::DriveDownloader;
use crate::feed::FeedState;
use crate::link::{DriveTeamLink, LinkStatus};
use crate::uploads::{TeamUploads, UploadNotes, mark_project, uploadable};

/// Where this Mac's team media lives: `~/Library/Caches/InFocus Cut/media`.
pub fn default_cache_dir() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join("Library/Caches/InFocus Cut/media"),
        None => std::env::temp_dir().join("InFocus Cut/media"),
    }
}

/// Opens `url` in the system browser (`open` on macOS, `xdg-open` elsewhere).
pub fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    // sign-in times out with a clear message if the browser never opens
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

/// What the controller needs from the app.
pub struct ControllerConfig {
    /// The Drive's address (config file or `INFOCUS_DRIVE_URL`), or why there is none.
    pub drive: Result<DriveConfig, String>,
    pub app: AppInfo,
    pub store: Arc<dyn TokenStore>,
    /// Team media cache ([`default_cache_dir`]).
    pub cache_dir: PathBuf,
    /// Opens the sign-in page.
    pub open_browser: Arc<dyn Fn(&str) + Send + Sync>,
    pub login: LoginOptions,
}

impl ControllerConfig {
    /// The desktop app's setup: Drive settings from the config file or environment, the token in
    /// the Keychain, the media cache in `~/Library/Caches`, sign-in in the system browser.
    pub fn for_app(app_version: &str, schema_version: u32) -> Result<Self, String> {
        let app = AppInfo::new(app_version, schema_version).map_err(|e| e.to_string())?;
        let device = local_device_name();
        let login =
            LoginOptions { device: if device.is_empty() { "InFocus Cut".into() } else { format!("InFocus Cut on {device}") }, ..LoginOptions::default() };
        Ok(Self {
            drive: DriveConfig::load().map_err(|e| e.to_string()),
            app,
            store: Arc::new(KeychainStore),
            cache_dir: default_cache_dir(),
            open_browser: Arc::new(open_in_browser),
            login,
        })
    }
}

/// Where sign-in stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    SignedOut,
    /// Signing in, or connecting with the stored token.
    Connecting,
    SignedIn,
}

/// The team project open in the session.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenTeamProject {
    pub id: String,
    pub name: String,
    /// `Team Projects/<name>.fcteam`.
    pub folder: String,
    pub cleanup: Option<Cleanup>,
}

struct Open {
    info: OpenTeamProject,
    media: TeamMedia,
    /// The cache index as the link sees it (`media_ref`, `resolve`).
    index: Arc<Mutex<CacheIndex>>,
    status: Arc<LinkStatus>,
    /// (revision, open sequences, Source item) when the wanted media was last worked out.
    seen: Option<(u64, Vec<ItemId>, Option<ItemId>)>,
    /// Media an export needs on this Mac.
    for_export: BTreeSet<ItemId>,
    exporting: bool,
    /// The member's own media going up to the project's media folder.
    uploads: TeamUploads,
    /// Who is uploading what (shared with the link).
    notes: Arc<Mutex<UploadNotes>>,
    /// The session revision the uploads last looked for new media at.
    scanned: Option<u64>,
}

enum Outcome {
    Connected {
        client: Arc<DriveClient>,
        me: String,
    },
    NoToken,
    SignedOut,
    Projects(Vec<ProjectSummary>),
    /// A project to attach: to the current session project (created from it, at that document
    /// epoch) or merged from the Drive.
    Ready {
        summary: ProjectSummary,
        data: Box<ProjectData>,
        current: Option<u64>,
    },
    /// The list changed on the Drive (keep): fetch it again.
    ListStale,
    Done,
    Failed {
        message: String,
        signed_out: bool,
    },
}

fn failed(e: DriveError) -> Outcome {
    Outcome::Failed { signed_out: matches!(e, DriveError::Unauthorized(_)), message: e.to_string() }
}

struct Job {
    label: &'static str,
    rx: Receiver<Outcome>,
}

/// The Team Projects panel's model. See the crate docs for the threading.
pub struct TeamSessionController {
    cfg: ControllerConfig,
    phase: Phase,
    client: Option<Arc<DriveClient>>,
    me: Option<String>,
    projects: Vec<ProjectSummary>,
    open: Option<Open>,
    jobs: Vec<Job>,
    error: Option<String>,
}

impl TeamSessionController {
    /// A controller; with a stored token it connects in the background.
    pub fn new(cfg: ControllerConfig) -> Self {
        let mut c = Self { cfg, phase: Phase::SignedOut, client: None, me: None, projects: Vec::new(), open: None, jobs: Vec::new(), error: None };
        if let Ok(drive) = c.cfg.drive.clone() {
            let (store, app) = (c.cfg.store.clone(), c.cfg.app.clone());
            c.phase = Phase::Connecting;
            c.spawn("Connecting to InFocus Drive", move || match store.load() {
                Ok(Some(token)) => connect(&drive, &token, app, None),
                Ok(None) => Outcome::NoToken,
                Err(e) => failed(e),
            });
        }
        c
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
    /// The signed-in member's Drive user name.
    pub fn user(&self) -> Option<&str> {
        self.me.as_deref()
    }
    /// What is running in the background, for a spinner.
    pub fn busy(&self) -> Option<&'static str> {
        self.jobs.first().map(|j| j.label)
    }
    /// The last failure, for the panel (stays until [`Self::clear_error`] or the next success).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn clear_error(&mut self) {
        self.error = None;
    }
    /// The team projects from the last [`Self::refresh_projects`] (with cleanup status).
    pub fn projects(&self) -> &[ProjectSummary] {
        &self.projects
    }
    pub fn open_project(&self) -> Option<&OpenTeamProject> {
        self.open.as_ref().map(|o| &o.info)
    }
    /// The open project's media downloads (progress, failures, retry).
    pub fn media(&self) -> Option<&TeamMedia> {
        self.open.as_ref().map(|o| &o.media)
    }
    pub fn media_mut(&mut self) -> Option<&mut TeamMedia> {
        self.open.as_mut().map(|o| &mut o.media)
    }
    /// The open project's uploads of this member's media (progress, failures).
    pub fn uploads(&self) -> Option<&TeamUploads> {
        self.open.as_ref().map(|o| &o.uploads)
    }
    /// Try failed downloads and uploads again.
    pub fn retry_media(&mut self) {
        if let Some(o) = self.open.as_mut() {
            o.media.retry();
            o.uploads.retry();
        }
    }
    /// Items whose file a teammate is still uploading, with the teammate's name.
    pub fn incoming_uploads(&self) -> Vec<(ItemId, String)> {
        self.open
            .as_ref()
            .map_or_else(Vec::new, |o| o.notes.lock().unwrap_or_else(PoisonError::into_inner).theirs.iter().map(|(id, by)| (ItemId(*id), by.clone())).collect())
    }
    /// True while live updates are connected (otherwise they are polled every 5 s).
    pub fn feed_live(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.status.feed_live())
    }
    /// The open project's last connection problem.
    pub fn link_error(&self) -> Option<String> {
        self.open.as_ref().and_then(|o| o.status.last_error())
    }
    /// The session's project as the Drive stores it (shared part and sequence parts, media at
    /// their `drive:/` paths through this Mac's cache index), for comparing members' copies.
    pub fn team_doc(&self, s: &Session) -> Result<TeamDoc, String> {
        let o = self.open.as_ref().ok_or("no team project is open")?;
        let index = o.media.index();
        let media_ref = |m: &MediaRef| match m {
            MediaRef::File { path } if path.starts_with(DRIVE_PREFIX) => Some(path.clone()),
            MediaRef::File { path } => index.drive_ref_of(std::path::Path::new(path)).map(str::to_string),
            _ => None,
        };
        filmcraft_team::split(&s.project, &media_ref).map_err(|e| e.to_string())
    }
    pub fn exporting(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.exporting)
    }

    /// Signs in through the browser (PKCE); the token goes to the Keychain.
    pub fn sign_in(&mut self) {
        let drive = match self.cfg.drive.clone() {
            Ok(d) => d,
            Err(e) => return self.error = Some(e),
        };
        if self.phase == Phase::Connecting {
            return;
        }
        self.phase = Phase::Connecting;
        let (store, app, opts, browser) = (self.cfg.store.clone(), self.cfg.app.clone(), self.cfg.login.clone(), self.cfg.open_browser.clone());
        self.spawn("Signing in", move || match login(&drive, store.as_ref(), &opts, &|u| browser(u)) {
            Ok(signed) => connect(&drive, &signed.token, app, Some(signed.username)),
            Err(e) => failed(e),
        });
    }

    /// Signs out: leaves the open team project (its locks are released), forgets the token here
    /// and revokes it on the Drive.
    pub fn sign_out(&mut self, s: &mut Session) {
        self.close(s);
        self.client = None;
        self.me = None;
        self.projects.clear();
        self.phase = Phase::SignedOut;
        let Ok(drive) = self.cfg.drive.clone() else { return };
        let store = self.cfg.store.clone();
        self.spawn("Signing out", move || logout(&drive, store.as_ref()).map_or_else(failed, |()| Outcome::SignedOut));
    }

    /// Leaves the open team project (locks released; the session keeps the project as it is).
    pub fn close(&mut self, s: &mut Session) {
        if self.open.take().is_some() {
            s.detach_team();
        }
    }

    pub fn refresh_projects(&mut self) {
        let Some(client) = self.client.clone() else { return };
        self.spawn("Loading team projects", move || client.list_projects().map_or_else(failed, Outcome::Projects));
    }

    /// Makes the session's project a team project called `name` on the Drive and keeps editing
    /// it as one. Media not on the Drive stays on this Mac (teammates see it offline).
    pub fn create_from_current(&mut self, s: &Session, name: &str) {
        let Some(client) = self.signed_in_client() else { return };
        if s.team.is_some() {
            return self.error = Some("This is already a team project".into());
        }
        let mut project = match project_json(s) {
            Ok(p) => p,
            Err(e) => return self.error = Some(format!("Could not prepare the project: {e}")),
        };
        // teammates see the media still on this Mac as uploading from this member
        if let Some(me) = &self.me {
            mark_project(&mut project, &uploadable(&s.project), me);
        }
        let (name, epoch) = (name.to_string(), s.document_epoch);
        self.spawn("Creating the team project", move || {
            let run = || -> Result<Outcome, DriveError> {
                let created = client.create_project(&name, Some(&project))?;
                let summary = client.list_projects()?.into_iter().find(|p| p.id == created.id).unwrap_or_else(|| ProjectSummary {
                    id: created.id.clone(),
                    name: name.clone(),
                    folder: format!("Team Projects/{name}.fcteam"),
                    locks: Default::default(),
                    last_open: 0.0,
                    cleanup: None,
                });
                let data = client.get_project(&created.id)?;
                Ok(Outcome::Ready { summary, data: Box::new(data), current: Some(epoch) })
            };
            run().unwrap_or_else(failed)
        });
    }

    /// Opens team project `id` in the session (replacing its project), from the Drive.
    pub fn open(&mut self, id: &str) {
        let Some(client) = self.signed_in_client() else { return };
        let id = id.to_string();
        self.spawn("Opening the team project", move || {
            let run = || -> Result<Outcome, DriveError> {
                let summary = client.list_projects()?.into_iter().find(|p| p.id == id).ok_or_else(|| DriveError::NotFound("team project".into()))?;
                let data = client.get_project(&id)?;
                Ok(Outcome::Ready { summary, data: Box::new(data), current: None })
            };
            run().unwrap_or_else(failed)
        });
    }

    /// "Keep" on a cleanup notice: restarts the project's 30-day clock on the Drive.
    pub fn keep(&mut self, id: &str) {
        let Some(client) = self.signed_in_client() else { return };
        let id = id.to_string();
        self.spawn("Keeping the project", move || client.open(&id).map_or_else(failed, |_| Outcome::ListStale));
    }

    /// "Keep forever" (creator or admin): cleanup never moves the project.
    pub fn set_keep_forever(&mut self, id: &str, keep: bool) {
        let Some(client) = self.signed_in_client() else { return };
        let id = id.to_string();
        self.spawn("Updating the project", move || client.keep(&id, keep).map_or_else(failed, |_| Outcome::ListStale));
    }

    /// The media items an export of `sequences` still needs downloaded (originals not on this
    /// Mac); they are fetched from now on. Export when this comes back empty.
    pub fn export_missing(&mut self, s: &Session, sequences: &[ItemId]) -> Vec<ItemId> {
        let Some(o) = self.open.as_mut() else { return Vec::new() };
        let missing: BTreeSet<ItemId> = cache::wanted(&s.project, sequences, None)
            .into_iter()
            .filter(|id| {
                s.project.item(*id).and_then(|i| i.as_media()).is_some_and(|m| matches!(&m.media, MediaRef::File { path } if path.starts_with(DRIVE_PREFIX)))
            })
            .collect();
        if missing != o.for_export {
            o.for_export = missing.clone();
            o.seen = None;
        }
        missing.into_iter().collect()
    }

    /// An export starts: downloads pause and the Drive's cleanup leaves the project alone.
    pub fn export_begin(&mut self) {
        let Some(o) = self.open.as_mut() else { return };
        o.media.pause(true);
        o.uploads.pause(true);
        o.exporting = true;
        let id = o.info.id.clone();
        let Some(client) = self.client.clone() else { return };
        self.spawn("Telling the Drive about the export", move || client.export_begin(&id).map_or_else(failed, |()| Outcome::Done));
    }

    /// The export ended (done, failed or cancelled): downloads carry on.
    pub fn export_end(&mut self) {
        let Some(o) = self.open.as_mut() else { return };
        o.media.pause(false);
        o.uploads.pause(false);
        o.exporting = false;
        o.for_export.clear();
        o.seen = None;
        let id = o.info.id.clone();
        let Some(client) = self.client.clone() else { return };
        self.spawn("Telling the Drive the export ended", move || client.export_end(&id).map_or_else(failed, |()| Outcome::Done));
    }

    /// Call once per frame: finishes background work and keeps the open project's media going.
    pub fn poll(&mut self, s: &mut Session) {
        let mut done = Vec::new();
        self.jobs.retain(|j| match j.rx.try_recv() {
            Ok(out) => {
                done.push(out);
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });
        for out in done {
            self.finish(out, s);
        }
        if self.open.is_some() && s.team.is_none() {
            // another project replaced the team project; its link (and locks) went with it
            self.open = None;
        }
        let Some(o) = self.open.as_mut() else { return };
        let sig = (s.revision, s.state.open_sequences.clone(), s.state.source_item);
        if o.seen.as_ref() != Some(&sig) {
            let mut wanted = cache::wanted(&s.project, &s.state.open_sequences, s.state.source_item);
            wanted.extend(o.for_export.iter().copied());
            o.media.ensure(wanted);
            o.seen = Some(sig);
        }
        o.media.poll(s);
        if o.scanned != Some(s.revision) {
            o.scanned = Some(s.revision);
            let queued = o.uploads.scan(&s.project, o.media.index());
            // sent again so teammates see them as uploading from this member
            s.team_push_items(&queued);
        }
        let finished = o.uploads.poll();
        for f in &finished {
            o.media.record_uploaded(s, &f.items, &f.drive_ref, &f.src, f.bytes);
        }
        {
            let mut mirror = o.index.lock().unwrap_or_else(PoisonError::into_inner);
            if *mirror != *o.media.index() {
                *mirror = o.media.index().clone();
            }
        }
        // after the index: the link now gives these items their `drive:/` path
        let items: Vec<ItemId> = finished.into_iter().flat_map(|f| f.items).collect();
        s.team_push_items(&items);
    }

    fn signed_in_client(&mut self) -> Option<Arc<DriveClient>> {
        if self.client.is_none() {
            self.error = Some("Sign in to InFocus Drive first".into());
        }
        self.client.clone()
    }

    fn spawn(&mut self, label: &'static str, work: impl FnOnce() -> Outcome + Send + 'static) {
        let (tx, rx) = channel();
        let started = std::thread::Builder::new().name("team-controller".into()).spawn(move || {
            let out =
                catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|_| Outcome::Failed { message: format!("{label} failed unexpectedly"), signed_out: false });
            let _ = tx.send(out);
        });
        match started {
            Ok(_) => self.jobs.push(Job { label, rx }),
            Err(e) => self.error = Some(format!("{label}: could not start ({e})")),
        }
    }

    fn finish(&mut self, out: Outcome, s: &mut Session) {
        match out {
            Outcome::Connected { client, me } => {
                self.client = Some(client);
                self.me = Some(me);
                self.phase = Phase::SignedIn;
                self.error = None;
                self.refresh_projects();
            }
            Outcome::NoToken => self.phase = Phase::SignedOut,
            Outcome::SignedOut => {}
            Outcome::Projects(list) => self.projects = list,
            Outcome::Ready { summary, data, current } => {
                if let Err(e) = self.attach(s, summary, *data, current) {
                    self.error = Some(e);
                }
            }
            Outcome::ListStale => self.refresh_projects(),
            Outcome::Done => {}
            Outcome::Failed { message, signed_out } => {
                if signed_out {
                    self.client = None;
                    self.me = None;
                }
                if self.client.is_none() {
                    self.phase = Phase::SignedOut;
                }
                self.error = Some(message);
            }
        }
    }

    /// Attaches a fetched project to the session (on the UI thread).
    fn attach(&mut self, s: &mut Session, summary: ProjectSummary, data: ProjectData, current: Option<u64>) -> Result<(), String> {
        let (Some(client), Some(me)) = (self.client.clone(), self.me.clone()) else { return Err("Sign in to InFocus Drive first".into()) };
        if current.is_some_and(|epoch| epoch != s.document_epoch || s.team.is_some()) {
            return Err(format!("Another project was opened while “{}” was being created; open it from the Team panel", summary.name));
        }
        let downloader = Arc::new(DriveDownloader::new(client.clone(), &summary.id, &summary.folder));
        let media = TeamMedia::new(self.cfg.cache_dir.clone(), &summary.id, downloader);
        let index = Arc::new(Mutex::new(media.index().clone()));
        let mut notes = UploadNotes::new(&me);
        notes.read_shared(&data.shared);
        let notes = Arc::new(Mutex::new(notes));
        let uploads = TeamUploads::new(self.cfg.cache_dir.clone(), &summary.id, client.clone(), notes.clone());
        let link = DriveTeamLink::start(client, &summary.id, FeedState::of(&data), index.clone()).with_upload_notes(notes.clone());
        let status = link.status();
        let revs = data.sequences.iter().map(|(id, part)| (ItemId(*id), part.rev)).collect();
        let holders = data.locks.iter().filter(|(_, l)| l.holder != me).map(|(id, l)| (ItemId(*id), l.holder.clone())).collect();
        let team = TeamState::new(Box::new(link), me, data.slot, revs, data.shared_rev, holders);
        self.close(s);
        if current.is_some() {
            s.attach_team(team).map_err(|e| e.to_string())?;
        } else {
            let doc = TeamDoc { shared: data.shared, sequences: data.sequences.into_iter().map(|(id, part)| (ItemId(id), part.json)).collect() };
            let project = filmcraft_team::merge(&doc, &|path| media.index().resolve(path)).map_err(|e| format!("Could not open “{}”: {e}", summary.name))?;
            s.open_team_project(project, team).map_err(|e| e.to_string())?;
        }
        // uploads cut short last time: the items play from the member's file again meanwhile
        for (items, src) in uploads.unfinished() {
            if src.is_file() {
                relink_local(s, &items, &src);
            }
        }
        let mut open = Open {
            info: OpenTeamProject { id: summary.id, name: summary.name, folder: summary.folder, cleanup: data.cleanup },
            media,
            index,
            status,
            seen: None,
            for_export: BTreeSet::new(),
            exporting: false,
            uploads,
            notes,
            scanned: None,
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs();
        if let Err(e) = open.media.opened(now) {
            s.log.push(Level::Warning, "team", format!("Could not update the media cache index: {e}"));
        }
        if let Err(e) = open.media.clean_up(s, now) {
            s.log.push(Level::Warning, "team", format!("Could not clean up the media cache: {e}"));
        }
        self.open = Some(open);
        self.error = None;
        self.refresh_projects();
        Ok(())
    }
}

/// Connects with a token and learns who is signed in.
fn connect(drive: &DriveConfig, token: &str, app: AppInfo, username: Option<String>) -> Outcome {
    let client = match DriveClient::connect(drive, token, app) {
        Ok(c) => Arc::new(c),
        Err(e) => return failed(e),
    };
    let me = match username.filter(|u| !u.is_empty()) {
        Some(u) => u,
        None => match client.me() {
            Ok(u) => u,
            Err(e) => return failed(e),
        },
    };
    Outcome::Connected { client, me }
}

/// The session's project as one JSON for `POST /api/team/projects`: the shared part with the
/// sequence items put back (local paths and proxies stripped).
fn project_json(s: &Session) -> Result<Value, String> {
    let doc = filmcraft_team::split(&s.project, &|_| None).map_err(|e| e.to_string())?;
    let mut project = doc.shared;
    let Some(items) = project.get_mut("items").and_then(Value::as_object_mut) else { return Err("the project has no items".into()) };
    for (id, json) in doc.sequences {
        items.insert(id.0.to_string(), json);
    }
    Ok(project)
}
