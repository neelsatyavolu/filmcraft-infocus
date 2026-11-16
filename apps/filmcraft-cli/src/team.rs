//! `filmcraft-cli team …`: a scriptable InFocus Cut team member, thin over
//! `filmcraft-team-link`'s [`TeamSessionController`]. Used by `scripts/team-e2e.sh`.
//!
//! A member's state lives in a folder (`--dir`, or `INFOCUS_TEAM_DIR`): the Drive address, the
//! `ifd_` token, the device name and the media cache. `team login` fills it; `team session` then
//! reads one command per line on stdin and answers each with one JSON line on stdout, polling the
//! session every 20 ms in between like the app's frame loop (so locks are renewed and edits pushed
//! as in the app). Locks live in that process; the one-shot commands (`team create`, `team open`,
//! `team status`, …) are a session with one command, synced before it exits.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use filmcraft_drive::{AppInfo, DriveConfig, DriveError, LoginOptions, TokenStore};
use filmcraft_engine::team::SequenceStatus;
use filmcraft_engine::{Event, Session};
use filmcraft_project::{ItemId, MediaRef};
use filmcraft_team_link::{ControllerConfig, Phase, TeamSessionController};
use serde_json::{Value, json};

use crate::args::Args;

const TOKEN_ENV: &str = "INFOCUS_DRIVE_TOKEN";
const DIR_ENV: &str = "INFOCUS_TEAM_DIR";
const TICK: Duration = Duration::from_millis(20);
/// How long a wait (`open`, `lock`, `sync`, …) may take before it fails.
const WAIT: Duration = Duration::from_secs(90);
/// `sync` returns once nothing has changed for this long.
const QUIET: Duration = Duration::from_millis(1500);
/// Project-level keys that differ between members by design (id slots, the local project name).
const PER_MEMBER: &[&str] = &["next_id", "id_slot", "name"];

/// The token in `<dir>/token` (mode 0600 on Unix).
struct FileStore(PathBuf);

impl TokenStore for FileStore {
    fn load(&self) -> filmcraft_drive::Result<Option<String>> {
        match std::fs::read_to_string(&self.0) {
            Ok(t) => Ok(Some(t.trim().to_string()).filter(|t| !t.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(DriveError::Io(e)),
        }
    }
    fn save(&self, token: &str) -> filmcraft_drive::Result<()> {
        write_private(&self.0, token).map_err(DriveError::Io)
    }
    fn clear(&self) -> filmcraft_drive::Result<()> {
        match std::fs::remove_file(&self.0) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(DriveError::Io(e)),
            _ => Ok(()),
        }
    }
}

fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(path)?;
    // the mode above only applies to a new file: one left readable is made private too
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    f.write_all(text.as_bytes())
}

pub(crate) fn read(dir: &Path, name: &str) -> Result<String, String> {
    std::fs::read_to_string(dir.join(name))
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("{}: {e} (run `filmcraft-cli team login` first)", dir.join(name).display()))
}

/// `filmcraft-cli team <command>`.
pub async fn cli_team(a: &Args) {
    let r = match a.pos(1) {
        Some("login") => login(a),
        Some("session") => session(a),
        Some("bench") => crate::team_bench::bench(a),
        Some(cmd) => one_shot(a, cmd),
        None => Err("missing team command (login, session, create, open, list, status, …)".into()),
    };
    if let Err(e) = r {
        crate::fail(e);
    }
}

pub(crate) fn dir_of(a: &Args) -> Result<PathBuf, String> {
    a.opt("--dir")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(DIR_ENV).map(PathBuf::from))
        .ok_or_else(|| format!("give the member folder with --dir or {DIR_ENV}"))
}

