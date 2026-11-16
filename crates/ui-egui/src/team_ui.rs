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
    /// This member's media going up to the project's media folder: (name, fraction done).
    pub uploads: Vec<(String, f32)>,
    /// This member's media that failed to upload: (name, why).
    pub upload_failures: Vec<(String, String)>,
    /// Media a teammate is still uploading: (name, teammate).
    pub incoming: Vec<(String, String)>,
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
    /// Try failed downloads and uploads again.
    RetryDownloads,
    ClearError,
    /// Leaving waits on: another round of waiting for the edits to reach the Drive.
    LeaveWait,
    /// Leave now; edits not on the Drive stay on this Mac only.
    LeaveAnyway,
    /// Don't close, sign out or quit after all.
    LeaveCancel,
}

/// What leaving the open team project is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeamLeave {
    Close,
    SignOut,
    Quit,
}

/// Close, Sign Out or Quit waiting for edits still going to the Drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TeamLeaving {
    pub what: TeamLeave,
    /// It has waited a while without the edits getting there: ask whether to wait more.
    pub timed_out: bool,
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
    /// Close, Sign Out or Quit waiting for edits still going to the Drive.
    fn leaving(&self) -> Option<TeamLeaving> {
        None
    }
    /// The window was asked to close: true to keep it open while edits still go to the Drive
    /// (quitting then waits, see [`Self::take_quit`]).
    fn hold_quit(&mut self, _s: &mut Session) -> bool {
        false
    }
    /// True once when a held quit may go ahead.
    fn take_quit(&mut self) -> bool {
        false
    }
}

/// Per frame (`logic_background`): poll the team link, and tell it when an export starts or ends
/// by any route (Export button, Quick Export, the queue, the control channel).
pub(crate) fn tick(app: &mut crate::FilmcraftApp, ctx: &egui::Context) {
    let exporting = crate::panels::export_mode::is_export_busy(app);
    let Some(team) = app.hooks.team.as_mut() else { return };
    team.poll(&mut app.session);
    if team.take_quit() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
    if team.exporting() != exporting && app.session.team.is_some() {
        team.set_exporting(exporting);
    } else if team.exporting() && app.session.team.is_none() {
        team.set_exporting(false);
    }
    if team.active() {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

/// The window was asked to close: true (cancel the close) while a team project's edits are still
/// going to the Drive. The app quits by itself once they are there, or when the member chooses
/// Leave Anyway in [`show_leaving`].
pub(crate) fn intercept_quit(app: &mut crate::FilmcraftApp) -> bool {
    app.hooks.team.as_mut().is_some_and(|t| t.hold_quit(&mut app.session))
}

/// The dialog while Close, Sign Out or Quit waits for edits to reach the Drive: Leave Anyway or
/// Cancel while it waits, then Wait or Leave Anyway once it has waited a while. Automation ids:
/// `team.leaving.wait`, `team.leaving.anyway`, `team.leaving.cancel`.
pub(crate) fn show_leaving(app: &mut crate::FilmcraftApp, ctx: &egui::Context) {
    let Some(leaving) = app.hooks.team.as_ref().and_then(|t| t.leaving()) else { return };
    let mut action = None;
    let mut elems: Vec<(&str, egui::Rect, &str)> = Vec::new();
    egui::Window::new(tl!("Saving to InFocus Drive"))
        .id(egui::Id::unique("team-leaving"))
        .collapsible(false)
        .resizable(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ui.set_max_width(420.0);
            let doing = match leaving.what {
                TeamLeave::Close => tl!("closing the project"),
                TeamLeave::SignOut => tl!("signing out"),
                TeamLeave::Quit => tl!("quitting"),
            };
            if leaving.timed_out {
                ui.label(tl!(
                    "Your latest edits haven't reached InFocus Drive yet. If you leave now, they stay on this Mac only and your teammates won't get them."
                ));
            } else {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(tlf!("Sending your edits to InFocus Drive before {doing}…", doing));
                });
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if leaving.timed_out {
                    let r = ui.button(tl!("Wait"));
                    elems.push(("team.leaving.wait", r.rect, "Wait"));
                    if r.clicked() {
                        action = Some(TeamAction::LeaveWait);
                    }
                }
                let r = ui.button(tl!("Leave Anyway"));
                elems.push(("team.leaving.anyway", r.rect, "Leave Anyway"));
                if r.clicked() {
                    action = Some(TeamAction::LeaveAnyway);
                }
                let r = ui.button(tl!("Cancel"));
                elems.push(("team.leaving.cancel", r.rect, "Cancel"));
                if r.clicked() {
                    action = Some(TeamAction::LeaveCancel);
                }
            });
        });
    for (id, r, label) in elems {
        app.auto.add(id, r, label);
    }
    if let (Some(action), Some(team)) = (action, app.hooks.team.as_mut()) {
        team.act(&mut app.session, action);
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(250));
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

    /// A team host whose edits are still on their way until `done`, recording what it is asked.
    #[derive(Default)]
    struct Unsent {
        done: Rc<Cell<bool>>,
        leaving: Rc<Cell<bool>>,
        acts: Rc<std::cell::RefCell<Vec<TeamAction>>>,
    }

    impl TeamUi for Unsent {
        fn poll(&mut self, _s: &mut Session) {}
        fn view(&self, _s: &Session) -> TeamView {
            TeamView::default()
        }
        fn act(&mut self, _s: &mut Session, action: TeamAction) {
            self.acts.borrow_mut().push(action);
        }
        fn export_missing(&mut self, _s: &Session, _sequences: &[ItemId]) -> Vec<ItemId> {
            Vec::new()
        }
        fn set_exporting(&mut self, _exporting: bool) {}
        fn exporting(&self) -> bool {
            false
        }
        fn active(&self) -> bool {
            false
        }
        fn leaving(&self) -> Option<TeamLeaving> {
            self.leaving.get().then_some(TeamLeaving { what: TeamLeave::Quit, timed_out: true })
        }
        fn hold_quit(&mut self, _s: &mut Session) -> bool {
            self.leaving.set(!self.done.get());
            !self.done.get()
        }
        fn take_quit(&mut self) -> bool {
            let go = self.leaving.get() && self.done.get();
            if go {
                self.leaving.set(false);
            }
            go
        }
    }

    #[test]
    fn quit_waits_for_edits_still_going_to_the_drive() {
        let mut app = crate::FilmcraftApp::new(Session::default());
        assert!(!intercept_quit(&mut app), "no team host: quit as before");
        let host = Unsent::default();
        let (done, leaving, acts) = (host.done.clone(), host.leaving.clone(), host.acts.clone());
        app.hooks.team = Some(Box::new(host));
        assert!(intercept_quit(&mut app), "held while edits are unsent");
        // the dialog offers Wait, Leave Anyway and Cancel
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput::default(), |ui| show_leaving(&mut app, ui.ctx())).textures_delta.clear();
        for id in ["team.leaving.wait", "team.leaving.anyway", "team.leaving.cancel"] {
            assert!(app.auto.find(id).is_some(), "{id}");
        }
        assert!(acts.borrow().is_empty());
        done.set(true);
        assert!(leaving.get());
        tick(&mut app, &ctx);
        assert!(!leaving.get(), "the host's quit was taken: the window closes");
        assert!(!intercept_quit(&mut app), "and is no longer held");
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
