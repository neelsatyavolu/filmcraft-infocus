//! Team panel (InFocus Cut): sign in to InFocus Drive, answer invitations, make the open project a
//! team project and invite teammates to it, list the team projects with their members, who is
//! editing what and their footage, and follow the open project's connection, footage downloads,
//! proxies and uploads. It draws a [`TeamView`] the host's [`TeamUi`](crate::team_ui::TeamUi)
//! gives it and sends back [`TeamAction`]s; without that host hook (the web app) it only says team
//! editing needs the desktop app.
//!
//! - [`projects`]: invitations, the project list and the "New Team Project" form.
//! - [`invite`]: the invite picker (after creating a project, or Invite… on a project).
//! - [`open`]: the open project: connection, footage, downloads, proxies and uploads.
//! - [`setup`]: the "Add footage" step of a new team project, before inviting teammates.
//!
//! Automation ids: `team.signIn`, `team.signOut`, `team.switchAccount`, `team.dashboard`, `team.status`, `team.error`,
//! `team.error.dismiss`, `team.busy`, `team.empty`, `team.new`, `team.create.name`, `team.create`,
//! `team.create.cancel`, `team.refresh`, `team.invitation.<id>`, `team.invitation.<id>.join`,
//! `team.invitation.<id>.decline`, `team.project.<id>`, `team.project.<id>.open`,
//! `team.project.<id>.members`, `team.project.<id>.footage`, `team.project.<id>.invite`,
//! `team.project.<id>.invited.<user>.cancel`, `team.project.<id>.leaveTeam`,
//! `team.project.<id>.cleanup`, `team.project.<id>.keep`, `team.project.<id>.keepForever`,
//! `team.confirm.leave`, `team.confirm.cancel`, `team.invite.search`,
//! `team.invite.person.<user>`, `team.invite.send`, `team.invite.skip`, `team.open`,
//! `team.leave`, `team.footage.add`, `team.setup.footage`, `team.setup.next`, `team.setup.skip`,
//! `team.drive_only`, `team.drive_only.add`, `team.project.<id>.member.<user>`, `team.download.<n>`, `team.proxy.<n>`, `team.upload.<n>`,
//! `team.incoming.<n>`, `team.failure.<n>`, `team.upload_failure.<n>`, `team.retry`.

mod invite;
mod open;
mod projects;
mod setup;

use std::collections::BTreeSet;

use egui::{Color32, Rect, RichText, vec2};

use crate::FilmcraftApp;
use crate::team_ui::{TeamAction, TeamPhase, TeamView};
use crate::theme::Tokens;

/// Automation elements collected while drawing: (id, rect, label).
type Elems = Vec<(String, Rect, String)>;

/// Space between the panel's sections.
const SECTION_GAP: f32 = 12.0;
/// Below this width a row's buttons go under its text instead of beside it.
const NARROW: f32 = 220.0;
/// Below this width the account chip's buttons go under the name.
const ACCOUNT_ONE_ROW: f32 = 340.0;

/// Text on the left and buttons on the right (added right to left). The buttons are placed first,
/// so long text wraps in the space left of them instead of running under them; in a very narrow
/// panel the buttons go on a line of their own under the text, still right-aligned.
fn split_row(ui: &mut egui::Ui, left: impl FnOnce(&mut egui::Ui), right: impl FnOnce(&mut egui::Ui)) {
    if ui.available_width() < NARROW {
        ui.horizontal_wrapped(left);
        // top-aligned: centred, the row would take the scroll area's whole height
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min).with_main_wrap(true), right);
        return;
    }
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            right(ui);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true), left);
        });
    });
}

/// What the panel remembers between frames (not part of the project).
#[derive(Clone, Debug, Default)]
struct PanelState {
    /// The "New Team Project" form is showing.
    creating: bool,
    /// Its name field.
    name: String,
    /// The invite picker is showing for this project.
    invite_for: Option<String>,
    /// The picker's search text and ticked user names.
    query: String,
    picked: BTreeSet<String>,
    /// The ticked people, so they stay listed while the search text changes.
    picked_people: Vec<crate::team_ui::TeamPerson>,
    /// Asking "Leave this project?" for this project.
    confirm_leave: Option<String>,
    /// The new project whose "Add footage" step is done (the invite step comes next).
    footage_done: Option<String>,
    /// Files seen uploading during the "Add footage" step (listed as uploaded once done).
    setup_files: Vec<String>,
    /// The "Add footage" step is showing (it lists the uploads itself).
    setup_open: bool,
    /// Something has downloaded or uploaded in this panel: the "how footage gets here" line
    /// has done its job and stays hidden.
    seen_transfer: bool,
}

