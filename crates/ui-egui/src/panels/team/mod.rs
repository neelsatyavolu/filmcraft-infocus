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
//!
//! Automation ids: `team.signIn`, `team.signOut`, `team.status`, `team.error`,
//! `team.error.dismiss`, `team.busy`, `team.empty`, `team.new`, `team.create.name`, `team.create`,
//! `team.create.cancel`, `team.refresh`, `team.invitation.<id>`, `team.invitation.<id>.join`,
//! `team.invitation.<id>.decline`, `team.project.<id>`, `team.project.<id>.open`,
//! `team.project.<id>.members`, `team.project.<id>.footage`, `team.project.<id>.invite`,
//! `team.project.<id>.invited.<user>.cancel`, `team.project.<id>.leaveTeam`,
//! `team.project.<id>.cleanup`, `team.project.<id>.keep`, `team.project.<id>.keepForever`,
//! `team.confirm.leave`, `team.confirm.cancel`, `team.invite.search`,
//! `team.invite.person.<user>`, `team.invite.send`, `team.invite.skip`, `team.open`,
//! `team.leave`, `team.footage.add`, `team.download.<n>`, `team.proxy.<n>`, `team.upload.<n>`,
//! `team.incoming.<n>`, `team.failure.<n>`, `team.upload_failure.<n>`, `team.retry`.

mod invite;
mod open;
mod projects;

use std::collections::BTreeSet;

use egui::{Color32, Rect, RichText, vec2};

use crate::FilmcraftApp;
use crate::team_ui::{TeamAction, TeamPhase, TeamView};
use crate::theme::Tokens;

/// Automation elements collected while drawing: (id, rect, label).
type Elems = Vec<(String, Rect, String)>;

/// Space between the panel's sections.
const SECTION_GAP: f32 = 12.0;

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
    /// Asking "Leave this project?" for this project.
    confirm_leave: Option<String>,
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
        if let Some(id) = st.invite_for.clone() {
            invite::picker(ui, &t, &view, &id, &mut st, &mut out);
        }
        projects::invitations(ui, &t, &view, &mut out);
        if let Some(open) = &view.open {
            let row = view.projects.iter().find(|p| p.id == open.id);
            open::section(ui, &t, open, row, &mut st, &mut out);
        }
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

fn account_row(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, out: &mut Out) {
    ui.horizontal(|ui| {
        let who = match (view.phase, &view.user) {
            (TeamPhase::SignedIn, Some(user)) => tlf!("Signed in as {user}", user),
            (TeamPhase::Connecting, _) => tl!("Connecting to InFocus Drive…").to_string(),
            _ => tl!("Not signed in").to_string(),
        };
        let r = ui.label(RichText::new(&who).color(t.text_dim));
        out.elem("team.status", r.rect, who);
        if view.phase == TeamPhase::SignedIn {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = ui.small_button(tl!("Sign Out"));
                out.elem("team.signOut", r.rect, tl!("Sign Out"));
                if r.clicked() {
                    out.actions.push(TeamAction::SignOut);
                }
            });
        }
    });
    ui.add_space(2.0);
}

/// What to do about an error, in plain words, when the message alone doesn't say.
fn next_step(error: &str) -> Option<&'static str> {
    if error.starts_with("Can't reach InFocus Drive") {
        Some(tl!("Check that this Mac is online, then press Refresh."))
    } else if error.starts_with("Sign in to InFocus Drive again") {
        Some(tl!("Press Sign In to continue."))
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
                let r = ui.add(egui::Label::new(RichText::new(error).color(t.danger)).wrap());
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
        ui.set_max_width(360.0);
        let status = tl!("Not signed in");
        let r = ui.label(RichText::new(tl!("Edit together on InFocus Drive")).strong().size(14.0).color(t.text));
        out.elem("team.status", r.rect, status);
        ui.add(
            egui::Label::new(
                RichText::new(tl!("Sign in with your InFocus Drive account to make a team project, invite classmates and share footage.")).color(t.text_dim),
            )
            .wrap(),
        );
        if let Some(e) = &view.error {
            ui.add_space(4.0);
            let r = ui.add(egui::Label::new(RichText::new(e).color(t.danger)).wrap());
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
        let r = primary_button(ui, t, tl!("Sign In…"), true);
        out.elem("team.signIn", r.rect, tl!("Sign In…"));
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
