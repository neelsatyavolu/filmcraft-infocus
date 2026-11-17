//! InFocus Cut team editing in the desktop app: the UI's [`TeamUi`] over
//! `filmcraft-team-link`'s [`TeamSessionController`], which talks to InFocus Drive on background
//! threads. The UI crate stays free of the network so the web build keeps working.

use chrono::TimeZone;
use filmcraft_drive::{Person, ProjectSummary, Role};
use filmcraft_engine::Session;
use filmcraft_project::ItemId;
use filmcraft_team_link::{ControllerConfig, Leave, Phase, TeamSessionController};
use filmcraft_ui_egui::team_ui::{TeamAction, TeamLeave, TeamLeaving, TeamOpenView, TeamPerson, TeamPhase, TeamProjectRow, TeamRole, TeamUi, TeamView};

/// The team side of the app; `Err` when the controller could not be set up (shown in the panel).
pub struct Team(Result<TeamSessionController, String>);

impl Team {
    pub fn new() -> Self {
        let cfg = ControllerConfig::for_app(env!("CARGO_PKG_VERSION"), filmcraft_format::SCHEMA_VERSION);
        if let Err(e) = &cfg {
            log::warn!("team editing is unavailable: {e}");
        }
        Team(cfg.map(TeamSessionController::new))
    }
}

/// "Nov 30, 2026" for a Unix time, in this Mac's time zone.
fn day(unix: f64) -> Option<String> {
    if !unix.is_finite() {
        return None;
    }
    let secs = unix.floor() as i64;
    chrono::Local.timestamp_opt(secs, 0).single().map(|d| d.format("%b %-d, %Y").to_string())
}

fn item_name(s: &Session, id: ItemId) -> String {
    s.project.item(id).map_or_else(|| format!("item {}", id.0), |i| i.name.clone())
}

fn person(p: &Person) -> TeamPerson {
    TeamPerson {
        username: p.username.clone(),
        name: p.shown().to_string(),
        online: p.online,
        last_seen: p.last_seen.filter(|t| t.is_finite() && *t > 0.0).map(|t| t as u64),
    }
}

/// A project of the list as the panel shows it. Lock holders go by their display name; the
/// sequences they edit are named when the project is open here.
fn row(c: &TeamSessionController, s: &Session, p: &ProjectSummary) -> TeamProjectRow {
    let shown = |user: &str| -> String {
        // the panel says "You're editing …" for the signed-in member's own user name
        if c.user() == Some(user) {
            return user.to_string();
        }
        p.members.iter().find(|m| m.username == user).map_or_else(|| user.to_string(), |m| m.shown().to_string())
    };
    let is_open = c.open_project().is_some_and(|o| o.id == p.id);
    let mut editors: Vec<String> = p.locks.values().map(|l| shown(&l.holder)).collect();
    editors.sort();
    editors.dedup();
    let editing = p.locks.iter().map(|(seq, l)| (shown(&l.holder), is_open.then(|| s.project.item(ItemId(*seq)).map(|i| i.name.clone())).flatten())).collect();
    let role = match p.role {
        Role::Member => TeamRole::Member,
        Role::Invited => TeamRole::Invited,
        Role::Open => TeamRole::Open,
    };
    TeamProjectRow {
        id: p.id.clone(),
        name: p.name.clone(),
        role,
        invited_by: p.invited_by.clone(),
        members: p.members.iter().map(person).collect(),
        invites: p.invites.iter().map(|i| TeamPerson { username: i.username.clone(), name: i.shown().to_string(), ..TeamPerson::default() }).collect(),
        editors,
        editing,
        media_count: p.media_count,
        media_bytes: p.media_bytes,
        moves_on: p.cleanup.as_ref().and_then(|k| k.move_after).and_then(day),
        keep_forever: p.cleanup.as_ref().is_some_and(|k| k.keep),
    }
}

fn view(c: &TeamSessionController, s: &Session) -> TeamView {
    let projects = c.projects().iter().map(|p| row(c, s, p)).collect();
    let open = c.open_project().map(|o| TeamOpenView {
        id: o.id.clone(),
        name: o.name.clone(),
        feed_live: c.feed_live(),
        link_error: c.link_error(),
        downloads: c.media().map(|m| m.progress().into_iter().map(|(id, f)| (item_name(s, id), f)).collect()).unwrap_or_default(),
        proxies: c.media().map(|m| m.proxy_progress(s).into_iter().map(|(id, f)| (item_name(s, id), f)).collect()).unwrap_or_default(),
        failures: c.media().map(|m| m.failures().into_iter().map(|(id, e)| (item_name(s, id), e)).collect()).unwrap_or_default(),
        uploads: c.uploads().map(|u| u.progress()).unwrap_or_default(),
        uploads_done: c.uploads().map(|u| u.just_finished()).unwrap_or_default(),
        uploads_resumed: c.uploads().map(|u| u.resumed()).unwrap_or_default(),
        upload_failures: c.uploads().map(|u| u.failures()).unwrap_or_default(),
        incoming: c.incoming_uploads().into_iter().map(|(id, by)| (item_name(s, id), by)).collect(),
        exporting: c.exporting(),
        drive_only: c.drive_only(),
    });
    let phase = match c.phase() {
        Phase::SignedOut => TeamPhase::SignedOut,
        Phase::Connecting => TeamPhase::Connecting,
        Phase::SignedIn => TeamPhase::SignedIn,
    };
    let error = c.error().map(str::to_string).or_else(|| s.team.as_ref().and_then(|t| t.problem.clone()));
    TeamView {
        phase,
        user: c.user().map(str::to_string),
        user_name: c.user_name().map(str::to_string),
        dashboard_url: Some(c.dashboard_url()),
        busy: c.busy().map(str::to_string),
        error,
        projects,
        open,
        people: c.people().iter().map(person).collect(),
        invite_prompt: c.invite_prompt().map(str::to_string),
    }
}

