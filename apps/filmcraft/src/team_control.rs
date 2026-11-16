//! `team.*` control-channel methods, in debug builds only, so a test script can drive the Team
//! panel's work without clicking (see `docs/control-protocol.md`). Each method starts the same
//! work the panel's buttons do and answers at once; `team.status` shows where it got to.
//!
//! - `team.status`: sign-in, projects (role, members, invites, footage, locks), people, the open
//!   project's downloads, proxies and uploads, and the session's media items with their files.
//! - `team.create {name}`: make the open project a team project.
//! - `team.invite {project, users: [user names]}` / `team.join {project}` / `team.open {project}`
//! - `team.open_sequence {sequence: id | name}`: open a sequence (its footage downloads).
//! - `team.add_footage {paths: [file paths]}`: import files into the open team project.
//! - `team.take_lock {sequence: id | name}`: ask for the lock to edit a sequence.
//! - `team.refresh`: fetch the project list again.

use filmcraft_engine::Session;
use filmcraft_project::{ItemId, MediaRef};
use filmcraft_team_link::{Phase, TeamSessionController};
use serde_json::{Value, json};

/// Most items `team.status` lists.
const MAX_ITEMS: usize = 500;

pub fn handle(c: &mut TeamSessionController, s: &mut Session, method: &str, p: &Value) -> Result<Value, String> {
    let text = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("{method} needs `{k}`"));
    let list = |k: &str| -> Result<Vec<String>, String> {
        let v = p.get(k).and_then(Value::as_array).ok_or_else(|| format!("{method} needs `{k}` (a list)"))?;
        v.iter().take(MAX_ITEMS).map(|x| x.as_str().map(str::to_string).ok_or_else(|| format!("`{k}` must list strings"))).collect()
    };
    let started = || Ok(json!({ "started": true }));
    match method {
        "team.status" => Ok(status(c, s)),
        "team.refresh" => {
            c.refresh_projects();
            started()
        }
        "team.create" => {
            c.create_from_current(s, &text("name")?);
            started()
        }
        "team.invite" => {
            c.invite(&text("project")?, &list("users")?);
            started()
        }
        "team.join" => {
            c.join(&text("project")?);
            started()
        }
        "team.open" => {
            c.open(&text("project")?);
            started()
        }
        "team.open_sequence" => {
            let seq = sequence(s, p)?;
            s.execute("sequence.open", json!({ "item": seq.0 })).map_err(|e| e.to_string())?;
            Ok(json!({ "sequence": seq.0 }))
        }
        "team.add_footage" => Ok(json!({ "added": c.add_footage(s, &list("paths")?)? })),
        "team.take_lock" => {
            let seq = sequence(s, p)?;
            s.team_take_lock(seq).map_err(|e| e.to_string())?;
            Ok(json!({ "sequence": seq.0 }))
        }
        m => Err(format!("unknown method `{m}`")),
    }
}

/// `sequence`: an item id, or a sequence's name.
fn sequence(s: &Session, p: &Value) -> Result<ItemId, String> {
    match p.get("sequence") {
        Some(Value::Number(n)) => n.as_u64().map(ItemId).filter(|id| s.project.item(*id).is_some()).ok_or_else(|| format!("no item {n}")),
        Some(Value::String(name)) => s.project.sequences().find(|q| &q.name == name).map(|q| q.id).ok_or_else(|| format!("no sequence named {name:?}")),
        _ => Err("need `sequence` (an id or a name)".into()),
    }
}

fn status(c: &TeamSessionController, s: &Session) -> Value {
    let phase = match c.phase() {
        Phase::SignedOut => "signed_out",
        Phase::Connecting => "connecting",
        Phase::SignedIn => "signed_in",
    };
    let projects: Vec<Value> = c
        .projects()
        .iter()
        .map(|p| {
            json!({
                "id": p.id, "name": p.name, "role": format!("{:?}", p.role).to_lowercase(),
                "members": p.members.iter().map(|m| &m.username).collect::<Vec<_>>(),
                "invites": p.invites.iter().map(|i| &i.username).collect::<Vec<_>>(),
                "invited_by": p.invited_by, "media_count": p.media_count, "media_bytes": p.media_bytes,
                "locks": p.locks.iter().map(|(seq, l)| json!({ "sequence": seq, "holder": l.holder })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let name = |id: ItemId| s.project.item(id).map_or_else(String::new, |i| i.name.clone());
    let open = c.open_project().map(|o| {
        json!({
            "id": o.id, "name": o.name, "feed_live": c.feed_live(), "link_error": c.link_error(),
            "downloads": c.media().map(|m| m.progress().into_iter().map(|(id, f)| json!({"item": id.0, "name": name(id), "done": f})).collect::<Vec<_>>()),
            "proxies": c.media().map(|m| m.proxy_progress(s).into_iter().map(|(id, f)| json!({"item": id.0, "name": name(id), "done": f})).collect::<Vec<_>>()),
            "download_failures": c.media().map(|m| m.failures().into_iter().map(|(id, e)| json!({"item": id.0, "name": name(id), "error": e})).collect::<Vec<_>>()),
            "uploads": c.uploads().map(|u| u.progress()),
            "upload_failures": c.uploads().map(|u| u.failures()),
            "uploads_busy": c.uploads().is_some_and(|u| u.busy()),
        })
    });
    let media: Vec<Value> = s
        .project
        .items
        .values()
        .filter_map(|i| i.as_media().map(|m| (i, m)))
        .take(MAX_ITEMS)
        .map(|(i, m)| {
            let path = |r: &MediaRef| match r {
                MediaRef::File { path } => Some(path.clone()),
                _ => None,
            };
            json!({ "id": i.id.0, "name": i.name, "path": path(&m.media), "proxy": m.proxy.as_ref().and_then(path), "offline": m.offline })
        })
        .collect();
    let sequences: Vec<Value> =
        s.project.sequences().map(|q| json!({ "id": q.id.0, "name": q.name, "status": s.team_status(q.id).map(|st| format!("{st:?}")) })).collect();
    json!({
        "phase": phase, "user": c.user(), "busy": c.busy(), "error": c.error(),
        "team_problem": s.team.as_ref().and_then(|t| t.problem.clone()),
        "invite_prompt": c.invite_prompt(),
        "people": c.people().iter().map(|p| json!({"username": p.username, "name": p.shown()})).collect::<Vec<_>>(),
        "projects": projects, "open": open, "media": media, "sequences": sequences,
        "open_sequences": s.state.open_sequences.iter().map(|q| q.0).collect::<Vec<_>>(),
    })
}