/// `team login --drive URL (--token-file F | INFOCUS_DRIVE_TOKEN) [--device NAME]`: checks the
/// token against the Drive and keeps it in the member folder.
fn login(a: &Args) -> Result<(), String> {
    let dir = dir_of(a)?;
    let url = a.opt("--drive").map(str::to_string).or_else(|| std::env::var(filmcraft_drive::DRIVE_URL_ENV).ok()).ok_or("give the Drive with --drive URL")?;
    let token = match a.opt("--token-file") {
        Some(f) => std::fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?,
        None => std::env::var(TOKEN_ENV).map_err(|_| format!("give the token with --token-file or {TOKEN_ENV}"))?,
    };
    let token = token.trim();
    let device = a.opt("--device").unwrap_or("filmcraft-cli");
    let cfg = DriveConfig::new(&url, None).map_err(|e| e.to_string())?;
    let client = filmcraft_drive::DriveClient::connect(&cfg, token, app_info(device)?).map_err(|e| e.to_string())?;
    let me = client.me().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    FileStore(dir.join("token")).save(token).map_err(|e| e.to_string())?;
    for (name, text) in [("drive_url", cfg.drive_url.as_str()), ("device", device)] {
        std::fs::write(dir.join(name), text).map_err(|e| format!("{name}: {e}"))?;
    }
    outln!("{}", json!({"ok": true, "user": me}));
    Ok(())
}

pub(crate) fn app_info(device: &str) -> Result<AppInfo, String> {
    AppInfo::new(env!("CARGO_PKG_VERSION"), filmcraft_format::SCHEMA_VERSION).map(|i| i.with_device(device)).map_err(|e| e.to_string())
}

/// A signed-in member: the controller and its session.
struct Member {
    ctrl: TeamSessionController,
    s: Session,
    toasts: Vec<String>,
}

impl Member {
    fn connect(dir: &Path) -> Result<Self, String> {
        let drive = DriveConfig::new(&read(dir, "drive_url")?, None).map_err(|e| e.to_string());
        let cfg = ControllerConfig {
            drive,
            app: app_info(&read(dir, "device")?)?,
            store: Arc::new(FileStore(dir.join("token"))),
            cache_dir: dir.join("cache"),
            open_browser: Arc::new(|_| {}),
            login: LoginOptions::default(),
        };
        let mut m = Member { ctrl: TeamSessionController::new(cfg), s: Session::default(), toasts: Vec::new() };
        m.wait("sign-in", |m| match m.ctrl.phase() {
            Phase::SignedIn => Some(Ok(Value::Null)),
            Phase::SignedOut => Some(Err(m.ctrl.error().unwrap_or("not signed in").to_string())),
            Phase::Connecting => None,
        })?;
        Ok(m)
    }

    /// One frame: the controller and the session's team side.
    fn tick(&mut self) {
        self.ctrl.poll(&mut self.s);
        self.s.poll_persistence();
        for ev in self.s.drain_events() {
            if let Event::Toast { message, .. } = ev {
                crate::diag(format_args!("team: {message}"));
                self.toasts.push(message);
            }
        }
    }

    /// Ticks until `done` answers, or fails after [`WAIT`].
    fn wait(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> Option<Result<Value, String>>) -> Result<Value, String> {
        let start = Instant::now();
        loop {
            self.tick();
            if let Some(r) = done(self) {
                return r;
            }
            if start.elapsed() > WAIT {
                return Err(format!("timed out waiting for {what}"));
            }
            std::thread::sleep(TICK);
        }
    }

    /// Runs a controller request and waits for `ready`, failing on a new controller error.
    fn request(&mut self, what: &str, start: impl FnOnce(&mut Self), ready: impl Fn(&Self) -> bool) -> Result<Value, String> {
        self.ctrl.clear_error();
        start(self);
        self.wait(what, |m| if ready(m) { Some(Ok(Value::Null)) } else { m.ctrl.error().map(|e| Err(e.to_string())) })
    }

    fn open_id(&self) -> Option<String> {
        self.ctrl.open_project().map(|o| o.id.clone())
    }

    fn seq(&self, word: &str) -> Result<ItemId, String> {
        let id = match word.parse::<u64>() {
            Ok(n) => ItemId(n),
            Err(_) => self
                .s
                .project
                .items
                .values()
                .find(|i| i.as_sequence().is_some() && i.name == word)
                .map(|i| i.id)
                .ok_or_else(|| format!("no sequence `{word}`"))?,
        };
        if self.s.project.sequence(id).is_none() {
            return Err(format!("item {} is not a sequence", id.0));
        }
        Ok(id)
    }