/// What the panel asks of the app after drawing.
#[derive(Default)]
struct Out {
    elems: Elems,
    actions: Vec<TeamAction>,
    /// Show the file picker for "Add Footage…".
    pick_footage: bool,
}

impl Out {
    fn elem(&mut self, id: impl Into<String>, r: Rect, label: impl Into<String>) {
        self.elems.push((id.into(), r, label.into()));
    }
}

fn state_id() -> egui::Id {
    egui::Id::unique("team-panel-state")
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    let Some(view) = app.hooks.team.as_ref().map(|team| team.view(&app.session)) else {
        crate::dock::placeholder(ui, rect, &t, tl!("Team editing is available in the desktop app."));
        return;
    };
    let in_team = app.session.team.is_some();
    let session_name = app.session.path.as_deref().and_then(|p| std::path::Path::new(p).file_stem()).map(|n| n.to_string_lossy().into_owned());
    let mut st: PanelState = ui.data_mut(|d| d.get_temp(state_id())).unwrap_or_default();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(10.0, 8.0))));
    let ui = &mut child;
    let mut out = Out::default();
    egui::ScrollArea::vertical().id_salt("team-panel-scroll").auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        if view.phase == TeamPhase::SignedOut {
            return signed_out(ui, &t, &view, &mut out);
        }
        account_row(ui, &t, &view, &mut out);
        if let Some(e) = &view.error {
            error_row(ui, &t, e, &mut out);
        }
        if let Some(busy) = &view.busy {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                let text = crate::i18n::t(busy);
                let r = ui.label(RichText::new(text).color(t.text_dim));
                out.elem("team.busy", r.rect, text);
            });
        }
        if view.phase != TeamPhase::SignedIn {
            return;
        }
        if let Some(id) = view.invite_prompt.clone()
            && st.invite_for.is_none()
        {
            st.invite_for = Some(id);
            st.picked.clear();
            st.query.clear();
        }
        ui.add_space(4.0);
        // the open project (who is editing, what is moving) comes first: it is what a member
        // checks most, and a narrow panel shouldn't push it below the fold
        let footage_step = view.invite_prompt.as_deref().is_some_and(|id| st.footage_done.as_deref() != Some(id));
        st.setup_open = footage_step;
        if let Some(open) = &view.open {
            let row = view.projects.iter().find(|p| p.id == open.id);
            open::section(ui, &t, open, row, &mut st, &mut out);
        }
        if footage_step {
            setup::footage_step(ui, &t, &view, &mut st, &mut out);
        } else if let Some(id) = st.invite_for.clone() {
            invite::picker(ui, &t, &view, &id, &mut st, &mut out);
        }
        projects::invitations(ui, &t, &view, &mut out);
        projects::list(ui, &t, &view, in_team, session_name.as_deref(), &mut st, &mut out);
    });
    ui.data_mut(|d| d.insert_temp(state_id(), st));
    for (id, r, label) in out.elems {
        app.auto.add(&id, r, &label);
    }
    for action in out.actions {
        if let Some(team) = app.hooks.team.as_mut() {
            team.act(&mut app.session, action);
        }
    }
    if out.pick_footage {
        pick_footage(app);
    }
}

/// "Add Footage…": the system file picker, then the chosen files go into the open team project.
fn pick_footage(app: &mut FilmcraftApp) {
    let exts: Vec<&str> =
        filmcraft_media::VIDEO_EXTENSIONS.iter().chain(filmcraft_media::AUDIO_EXTENSIONS).chain(filmcraft_media::STILL_EXTENSIONS).copied().collect();
    app.pick_ui(crate::native_dialogs::FileDialog::open_files(&exts), |app, paths| {
        if paths.is_empty() {
            return;
        }
        if let Some(team) = app.hooks.team.as_mut() {
            team.act(&mut app.session, TeamAction::AddFootage(paths));
        }
    });
}

/// "Maya Lee" with "maya" under it, or the user name alone.
fn who(view: &TeamView) -> Option<(String, Option<String>)> {
    let user = view.user.clone()?;
    match view.user_name.as_deref().map(str::trim).filter(|n| !n.is_empty() && *n != user) {
        Some(name) => Some((name.to_string(), Some(user))),
        None => Some((user, None)),
    }
}