/// "Add Footage…": imports the files; a refusal or a failed import shows in the panel.
fn add_footage(c: &mut TeamSessionController, s: &mut Session, paths: &[String]) {
    match c.add_footage(s, paths) {
        Ok(n) => s.log.push(filmcraft_engine::panels::Level::Info, "team", format!("Added {n} footage files to the team project")),
        Err(e) => {
            s.log.push(filmcraft_engine::panels::Level::Warning, "team", e.clone());
            c.report(e);
        }
    }
}

impl TeamUi for Team {
    fn poll(&mut self, s: &mut Session) {
        if let Ok(c) = self.0.as_mut() {
            c.poll(s);
        }
    }

    fn view(&self, s: &Session) -> TeamView {
        match &self.0 {
            Ok(c) => view(c, s),
            Err(e) => TeamView { error: Some(e.clone()), ..TeamView::default() },
        }
    }

    fn act(&mut self, s: &mut Session, action: TeamAction) {
        let Ok(c) = self.0.as_mut() else { return };
        match action {
            TeamAction::SignIn => c.sign_in(),
            // both wait for edits still going to the Drive
            TeamAction::SignOut => c.leave(s, Leave::SignOut),
            TeamAction::SwitchAccount => c.switch_account(s),
            TeamAction::OpenDashboard => c.open_dashboard(),
            TeamAction::Refresh => {
                c.refresh_projects();
                c.refresh_media();
            }
            TeamAction::CreateFromCurrent(name) => c.create_from_current(s, &name),
            TeamAction::Open(id) => c.open(&id),
            TeamAction::Close => c.leave(s, Leave::Close),
            TeamAction::Keep(id) => c.keep(&id),
            TeamAction::SetKeepForever(id, keep) => c.set_keep_forever(&id, keep),
            TeamAction::RetryDownloads => c.retry_media(),
            TeamAction::ClearError => c.clear_error(),
            TeamAction::LeaveWait => c.leave_wait(s),
            TeamAction::LeaveAnyway => c.leave_anyway(s),
            TeamAction::LeaveCancel => c.cancel_leave(),
            TeamAction::SearchPeople(q) => c.search_people(&q),
            TeamAction::Invite(id, users) => c.invite(&id, &users),
            TeamAction::CancelInvite(id, user) => c.cancel_invite(&id, &user),
            TeamAction::Join(id) => c.join(&id),
            TeamAction::Decline(id) => c.decline(&id),
            TeamAction::LeaveTeam(id) => c.leave_team(s, &id),
            TeamAction::DismissInvitePrompt => c.dismiss_invite_prompt(),
            TeamAction::AddFootage(paths) => add_footage(c, s, &paths),
            TeamAction::AddDriveFootage => c.add_drive_footage(),
        }
    }

    fn leaving(&self) -> Option<TeamLeaving> {
        let (what, timed_out) = self.0.as_ref().ok()?.leaving()?;
        let what = match what {
            Leave::Close => TeamLeave::Close,
            Leave::SignOut => TeamLeave::SignOut,
            Leave::Quit => TeamLeave::Quit,
        };
        Some(TeamLeaving { what, timed_out })
    }

    fn hold_quit(&mut self, s: &mut Session) -> bool {
        let Ok(c) = self.0.as_mut() else { return false };
        if c.open_project().is_none() {
            return false;
        }
        c.leave(s, Leave::Quit);
        // left at once (nothing unsent): quit now
        !c.take_quit()
    }

    fn take_quit(&mut self) -> bool {
        self.0.as_mut().is_ok_and(TeamSessionController::take_quit)
    }

    /// `team.*` control methods, for test scripts driving the app (debug builds only).
    fn control(&mut self, s: &mut Session, method: &str, params: &serde_json::Value) -> Option<Result<serde_json::Value, String>> {
        if !cfg!(debug_assertions) {
            return None;
        }
        Some(match self.0.as_mut() {
            Ok(c) => crate::team_control::handle(c, s, method, params),
            Err(e) => Err(e.clone()),
        })
    }

    fn export_missing(&mut self, s: &Session, sequences: &[ItemId]) -> Vec<ItemId> {
        self.0.as_mut().map(|c| c.export_missing(s, sequences)).unwrap_or_default()
    }

    fn set_exporting(&mut self, exporting: bool) {
        if let Ok(c) = self.0.as_mut() {
            if exporting {
                c.export_begin();
            } else {
                c.export_end();
            }
        }
    }

    fn exporting(&self) -> bool {
        self.0.as_ref().is_ok_and(TeamSessionController::exporting)
    }

    fn active(&self) -> bool {
        self.0.as_ref().is_ok_and(|c| {
            c.busy().is_some() || c.media().is_some_and(|m| !m.progress().is_empty()) || c.uploads().is_some_and(|u| u.busy()) || c.open_project().is_some()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_day_reads_as_a_date() {
        let d = day(1_796_000_000.0).unwrap();
        assert!(d.ends_with(", 2026"), "{d}");
        assert_eq!(day(f64::NAN), None);
    }
}
