//! InFocus Cut team editing in the desktop app: the UI's [`TeamUi`] over
//! `filmcraft-team-link`'s [`TeamSessionController`], which talks to InFocus Drive on background
//! threads. The UI crate stays free of the network so the web build keeps working.

use chrono::TimeZone;
use filmcraft_engine::Session;
use filmcraft_project::ItemId;
use filmcraft_team_link::{ControllerConfig, Phase, TeamSessionController};
use filmcraft_ui_egui::team_ui::{TeamAction, TeamOpenView, TeamPhase, TeamProjectRow, TeamUi, TeamView};

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

fn view(c: &TeamSessionController, s: &Session) -> TeamView {
    let projects = c
        .projects()
        .iter()
        .map(|p| {
            let mut editors: Vec<String> = p.locks.values().map(|l| l.holder.clone()).collect();
            editors.sort();
            editors.dedup();
            let keep_forever = p.cleanup.as_ref().is_some_and(|k| k.keep);
            let moves_on = p.cleanup.as_ref().and_then(|k| k.move_after).and_then(day);
            TeamProjectRow { id: p.id.clone(), name: p.name.clone(), editors, moves_on, keep_forever }
        })
        .collect();
    let open = c.open_project().map(|o| TeamOpenView {
        id: o.id.clone(),
        name: o.name.clone(),
        feed_live: c.feed_live(),
        link_error: c.link_error(),
        downloads: c.media().map(|m| m.progress().into_iter().map(|(id, f)| (item_name(s, id), f)).collect()).unwrap_or_default(),
        failures: c.media().map(|m| m.failures().into_iter().map(|(id, e)| (item_name(s, id), e)).collect()).unwrap_or_default(),
        exporting: c.exporting(),
    });
    let phase = match c.phase() {
        Phase::SignedOut => TeamPhase::SignedOut,
        Phase::Connecting => TeamPhase::Connecting,
        Phase::SignedIn => TeamPhase::SignedIn,
    };
    let error = c.error().map(str::to_string).or_else(|| s.team.as_ref().and_then(|t| t.problem.clone()));
    TeamView { phase, user: c.user().map(str::to_string), busy: c.busy().map(str::to_string), error, projects, open }
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
            TeamAction::SignOut => c.sign_out(s),
            TeamAction::Refresh => c.refresh_projects(),
            TeamAction::CreateFromCurrent(name) => c.create_from_current(s, &name),
            TeamAction::Open(id) => c.open(&id),
            TeamAction::Close => c.close(s),
            TeamAction::Keep(id) => c.keep(&id),
            TeamAction::SetKeepForever(id, keep) => c.set_keep_forever(&id, keep),
            TeamAction::RetryDownloads => {
                if let Some(m) = c.media_mut() {
                    m.retry();
                }
            }
            TeamAction::ClearError => c.clear_error(),
        }
    }

    fn active(&self) -> bool {
        self.0.as_ref().is_ok_and(|c| c.busy().is_some() || c.media().is_some_and(|m| !m.progress().is_empty()) || c.open_project().is_some())
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