/// The account chip: who is signed in, Switch Account and Sign Out, and the dashboard link.
fn account_row(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, out: &mut Out) {
    if view.phase != TeamPhase::SignedIn {
        let text = tl!("Connecting to InFocus Drive…");
        let r = ui.label(RichText::new(text).color(t.text_dim));
        out.elem("team.status", r.rect, text);
        if view.busy.as_deref() == Some("Signing in") {
            let hint = tl!("Finish signing in in your browser. This panel updates by itself.");
            ui.add(egui::Label::new(RichText::new(hint).small().color(t.text_faint)).wrap());
        }
        ui.add_space(2.0);
        return;
    }
    let Some((shown, username)) = who(view) else { return };
    let status = match &username {
        Some(u) => tlf!("Signed in as {name} ({user})", name = shown, user = u),
        None => tlf!("Signed in as {user}", user = shown),
    };
    let frame = egui::Frame::new().fill(t.field_bg).corner_radius(4.0).inner_margin(egui::Margin::symmetric(8, 6));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let mut account = |ui: &mut egui::Ui| {
            let r = ui.small_button(tl!("Sign Out"));
            out.elem("team.signOut", r.rect, tl!("Sign Out"));
            if r.on_hover_text(tl!("Sign out of InFocus Drive on this Mac. Your edits are sent first.")).clicked() {
                out.actions.push(TeamAction::SignOut);
            }
            let r = ui.small_button(tl!("Switch Account"));
            out.elem("team.switchAccount", r.rect, tl!("Switch Account"));
            if r.on_hover_text(tl!("Sign out, then sign in with another InFocus Drive account.")).clicked() {
                out.actions.push(TeamAction::SwitchAccount);
            }
        };
        // a narrow panel puts the buttons on their own row, so the name keeps its width
        let narrow = ui.available_width() < ACCOUNT_ONE_ROW;
        let mut chip = Rect::NOTHING;
        ui.horizontal(|ui| {
            let initial: String = shown.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
            let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 11.0, t.accent);
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, initial, Tokens::ui(12.0), Color32::WHITE);
            let names = |ui: &mut egui::Ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.add(egui::Label::new(RichText::new(&shown).strong().color(t.text)).truncate()).on_hover_text(&shown);
                    if let Some(u) = &username {
                        ui.add(egui::Label::new(RichText::new(u).small().color(t.text_dim)).truncate()).on_hover_text(u);
                    }
                })
                .response
                .rect
            };
            let text = if narrow {
                names(ui)
            } else {
                let mut rect = Rect::NOTHING;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    account(ui);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| rect = names(ui));
                });
                rect
            };
            chip = r.union(text);
        });
        if narrow {
            // same order as the wide row (Switch Account, then Sign Out on the right)
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min).with_main_wrap(true), account);
        }
        out.elem("team.status", chip, status);
        if view.dashboard_url.is_some() {
            let r = ui.add(egui::Label::new(RichText::new(tl!("Open the team dashboard ↗")).small().color(t.accent)).sense(egui::Sense::click()));
            out.elem("team.dashboard", r.rect, tl!("Open the team dashboard ↗"));
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tl!("See your team projects, footage and invitations on the web.")).clicked() {
                out.actions.push(TeamAction::OpenDashboard);
            }
        }
    });
    ui.add_space(4.0);
}

/// What to do about an error, in plain words, when the message alone doesn't say.
fn next_step(error: &str) -> Option<&'static str> {
    // the plain-words messages already say what to do
    if error.ends_with("try again.") || error.ends_with("try again in a minute.") || error.ends_with("add you.") {
        return None;
    }
    if error.starts_with("Can't reach InFocus Drive") {
        Some(tl!("Check that this Mac is online, then press Refresh."))
    } else if error.starts_with("Sign in to InFocus Drive again") {
        Some(tl!("Press Sign in with InFocus Drive to continue."))
    } else if error.starts_with("InFocus Drive settings") {
        Some(tl!("Ask whoever runs your InFocus Drive for its address."))
    } else if error.starts_with("You don't have access") || error.starts_with("Not found") {
        Some(tl!("Press Refresh. If it's still missing, ask a teammate to invite you again."))
    } else {
        None
    }
}

