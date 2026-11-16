//! The open team project: its connection, Add Footage…, and what this Mac is downloading, making
//! proxies of and uploading.

use egui::{Align2, Rect, RichText, Sense, pos2, vec2};

use super::{Out, PanelState, SECTION_GAP, footage_text, heading};
use crate::team_ui::{TeamAction, TeamOpenView, TeamProjectRow, TeamRole};
use crate::theme::Tokens;

pub(super) fn section(ui: &mut egui::Ui, t: &Tokens, open: &TeamOpenView, row: Option<&TeamProjectRow>, st: &mut PanelState, out: &mut Out) {
    heading(ui, t, tl!("Open now"));
    ui.horizontal(|ui| {
        let (dot, state) = if open.feed_live { (t.render_green, tl!("Live")) } else { (t.render_yellow, tl!("Reconnecting…")) };
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 4.0, dot);
        let r = ui.label(RichText::new(&open.name).strong().color(t.text));
        ui.label(RichText::new(state).small().color(t.text_dim));
        out.elem("team.open", r.rect, format!("{} ({state})", open.name));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let r = ui.button(tl!("Close"));
            out.elem("team.leave", r.rect, tl!("Close"));
            if r.on_hover_text(tl!("Stop working on this project here. Your edits are sent to the Drive first and your locks are released.")).clicked() {
                out.actions.push(TeamAction::Close);
            }
        });
    });
    if let Some(e) = &open.link_error
        && !open.feed_live
    {
        ui.add(egui::Label::new(RichText::new(e).small().color(t.text_faint)).wrap());
    }
    ui.horizontal(|ui| {
        let r = ui.button(tl!("Add Footage…"));
        out.elem("team.footage.add", r.rect, tl!("Add Footage…"));
        if r.on_hover_text(tl!("Choose video, audio or photos. They go into this project and upload to InFocus Drive so your teammates get them.")).clicked() {
            out.pick_footage = true;
        }
        if row.is_some_and(|r| r.role == TeamRole::Member) {
            let r = ui.button(tl!("Invite…"));
            out.elem("team.open.invite", r.rect, tl!("Invite…"));
            if r.clicked() {
                st.invite_for = Some(open.id.clone());
                st.picked.clear();
                st.query.clear();
                out.actions.push(TeamAction::LoadPeople);
            }
        }
        if let Some(row) = row {
            ui.label(RichText::new(footage_text(row.media_count, row.media_bytes)).small().color(t.text_dim));
        }
    });
    if open.exporting {
        ui.label(RichText::new(tl!("Downloads and uploads are paused while exporting.")).small().color(t.text_dim));
    }
    transfers(ui, t, open, out);
    ui.add_space(SECTION_GAP);
}

/// Downloads, proxies, uploads, teammates' uploads and failures, or a line saying how footage
/// gets here when nothing is moving.
fn transfers(ui: &mut egui::Ui, t: &Tokens, open: &TeamOpenView, out: &mut Out) {
    let idle = open.downloads.is_empty() && open.proxies.is_empty() && open.uploads.is_empty() && open.incoming.is_empty();
    if idle && open.failures.is_empty() && open.upload_failures.is_empty() {
        let text = tl!("Footage downloads to this Mac when you open a sequence that uses it, and this Mac makes its own proxies for smooth editing.");
        ui.add(egui::Label::new(RichText::new(text).small().color(t.text_faint)).wrap());
        return;
    }
    for (i, (name, frac)) in open.downloads.iter().enumerate() {
        let text = tlf!("Downloading {name}", name);
        progress_row(ui, t, &text, *frac, format!("team.download.{i}"), out);
    }
    for (i, (name, frac)) in open.proxies.iter().enumerate() {
        let text = tlf!("Making a proxy of {name}", name);
        progress_row(ui, t, &text, *frac, format!("team.proxy.{i}"), out);
    }
    for (i, (name, frac)) in open.uploads.iter().enumerate() {
        let text = tlf!("Uploading {name}", name);
        progress_row(ui, t, &text, *frac, format!("team.upload.{i}"), out);
    }
    for (i, (name, member)) in open.incoming.iter().enumerate() {
        let text = tlf!("{name}: uploading from {member}", name, member);
        let r = ui.add(egui::Label::new(RichText::new(&text).small().color(t.text_dim)).wrap());
        out.elem(format!("team.incoming.{i}"), r.rect, text);
    }
    if open.failures.is_empty() && open.upload_failures.is_empty() {
        return;
    }
    for (i, (name, why)) in open.failures.iter().enumerate() {
        let text = tlf!("{name} didn't download: {why}", name, why);
        let r = ui.add(egui::Label::new(RichText::new(&text).small().color(t.danger)).wrap());
        out.elem(format!("team.failure.{i}"), r.rect, text);
    }
    for (i, (name, why)) in open.upload_failures.iter().enumerate() {
        let text = tlf!("{name} didn't upload: {why}", name, why);
        let r = ui.add(egui::Label::new(RichText::new(&text).small().color(t.danger)).wrap());
        out.elem(format!("team.upload_failure.{i}"), r.rect, text);
    }
    let r = ui.button(tl!("Retry"));
    out.elem("team.retry", r.rect, tl!("Retry"));
    if r.on_hover_text(tl!("Try the failed downloads and uploads again.")).clicked() {
        out.actions.push(TeamAction::RetryDownloads);
    }
}

/// A name, its percentage and a progress bar (a download, a proxy or an upload).
fn progress_row(ui: &mut egui::Ui, t: &Tokens, name: &str, frac: f32, id: String, out: &mut Out) {
    let frac = if frac.is_finite() { frac.clamp(0.0, 1.0) } else { 0.0 };
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    let pct = format!("{:.0} %", frac * 100.0);
    let text_clip = Rect::from_min_max(r.min, pos2(r.max.x - 44.0, r.max.y));
    ui.painter().with_clip_rect(text_clip).text(r.min + vec2(0.0, 2.0), Align2::LEFT_TOP, name, Tokens::ui(12.0), t.text);
    ui.painter().text(pos2(r.max.x, r.min.y + 2.0), Align2::RIGHT_TOP, &pct, Tokens::ui(11.0), t.text_dim);
    let bar = Rect::from_min_max(pos2(r.min.x, r.max.y - 8.0), pos2(r.max.x, r.max.y - 3.0));
    ui.painter().rect_filled(bar, 2.0, t.field_bg);
    ui.painter().rect_filled(Rect::from_min_max(bar.min, pos2(bar.min.x + bar.width() * frac, bar.max.y)), 2.0, t.accent);
    out.elem(id, r, format!("{name}: {pct}"));
}
