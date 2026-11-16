//! InFocus Cut team editing, as the UI sees it. The desktop app plugs a [`TeamUi`] into
//! [`HostHooks::team`](crate::HostHooks::team) (`apps/filmcraft/src/team.rs`, over
//! `filmcraft-team-link`, which brings the network and threads this crate must not depend on).
//! Without it (the web app, headless tests) there is no Team panel and nothing here runs.

use filmcraft_engine::Session;

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
    /// Something is in flight (sign-in, downloads): the frame loop keeps polling.
    fn active(&self) -> bool;
}

/// Per frame (`logic_background`): poll the team link.
pub(crate) fn tick(app: &mut crate::FilmcraftApp, ctx: &egui::Context) {
    let Some(team) = app.hooks.team.as_mut() else { return };
    team.poll(&mut app.session);
    if team.active() {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}
