//! InFocus Cut team editing, as the UI sees it. The desktop app plugs a [`TeamUi`] into
//! [`HostHooks::team`](crate::HostHooks::team) (`apps/filmcraft/src/team.rs`, over
//! `filmcraft-team-link`, which brings the network and threads this crate must not depend on).
//! Without it (the web app, headless tests) there is no Team panel and nothing here runs.

use filmcraft_engine::Session;
use filmcraft_project::ItemId;

/// Where sign-in stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TeamPhase {
    #[default]
    SignedOut,
    /// Signing in, or connecting with the stored token.
    Connecting,
    SignedIn,
}

/// One team project in the list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TeamProjectRow {
    pub id: String,
    pub name: String,
    /// Who holds a sequence lock right now (each name once).
    pub editors: Vec<String>,
    /// The day the Drive's cleanup moves it to the recycle bin, when a notice is out.
    pub moves_on: Option<String>,
    /// Marked "Keep forever" (only the creator or a Drive admin may change it).
    pub keep_forever: bool,
}

/// The open team project's connection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TeamOpenView {
    pub id: String,
    pub name: String,
    /// Live updates are connected (otherwise they are polled every few seconds).
    pub feed_live: bool,
    /// The last connection problem.
    pub link_error: Option<String>,
    /// Media downloading: (name, fraction done).
    pub downloads: Vec<(String, f32)>,
    /// Media that failed to download: (name, why).
    pub failures: Vec<(String, String)>,
    /// An export is running (downloads are paused).
    pub exporting: bool,
}

/// Everything the Team panel draws, read once per frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TeamView {
    pub phase: TeamPhase,
    pub user: Option<String>,
    /// What is running in the background.
    pub busy: Option<String>,
    pub error: Option<String>,
    pub projects: Vec<TeamProjectRow>,
    pub open: Option<TeamOpenView>,
}

/// What the Team panel asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum TeamAction {
    SignIn,
    SignOut,
    Refresh,
    /// Make the session's project a team project with this name.
    CreateFromCurrent(String),
    Open(String),
    /// Leave the open team project (the session keeps the project as it is).
    Close,
    /// Restart the project's 30-day cleanup clock.
    Keep(String),
    SetKeepForever(String, bool),
    RetryDownloads,
    ClearError,
}

/// The team side of the desktop app. Object-safe so the host can box it.
pub trait TeamUi {
    /// Once per frame: finishes background work and keeps media downloads going.
    fn poll(&mut self, s: &mut Session);
    fn view(&self, s: &Session) -> TeamView;
    fn act(&mut self, s: &mut Session, action: TeamAction);
    /// Media an export of `sequences` still needs on this Mac (they are fetched from now on).
    fn export_missing(&mut self, s: &Session, sequences: &[ItemId]) -> Vec<ItemId>;
    /// An export started (downloads pause) or ended.
    fn set_exporting(&mut self, exporting: bool);
    fn exporting(&self) -> bool;
    /// Something is in flight (sign-in, downloads): the frame loop keeps polling.
    fn active(&self) -> bool;
}