    fn sequences(&self) -> Vec<ItemId> {
        self.s.project.items.values().filter(|i| i.as_sequence().is_some()).map(|i| i.id).collect()
    }

    /// Nothing left to send or download, and no change for [`QUIET`].
    fn sync(&mut self) -> Result<Value, String> {
        let mut last = (self.s.revision, Instant::now());
        self.wait("sync", |m| {
            let t = m.s.team.as_ref()?;
            if let Some(p) = &t.problem {
                return Some(Err(p.clone()));
            }
            let busy = !t.idle() || m.ctrl.uploads().is_some_and(|u| u.busy()) || m.ctrl.media().is_some_and(|d| !d.progress().is_empty());
            if busy || m.s.revision != last.0 {
                last = (m.s.revision, Instant::now());
                return None;
            }
            (last.1.elapsed() >= QUIET).then(|| Ok(json!({"revision": m.s.revision})))
        })
    }

    fn status(&self) -> Value {
        let t = self.s.team.as_ref();
        let sequences: Vec<Value> = self
            .sequences()
            .into_iter()
            .map(|id| {
                let name = self.s.project.item(id).map(|i| i.name.clone()).unwrap_or_default();
                let (state, holder) = match self.s.team_status(id) {
                    Some(SequenceStatus::Editing { unsynced }) => (if unsynced { "editing-unsynced" } else { "editing" }, None),
                    Some(SequenceStatus::Requested) => ("requested", None),
                    Some(SequenceStatus::ReadOnly { holder }) => ("read-only", Some(holder)),
                    Some(SequenceStatus::Free) | None => ("free", None),
                };
                json!({"id": id.0, "name": name, "status": state, "holder": holder})
            })
            .collect();
        let media: Vec<Value> = self
            .s
            .project
            .items
            .values()
            .filter_map(|i| {
                let m = i.as_media()?;
                let path = match &m.media {
                    MediaRef::File { path } => Some(path.clone()),
                    _ => None,
                };
                Some(json!({"id": i.id.0, "name": i.name, "path": path, "in_bin": self.s.project.root.parent_of(i.id).is_some() || self.s.project.root.children.contains(&filmcraft_project::BinEntry::Item(i.id))}))
            })
            .collect();
        json!({
            "user": self.ctrl.user(),
            "project": self.ctrl.open_project().map(|o| json!({"id": o.id, "name": o.name, "folder": o.folder})),
            "slot": t.map(|t| t.slot),
            "idle": t.is_some_and(|t| t.idle()),
            "feed_live": self.ctrl.feed_live(),
            "sequences": sequences,
            "media": media,
            "uploads": self.ctrl.uploads().map(|u| u.progress().len()).unwrap_or(0),
            "upload_failures": self.ctrl.uploads().map(|u| u.failures()).unwrap_or_default(),
            "downloads": self.ctrl.media().map(|d| d.progress().len()).unwrap_or(0),
            "download_failures": self.ctrl.media().map(|d| d.failures().into_iter().map(|(id, e)| format!("{}: {e}", id.0)).collect::<Vec<_>>()).unwrap_or_default(),
            "messages": self.toasts,
        })
    }

    /// The project as the Drive stores it, without the fields that are per member by design.
    fn split(&self) -> Result<Value, String> {
        let doc = self.ctrl.team_doc(&self.s)?;
        let mut shared = doc.shared;
        if let Some(o) = shared.as_object_mut() {
            for k in PER_MEMBER {
                o.remove(*k);
            }
        }
        let sequences: serde_json::Map<String, Value> = doc.sequences.into_iter().map(|(id, v)| (id.0.to_string(), v)).collect();
        Ok(json!({"shared": shared, "sequences": sequences}))
    }

