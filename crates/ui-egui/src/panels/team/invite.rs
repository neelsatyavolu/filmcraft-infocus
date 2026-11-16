//! The invite picker: search the Drive's members, tick teammates, send. Shown after creating a
//! project ("Invite your teammates"), and by Invite… on a project.

use egui::RichText;

use super::{Out, PanelState, SECTION_GAP, primary_button};
use crate::team_ui::{TeamAction, TeamPerson, TeamView};
use crate::theme::Tokens;

/// Most people one project can have, members and invites together (the Drive's limit).
pub(super) const MAX_TEAM: usize = 12;
/// Height of the people list before it scrolls.
const LIST_HEIGHT: f32 = 180.0;

/// True when `p` matches the search text (name or user name, any case).
fn matches(p: &TeamPerson, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty() || p.name.to_lowercase().contains(&q) || p.username.to_lowercase().contains(&q)
}

pub(super) fn picker(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, id: &str, st: &mut PanelState, out: &mut Out) {
    let row = view.projects.iter().find(|p| p.id == id);
    let name = row.map(|p| p.name.clone()).or_else(|| view.open.as_ref().filter(|o| o.id == id).map(|o| o.name.clone())).unwrap_or_default();
    // people already in the project or invited aren't offered again
    let taken: Vec<&str> = row.map(|p| p.members.iter().chain(&p.invites).map(|m| m.username.as_str()).collect()).unwrap_or_default();
    let room = MAX_TEAM.saturating_sub(row.map_or(1, |p| p.members.len().max(1) + p.invites.len()));
    let from_prompt = view.invite_prompt.as_deref() == Some(id);
    let frame = egui::Frame::new()
        .fill(t.field_bg)
        .stroke(egui::Stroke::new(1.0, t.accent.gamma_multiply(0.6)))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::same(10));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let title = tlf!("Invite teammates to “{name}”", name);
        ui.label(RichText::new(title).strong().color(t.text));
        let hint = if from_prompt {
            tl!("Your project is on InFocus Drive. Pick who you're making it with: they get an invitation in their Team panel.")
        } else {
            tl!("They get an invitation in their Team panel.")
        };
        ui.add(egui::Label::new(RichText::new(hint).small().color(t.text_dim)).wrap());
        ui.add_space(6.0);
        let offered: Vec<&TeamPerson> = view.people.iter().filter(|p| !taken.contains(&p.username.as_str())).collect();
        if offered.is_empty() {
            let text = if view.people.is_empty() {
                tl!("No one else can be invited yet. Your teammates need an InFocus Drive account first.")
            } else {
                tl!("Everyone on InFocus Drive is already in this project or invited.")
            };
            ui.add(egui::Label::new(RichText::new(text).color(t.text_faint)).wrap());
        } else {
            let r = crate::widgets::search_field(ui, &mut st.query, tl!("Search by name"), ui.available_width(), t);
            out.elem("team.invite.search", r.rect, st.query.clone());
            ui.add_space(4.0);
            people(ui, t, &offered, room, st, out);
        }
        if room == 0 {
            ui.label(RichText::new(tlf!("A project can have up to {n} people.", n = MAX_TEAM)).small().color(t.render_yellow));
        }
        ui.add_space(8.0);
        buttons(ui, t, id, from_prompt, st, out);
    });
    ui.add_space(SECTION_GAP);
}

fn people(ui: &mut egui::Ui, t: &Tokens, offered: &[&TeamPerson], room: usize, st: &mut PanelState, out: &mut Out) {
    let shown: Vec<&&TeamPerson> = offered.iter().filter(|p| matches(p, &st.query)).collect();
    if shown.is_empty() {
        ui.label(RichText::new(tl!("No one matches that name.")).color(t.text_faint));
        return;
    }
    egui::ScrollArea::vertical().id_salt("team-invite-people").max_height(LIST_HEIGHT).auto_shrink([false, true]).show(ui, |ui| {
        for p in shown {
            let mut on = st.picked.contains(&p.username);
            let full = !on && st.picked.len() >= room;
            let label = if p.name.is_empty() || p.name == p.username { p.username.clone() } else { format!("{} ({})", p.name, p.username) };
            let r = ui.add_enabled(!full, egui::Checkbox::new(&mut on, RichText::new(&label).color(t.text)));
            out.elem(format!("team.invite.person.{}", p.username), r.rect, label);
            if r.changed() {
                if on {
                    st.picked.insert(p.username.clone());
                } else {
                    st.picked.remove(&p.username);
                }
            }
        }
    });
}

fn buttons(ui: &mut egui::Ui, t: &Tokens, id: &str, from_prompt: bool, st: &mut PanelState, out: &mut Out) {
    ui.horizontal(|ui| {
        let n = st.picked.len();
        let text = match n {
            0 => tl!("Send Invitations").to_string(),
            1 => tl!("Invite 1 Person").to_string(),
            n => tlf!("Invite {n} People", n),
        };
        let r = primary_button(ui, t, &text, n > 0);
        out.elem("team.invite.send", r.rect, text);
        let done = if r.clicked() && n > 0 {
            out.actions.push(TeamAction::Invite(id.to_string(), st.picked.iter().cloned().collect()));
            true
        } else {
            false
        };
        let skip = if from_prompt { tl!("Skip for Now") } else { tl!("Cancel") };
        let r = ui.button(skip);
        out.elem("team.invite.skip", r.rect, skip);
        if done || r.clicked() {
            if from_prompt {
                out.actions.push(TeamAction::DismissInvitePrompt);
            }
            st.invite_for = None;
            st.picked.clear();
            st.query.clear();
        }
    });
    if from_prompt {
        ui.label(RichText::new(tl!("You can invite people later with Invite… on the project.")).small().color(t.text_faint));
    }
}
