//! The "Add footage" step of a new team project, between naming it and inviting teammates:
//! choose files or drop them anywhere on the window. Each file uploads in the background with
//! its own progress bar (and carries on after a quit and relaunch); editing goes on meanwhile.

use egui::{RichText, Sense, Stroke, vec2};

use super::{Out, PanelState, SECTION_GAP, open::progress_row, primary_button};
use crate::team_ui::TeamView;
use crate::theme::Tokens;

/// Height of the drop area.
const DROP_HEIGHT: f32 = 72.0;

pub(super) fn footage_step(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, st: &mut PanelState, out: &mut Out) {
    let Some(id) = view.invite_prompt.clone() else { return };
    let name = view.open.as_ref().filter(|o| o.id == id).map(|o| o.name.clone()).unwrap_or_default();
    let uploads = view.open.as_ref().filter(|o| o.id == id).map(|o| o.uploads.clone()).unwrap_or_default();
    let frame =
        egui::Frame::new().fill(t.field_bg).stroke(Stroke::new(1.0, t.accent.gamma_multiply(0.6))).corner_radius(4.0).inner_margin(egui::Margin::same(10));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(tlf!("Add footage to “{name}”", name)).strong().color(t.text));
        let hint = tl!("Your teammates download it when they open a sequence that uses it. It uploads in the background while you keep editing.");
        ui.add(egui::Label::new(RichText::new(hint).small().color(t.text_dim)).wrap());
        ui.add_space(6.0);
        drop_area(ui, t, out);
        for (i, (file, frac)) in uploads.iter().enumerate() {
            let text = tlf!("Uploading {name}", name = file);
            progress_row(ui, t, &text, *frac, format!("team.setup.upload.{i}"), out);
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            let r = primary_button(ui, t, tl!("Next: Invite Teammates"), true);
            out.elem("team.setup.next", r.rect, tl!("Next: Invite Teammates"));
            let next = r.clicked();
            let r = ui.button(tl!("Skip"));
            out.elem("team.setup.skip", r.rect, tl!("Skip"));
            if next || r.on_hover_text(tl!("Add footage later with Add Footage… on the open project.")).clicked() {
                st.footage_done = Some(id.clone());
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
    let text = if hovering { tl!("Drop to add to the project") } else { tl!("Drop video, audio or photos here") };
    let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(8.0)).layout(egui::Layout::top_down(egui::Align::Center)));
    inner.add_space(4.0);
    inner.label(RichText::new(text).color(t.text_dim));
    inner.add_space(4.0);
    let r = inner.button(tl!("Choose Files…"));
    out.elem("team.setup.footage", r.rect, tl!("Choose Files…"));
    if r.clicked() {
        out.pick_footage = true;
    }
    out.elem("team.setup.drop", rect, text);
}