    /// Runs one session command line.
    fn run(&mut self, line: &str) -> Result<Value, String> {
        let line = line.trim();
        let (cmd, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(c, r)| (c, r.trim()));
        let words: Vec<&str> = rest.split_whitespace().collect();
        let word = |i: usize| words.get(i).copied().ok_or_else(|| format!("`{cmd}` needs more arguments"));
        match cmd {
            "create" => {
                // `create <name> <project file>` (the file path may contain spaces)
                let name = word(0)?;
                let path =
                    rest.split_once(char::is_whitespace).map(|(_, p)| p.trim()).filter(|p| !p.is_empty()).ok_or("`create` needs a name and a project file")?;
                self.s.execute("file.open", json!({"path": path})).map_err(|e| e.to_string())?;
                self.request("the project to be created", |m| m.ctrl.create_from_current(&m.s, name), |m| m.ctrl.open_project().is_some())?;
                self.sync()?;
                Ok(json!({"id": self.open_id()}))
            }
            "open" => {
                let id = word(0)?.to_string();
                let want = id.clone();
                self.request("the project to open", |m| m.ctrl.open(&id), |m| m.open_id().as_deref() == Some(want.as_str()))?;
                self.sync()?;
                Ok(self.status())
            }
            "list" => {
                self.ctrl.refresh_projects();
                self.wait("the project list", |m| m.ctrl.busy().is_none().then_some(Ok(Value::Null)))?;
                let role = |p: &filmcraft_drive::ProjectSummary| format!("{:?}", p.role).to_lowercase();
                Ok(json!(
                    self.ctrl
                        .projects()
                        .iter()
                        .map(|p| json!({"id": p.id, "name": p.name, "folder": p.folder, "role": role(p), "invited_by": p.invited_by}))
                        .collect::<Vec<_>>()
                ))
            }
            "invite" => {
                // `invite <id> <user name>…`
                let id = word(0)?.to_string();
                let users: Vec<String> = words.iter().skip(1).map(|u| (*u).to_string()).collect();
                if users.is_empty() {
                    return Err("`invite` needs a project id and user names".into());
                }
                self.ctrl.invite(&id, &users);
                self.wait("the invitations", |m| {
                    m.ctrl.busy().is_none().then(|| m.ctrl.error().map_or(Ok(json!({"invited": users.len()})), |e| Err(e.to_string())))
                })
            }
            "join" => {
                // `join <id>`: accept the invitation; the project opens
                let id = word(0)?.to_string();
                let want = id.clone();
                self.request("the project to join", |m| m.ctrl.join(&id), |m| m.open_id().as_deref() == Some(want.as_str()))?;
                self.sync()?;
                Ok(self.status())
            }
            "lock" => {
                let seq = self.seq(word(0)?)?;
                self.s.team_take_lock(seq).map_err(|e| e.to_string())?;
                self.wait("the lock", |m| match m.s.team_status(seq) {
                    Some(SequenceStatus::Editing { .. }) => Some(Ok(json!({"seq": seq.0, "locked": true}))),
                    Some(SequenceStatus::ReadOnly { holder }) => Some(Err(format!("refused: held by {holder}"))),
                    Some(SequenceStatus::Free) => Some(Err(m.toasts.last().cloned().unwrap_or_else(|| "the lock was not granted".into()))),
                    _ => None,
                })
            }
            "release" => {
                let seq = self.seq(word(0)?)?;
                self.s.team_release_lock(seq).map_err(|e| e.to_string())?;
                self.wait("the release", |m| (!matches!(m.s.team_status(seq), Some(SequenceStatus::Editing { .. }))).then(|| Ok(json!({"seq": seq.0}))))
            }
            "edit" => {
                // a scripted change: rename the sequence (its name lives in its own part)
                let seq = self.seq(word(0)?)?;
                let name =
                    rest.split_once(char::is_whitespace).map(|(_, n)| n.trim()).filter(|n| !n.is_empty()).ok_or("`edit` needs a sequence and a new name")?;
                self.s.execute("clip.rename", json!({"item": seq.0, "name": name})).map_err(|e| e.to_string())?;
                Ok(json!({"seq": seq.0, "name": name}))
            }
            "exec" => {
                let (id, params) = rest.split_once(char::is_whitespace).map_or((rest, "{}"), |(i, p)| (i, p.trim()));
                let params: Value = serde_json::from_str(params).map_err(|e| format!("params JSON: {e}"))?;
                self.s.execute(id, params).map_err(|e| e.to_string())
            }
            "delete" => {
                let seq = self.seq(word(0)?)?;
                self.s.execute("project.delete", json!({"items": [seq.0]})).map_err(|e| e.to_string())
            }
            "import" => {
                if rest.is_empty() {
                    return Err("`import` needs a media file".into());
                }
                let r = self.s.execute("file.import", json!({"paths": [crate::absolute(rest)]})).map_err(|e| e.to_string())?;
                self.sync()?;
                Ok(r)
            }
            "fetch" => {
                // the originals every sequence needs, downloaded as an export would ask for them
                let seqs = self.sequences();
                self.wait("media downloads", |m| {
                    let missing = m.ctrl.export_missing(&m.s, &seqs);
                    if let Some(f) = m.ctrl.media().map(|d| d.failures()).filter(|f| !f.is_empty()) {
                        return Some(Err(format!("download failed: {f:?}")));
                    }
                    (missing.is_empty() && m.ctrl.media().is_none_or(|d| d.progress().is_empty())).then_some(Ok(Value::Null))
                })?;
                Ok(self.status())
            }
            "sync" | "push" => self.sync(),
            "status" => Ok(self.status()),
            "split" => self.split(),
            "freeze" => {
                // the Mac sleeps: no frames, so no heartbeats and no pushes
                let secs: u64 = word(0)?.parse().map_err(|_| "`freeze` needs whole seconds".to_string())?;
                std::thread::sleep(Duration::from_secs(secs.min(600)));
                Ok(json!({"froze": secs}))
            }
            other => Err(format!("unknown team command `{other}`")),
        }
    }
}