fn error_row(ui: &mut egui::Ui, t: &Tokens, error: &str, out: &mut Out) {
    let frame = egui::Frame::new().fill(t.danger.gamma_multiply(0.12)).corner_radius(4.0).inner_margin(egui::Margin::symmetric(8, 6));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 64.0).max(80.0);
            ui.vertical(|ui| {
                ui.set_width(width);
                let r = ui.add(egui::Label::new(RichText::new(crate::i18n::t(error)).color(t.danger)).wrap());
                out.elem("team.error", r.rect, error);
                if let Some(step) = next_step(error) {
                    ui.add(egui::Label::new(RichText::new(step).small().color(t.text_dim)).wrap());
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let r = ui.small_button(tl!("Dismiss"));
                out.elem("team.error.dismiss", r.rect, tl!("Dismiss"));
                if r.clicked() {
                    out.actions.push(TeamAction::ClearError);
                }
            });
        });
    });
    ui.add_space(4.0);
}

/// Signed out: one short block in the middle of the panel, as other panels show their empty
/// state, so the panel isn't a line at the top over empty space.
fn signed_out(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, out: &mut Out) {
    // centred on the block's height last frame; a narrow panel scrolls instead of cutting it off
    let height_id = egui::Id::unique("team-signed-out-height");
    let block = ui.data(|d| d.get_temp::<f32>(height_id)).unwrap_or(150.0);
    ui.add_space(((ui.available_height() - block) / 2.0).max(0.0));
    let r = ui.vertical_centered(|ui| {
        // never wider than the panel: a fixed 360 overflowed (and clipped) narrow panels
        ui.set_max_width(ui.available_width().min(360.0));
        let status = tl!("Not signed in");
        let r = ui.add(egui::Label::new(RichText::new(tl!("Edit together on InFocus Drive")).strong().size(14.0).color(t.text)).wrap());
        out.elem("team.status", r.rect, status);
        let intro = tl!("InFocus Drive is the school's shared drive for footage. Sign in to edit with your team.");
        let r = ui.add(egui::Label::new(RichText::new(intro).color(t.text_dim)).wrap());
        out.elem("team.intro", r.rect, intro);
        if let Some(e) = &view.error {
            ui.add_space(4.0);
            let r = ui.add(egui::Label::new(RichText::new(crate::i18n::t(e)).color(t.danger)).wrap());
            out.elem("team.error", r.rect, e);
            if let Some(step) = next_step(e) {
                ui.add(egui::Label::new(RichText::new(step).small().color(t.text_dim)).wrap());
            }
            let r = ui.small_button(tl!("Dismiss"));
            out.elem("team.error.dismiss", r.rect, tl!("Dismiss"));
            if r.clicked() {
                out.actions.push(TeamAction::ClearError);
            }
        }
        ui.add_space(10.0);
        let r = primary_button(ui, t, tl!("Sign in with InFocus Drive"), true);
        out.elem("team.signIn", r.rect, tl!("Sign in with InFocus Drive"));
        if r.clicked() {
            out.actions.push(TeamAction::SignIn);
        }
        ui.add_space(2.0);
        ui.label(RichText::new(tl!("Your browser opens to finish signing in.")).small().color(t.text_faint));
    });
    ui.data_mut(|d| d.insert_temp(height_id, r.response.rect.height()));
}

/// The panel's main button: accent fill, as other dialogs' default buttons.
fn primary_button(ui: &mut egui::Ui, t: &Tokens, text: &str, enabled: bool) -> egui::Response {
    let button = egui::Button::new(RichText::new(text).color(Color32::WHITE)).fill(t.accent).min_size(vec2(0.0, 24.0)).corner_radius(4.0);
    ui.add_enabled(enabled, button)
}

/// A small grey section title with the rule under it.
fn heading(ui: &mut egui::Ui, t: &Tokens, title: &str) {
    ui.label(RichText::new(title).strong().color(t.text));
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), egui::Stroke::new(1.0, t.separator));
    ui.add_space(4.0);
}

/// "1.2 GB" / "340 MB" / "12 KB".
fn size_text(bytes: u64) -> String {
    const KB: f64 = 1000.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.0} MB", b / (KB * KB))
    } else if bytes == 0 {
        "0 KB".into()
    } else {
        format!("{:.0} KB", (b / KB).max(1.0))
    }
}

/// "3 files · 1.2 GB" (no files: "No footage yet").
fn footage_text(count: u64, bytes: u64) -> String {
    match count {
        0 => tl!("No footage yet").into(),
        1 => tlf!("1 file · {size}", size = size_text(bytes)),
        n => tlf!("{n} files · {size}", n, size = size_text(bytes)),
    }
}

#[cfg(test)]
mod tests;
