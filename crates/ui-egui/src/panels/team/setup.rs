//! The "Add footage" step of a new team project, between naming it and inviting teammates:
//! choose files or drop them anywhere on the window. Each file uploads in the background with
//! its own progress bar (and carries on after a quit and relaunch); editing goes on meanwhile.

use egui::{RichText, Sense, Stroke, vec2};

use super::{Out, PanelState, SECTION_GAP, open::progress_row, primary_button};
use crate::team_ui::{TeamAction, TeamView};
use crate::theme::Tokens;

/// Height of the drop area: one line, so the step stays small in a narrow panel.
const DROP_HEIGHT: f32 = 40.0;

pub(super) fn footage_step(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, st: &mut PanelState, out: &mut Out) {
    let Some(id) = view.invite_prompt.clone() else { return };
    let open = view.open.as_ref().filter(|o| o.id == id);
    let name = open.map(|o| o.name.clone()).unwrap_or_default();
    let uploads = open.map(|o| o.uploads.clone()).unwrap_or_default();
    let failed: Vec<String> = open.map(|o| o.upload_failures.iter().map(|(n, _)| n.clone()).collect()).unwrap_or_default();
    for (file, _) in &uploads {
        if !st.setup_files.contains(file) {
            st.setup_files.push(file.clone());
        }
    }
    let uploaded: Vec<String> = st.setup_files.iter().filter(|f| !uploads.iter().any(|(u, _)| u == *f) && !failed.contains(f)).cloned().collect();
    let row = view.projects.iter().find(|p| p.id == id);
    let has_footage = !uploaded.is_empty() || row.is_some_and(|r| r.media_count > 0);
    // teammates are already in (invited from the dashboard, or the project had people): both
    // steps are done once footage is up
    let has_team = row.is_some_and(|r| r.members.len() > 1 || !r.invites.is_empty());
    if has_team && has_footage && uploads.is_empty() {
        st.footage_done = Some(id);
        st.invite_for = None;
        st.setup_files.clear();
        out.actions.push(TeamAction::DismissInvitePrompt);
        return;
    }
    let frame =
        egui::Frame::new().fill(t.field_bg).stroke(Stroke::new(1.0, t.accent.gamma_multiply(0.6))).corner_radius(4.0).inner_margin(egui::Margin::same(8));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let r = ui.add(egui::Label::new(RichText::new(tlf!("Add footage to “{name}”", name)).strong().color(t.text)).wrap());
        out.elem("team.setup.title", r.rect, name.clone());
        ui.add(egui::Label::new(RichText::new(tl!("Teammates download it when they open a sequence that uses it.")).small().color(t.text_dim)).wrap());
        ui.add_space(4.0);
        drop_area(ui, t, out);
        for (i, (file, frac)) in uploads.iter().enumerate() {
            let text = tlf!("Uploading {name}", name = file);
            progress_row(ui, t, &text, *frac, format!("team.setup.upload.{i}"), out);
        }
        for (i, file) in uploaded.iter().enumerate() {
            let text = tlf!("{name} uploaded", name = file);
            let r = ui.add(egui::Label::new(RichText::new(format!("✓ {text}")).small().color(t.render_green)).truncate());
            out.elem(format!("team.setup.done.{i}"), r.rect, text);
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let r = primary_button(ui, t, tl!("Next: Invite Teammates"), true);
            out.elem("team.setup.next", r.rect, tl!("Next: Invite Teammates"));
            let next = r.clicked();
            // after the first upload this isn't skipping anything
            let later = if has_footage { tl!("Done") } else { tl!("Skip") };
            let r = ui.button(later);
            out.elem("team.setup.skip", r.rect, later);
            let skip = r.on_hover_text(tl!("Add footage later with Add Footage… on the open project.")).clicked();
            if next || skip {
                st.footage_done = Some(id.clone());
                st.setup_files.clear();
            }
            if skip && has_team {
                st.invite_for = None;
                out.actions.push(TeamAction::DismissInvitePrompt);
            }
        });
    });
    ui.add_space(SECTION_GAP);
}

/// A dashed area that says files can be dropped (the window imports drops into the open team
/// project), with Choose Files… in it. It lights up while files hover over the window.
fn drop_area(ui: &mut egui::Ui, t: &Tokens, out: &mut Out) {
    let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), DROP_HEIGHT), Sense::hover());
    let color = if hovering { t.accent } else { t.separator };
    if hovering {
        ui.painter().rect_filled(rect, 4.0, t.accent.gamma_multiply(0.12));
    }
    ui.painter().add(egui::Shape::dashed_line(
        &[rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()],
        Stroke::new(1.0, color),
        6.0,
        4.0,
    ));
    let text = if hovering { tl!("Drop to add to the project") } else { tl!("Drop files here, or") };
    let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 4.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    inner.add(egui::Label::new(RichText::new(text).color(t.text_dim)).truncate());
    let r = inner.button(tl!("Choose Files…"));
    out.elem("team.setup.footage", r.rect, tl!("Choose Files…"));
    if r.clicked() {
        out.pick_footage = true;
    }
    out.elem("team.setup.drop", rect, text);
}