/// `team session --dir D`: one command per stdin line, one JSON answer per stdout line
/// (`{"ok":true,"result":…}` or `{"ok":false,"error":…}`); `quit` or end of input stops it.
fn session(a: &Args) -> Result<(), String> {
    let mut m = Member::connect(&dir_of(a)?)?;
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    outln!("{}", json!({"ok": true, "result": {"user": m.ctrl.user()}}));
    loop {
        match rx.recv_timeout(TICK) {
            Ok(line) if line.trim().is_empty() => {}
            Ok(line) if line.trim() == "quit" => break,
            Ok(line) => answer(m.run(&line)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        m.tick();
    }
    // leave the project the way the app does: locks released after pending pushes
    let _ = m.sync();
    m.ctrl.close(&mut m.s);
    Ok(())
}

fn answer(r: Result<Value, String>) {
    let line = match r {
        Ok(v) => json!({"ok": true, "result": v}),
        Err(e) => json!({"ok": false, "error": e}),
    };
    outln!("{line}");
}

/// `team create <name> <project.fcproj>`, `team open <id> [--out p.fcproj]`, `team status <id>`,
/// `team list`, …: a session with that one command (opening `<id>` first when it needs it).
fn one_shot(a: &Args, cmd: &str) -> Result<(), String> {
    let mut m = Member::connect(&dir_of(a)?)?;
    let args: Vec<&str> = a.positionals.iter().skip(2).map(String::as_str).collect();
    let needs_open = !matches!(cmd, "create" | "list" | "open");
    let rest = if needs_open {
        let id = args.first().ok_or_else(|| format!("`team {cmd}` needs a project id"))?;
        m.run(&format!("open {id}"))?;
        args.get(1..).unwrap_or_default().join(" ")
    } else {
        args.join(" ")
    };
    let r = m.run(&format!("{cmd} {rest}"));
    if r.is_ok() && cmd != "list" {
        m.sync()?;
    }
    if let (Ok(_), Some(out)) = (&r, a.opt("--out")) {
        // the merged project as a plain project file (this Mac's media paths)
        m.ctrl.close(&mut m.s);
        m.s.execute("file.saveAs", json!({"path": out})).map_err(|e| format!("saving {out}: {e}"))?;
    } else {
        m.ctrl.close(&mut m.s);
    }
    answer(r);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn token_file_is_private_even_when_it_was_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("filmcraft-cli-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        // left readable by an older version, or made by hand
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, "ifd_new").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ifd_new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