/// Per frame (`logic_background`): poll the team link, and tell it when an export starts or ends
/// by any route (Export button, Quick Export, the queue, the control channel).
pub(crate) fn tick(app: &mut crate::FilmcraftApp, ctx: &egui::Context) {
    let exporting = crate::panels::export_mode::is_export_busy(app);
    let Some(team) = app.hooks.team.as_mut() else { return };
    team.poll(&mut app.session);
    if team.exporting() != exporting && app.session.team.is_some() {
        team.set_exporting(exporting);
    } else if team.exporting() && app.session.team.is_none() {
        team.set_exporting(false);
    }
    if team.active() {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

/// Before an export of `sequences` in a team project: Err with what is still downloading when
/// media is missing on this Mac (it downloads from now on), so the export waits for it.
pub fn export_guard(app: &mut crate::FilmcraftApp, sequences: &[ItemId]) -> Result<(), String> {
    if app.session.team.is_none() {
        return Ok(());
    }
    let Some(team) = app.hooks.team.as_mut() else { return Ok(()) };
    let missing = team.export_missing(&app.session, sequences);
    if missing.is_empty() {
        return Ok(());
    }
    Err(missing_message(&app.session, &missing))
}

/// "Export waits for media still downloading: A001.mov, A002.mov and 3 more."
fn missing_message(s: &Session, missing: &[ItemId]) -> String {
    const SHOWN: usize = 3;
    let names: Vec<String> = missing.iter().take(SHOWN).map(|id| s.project.item(*id).map_or_else(|| format!("item {}", id.0), |i| i.name.clone())).collect();
    let list = names.join(", ");
    let more = missing.len().saturating_sub(SHOWN);
    if more == 0 {
        tlf!("Can't export yet. Still downloading: {list}", list)
    } else {
        tlf!("Can't export yet. Still downloading: {list} and {count} more", list, count = more)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    /// A team host whose export always lacks item 900, recording when exports start and end.
    struct Missing {
        exporting: Rc<Cell<bool>>,
    }

    impl TeamUi for Missing {
        fn poll(&mut self, _s: &mut Session) {}
        fn view(&self, _s: &Session) -> TeamView {
            TeamView::default()
        }
        fn act(&mut self, _s: &mut Session, _action: TeamAction) {}
        fn export_missing(&mut self, _s: &Session, _sequences: &[ItemId]) -> Vec<ItemId> {
            vec![ItemId(900)]
        }
        fn set_exporting(&mut self, exporting: bool) {
            self.exporting.set(exporting);
        }
        fn exporting(&self) -> bool {
            self.exporting.get()
        }
        fn active(&self) -> bool {
            false
        }
    }

    #[test]
    fn export_guard_blocks_only_team_projects_with_missing_media() {
        let mut app = crate::FilmcraftApp::new(Session::default());
        assert_eq!(export_guard(&mut app, &[]), Ok(()), "no host hook");
        app.hooks.team = Some(Box::new(Missing { exporting: Rc::default() }));
        assert_eq!(export_guard(&mut app, &[]), Ok(()), "not a team project");
    }

    /// A team link that sends nothing anywhere.
    struct NoLink;

    impl filmcraft_engine::team::TeamLink for NoLink {
        fn take_lock(&mut self, _seq: ItemId) {}
        fn heartbeat(&mut self, _seq: ItemId) {}
        fn release(&mut self, _seq: ItemId) {}
        fn put_sequence(&mut self, _seq: ItemId, _base_rev: u64, _json: serde_json::Value) {}
        fn delete_sequence(&mut self, _seq: ItemId, _base_rev: u64) {}
        fn post_shared(&mut self, _push: filmcraft_engine::team::SharedPush) {}
        fn refresh(&mut self) {}
        fn poll(&mut self) -> Vec<filmcraft_engine::team::LinkEvent> {
            Vec::new()
        }
    }

    #[test]
    fn export_waits_for_missing_media_in_a_team_project() {
        let exporting = Rc::new(Cell::new(false));
        let mut app = crate::FilmcraftApp::new(Session::default());
        app.hooks.team = Some(Box::new(Missing { exporting: exporting.clone() }));
        let team = filmcraft_engine::team::TeamState::new(Box::new(NoLink), "maya".into(), 1, Default::default(), 0, Default::default());
        app.session.attach_team(team).unwrap();
        let err = export_guard(&mut app, &[]).unwrap_err();
        assert_eq!(err, "Can't export yet. Still downloading: item 900");
        // an export starting by any route pauses the downloads, and its end resumes them
        app.session.export_queue.running = true;
        tick(&mut app, &egui::Context::default());
        assert!(!exporting.get(), "nothing queued yet: not exporting");
        app.session.jobs.push(filmcraft_engine::Job { id: 1, label: "Export Main".into(), progress: Default::default(), result: Default::default() });
        tick(&mut app, &egui::Context::default());
        assert!(exporting.get(), "an export job pauses downloads");
        app.session.jobs[0].progress.finished.store(true, std::sync::atomic::Ordering::Relaxed);
        tick(&mut app, &egui::Context::default());
        assert!(!exporting.get(), "its end resumes them");
    }

    #[test]
    fn tick_leaves_export_mode_outside_a_team_project() {
        let exporting = Rc::new(Cell::new(true));
        let mut app = crate::FilmcraftApp::new(Session::default());
        app.hooks.team = Some(Box::new(Missing { exporting: exporting.clone() }));
        tick(&mut app, &egui::Context::default());
        assert!(!exporting.get(), "downloads resume once no team project is open");
    }

    #[test]
    fn missing_message_lists_three_then_counts() {
        let s = Session::default();
        let ids: Vec<ItemId> = (900..905).map(ItemId).collect();
        let m = missing_message(&s, &ids);
        assert_eq!(m, "Can't export yet. Still downloading: item 900, item 901, item 902 and 2 more");
        assert_eq!(missing_message(&s, &ids[..1]), "Can't export yet. Still downloading: item 900");
    }
}
