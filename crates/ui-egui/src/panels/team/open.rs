//! The open team project: its connection, Add Footage…, and what this Mac is downloading, making
//! proxies of and uploading.

use egui::{Align2, Rect, RichText, Sense, pos2, vec2};

use super::{Out, PanelState, SECTION_GAP, footage_text, heading, split_row};
use crate::team_ui::{TeamAction, TeamOpenView, TeamProjectRow, TeamRole};
use crate::theme::Tokens;

pub(super) fn section(ui: &mut egui::Ui, t: &Tokens, open: &TeamOpenView, row: Option<&TeamProjectRow>, me: Option<&str>, st: &mut PanelState, out: &mut Out) {
    heading(ui, t, tl!("Open now"));
    let (dot, state) = if open.feed_live { (t.render_green, tl!("Live")) } else { (t.render_yellow, tl!("Reconnecting…")) };
    let mut name_rect = Rect::NOTHING;
    split_row(
        ui,
        |ui| {
            let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.0, dot);
            name_rect = ui.label(RichText::new(&open.name).strong().color(t.text)).rect;
            ui.label(RichText::new(state).small().color(t.text_dim));
        },
        |ui| {
            let r = ui.button(tl!("Close"));
            out.elem("team.leave", r.rect, tl!("Close"));
            if r.on_hover_text(tl!("Stop working on this project here. Your edits are sent to the Drive first and your locks are released.")).clicked() {
                out.actions.push(TeamAction::Close);
            }
        },
    );
    out.elem("team.open", name_rect, format!("{} ({state})", open.name));
    if let Some(e) = &open.link_error
        && !open.feed_live
    {
        ui.add(egui::Label::new(RichText::new(e).small().color(t.text_faint)).wrap());
    }
    ui.horizontal_wrapped(|ui| {
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
                out.actions.push(TeamAction::SearchPeople(String::new()));
            }
        }
    });
    if let Some(row) = row {
        // who is editing which sequence, then who is here and what's on the Drive, without
        // scrolling down to the project's card
        let lines = super::projects::editing_lines(row, me);
        if lines.is_empty() {
            let text = tl!("No one is editing right now");
            let r = ui.add(egui::Label::new(RichText::new(text).small().color(t.text_faint)).wrap());
            out.elem("team.open.editing", r.rect, text);
        }
        for (i, line) in lines.iter().enumerate() {
            let r = ui.add(egui::Label::new(RichText::new(line).small().color(t.render_green)).wrap());
            out.elem(format!("team.open.editing.{i}"), r.rect, line.clone());
        }
        let text = tlf!("{footage} on InFocus Drive", footage = footage_text(row.media_count, row.media_bytes));
        let r = ui.add(egui::Label::new(RichText::new(&text).small().color(t.text_dim)).wrap());
        out.elem("team.open.footage", r.rect, text);
        if !row.members.is_empty() {
            super::projects::members(ui, t, row, "team.open", out);
        }
    }
    drive_only(ui, t, open, out);
    if open.exporting {
        ui.label(RichText::new(tl!("Downloads and uploads are paused while exporting.")).small().color(t.text_dim));
    }
    transfers(ui, t, open, st, out);
    ui.add_space(SECTION_GAP);
}

/// Files a teammate put in the project's media folder from the Drive website that no item uses
/// yet: Add to Project brings them in (they download to this Mac and go into the project's bin
/// for everyone).
fn drive_only(ui: &mut egui::Ui, t: &Tokens, open: &TeamOpenView, out: &mut Out) {
    if open.drive_only.is_empty() {
        return;
    }
    let bytes = open.drive_only.iter().fold(0u64, |acc, (_, b)| acc.saturating_add(*b));
    let n = open.drive_only.len() as u64;
    let text = match n {
        1 => tlf!("1 new file on InFocus Drive · {size}", size = super::size_text(bytes)),
        n => tlf!("{n} new files on InFocus Drive · {size}", n, size = super::size_text(bytes)),
    };
    let frame = egui::Frame::new().fill(t.accent.gamma_multiply(0.10)).corner_radius(4.0).inner_margin(egui::Margin::symmetric(8, 6));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let mut text_rect = Rect::NOTHING;
        split_row(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    text_rect = ui.add(egui::Label::new(RichText::new(&text).color(t.text)).wrap()).rect;
                    let names: Vec<&str> = open.drive_only.iter().take(3).map(|(p, _)| p.rsplit('/').next().unwrap_or(p)).collect();
                    let more = open.drive_only.len().saturating_sub(names.len());
                    let list = if more == 0 { names.join(", ") } else { tlf!("{list} and {count} more", list = names.join(", "), count = more) };
                    ui.add(egui::Label::new(RichText::new(list).small().color(t.text_dim)).wrap());
                });
            },
            |ui| {
                let r = ui.button(tl!("Add to Project"));
                out.elem("team.drive_only.add", r.rect, tl!("Add to Project"));
                if r.on_hover_text(tl!("Added on the Drive website. They download to this Mac and go into the project's bin for the whole team.")).clicked() {
                    out.actions.push(TeamAction::AddDriveFootage);
                }
            },
        );
        out.elem("team.drive_only", text_rect, text.clone());
    });
    ui.add_space(4.0);
}

/// Downloads, proxies, uploads, teammates' uploads and failures, or a line saying how footage
/// gets here when nothing is moving.
fn transfers(ui: &mut egui::Ui, t: &Tokens, open: &TeamOpenView, st: &mut PanelState, out: &mut Out) {
    let idle = open.downloads.is_empty() && open.proxies.is_empty() && open.uploads.is_empty() && open.uploads_done.is_empty() && open.incoming.is_empty();
    st.seen_transfer |= !idle;
    if idle && open.failures.is_empty() && open.upload_failures.is_empty() {
        // said until the first transfer shows how it works, then out of the way
        if !st.seen_transfer {
            let text = tl!("Footage downloads to this Mac when you open a sequence that uses it, and this Mac makes its own proxies.");
            let r = ui.add(egui::Label::new(RichText::new(text).small().color(t.text_dim)).wrap());
            out.elem("team.open.howto", r.rect, text);
        }
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
        // after a quit the upload carries on where it stopped: say so
        let text = if open.uploads_resumed.contains(name) { tlf!("Resuming upload of {name}", name) } else { tlf!("Uploading {name}", name) };
        progress_row(ui, t, &text, *frac, format!("team.upload.{i}"), out);
    }
    // the "Add footage" step lists its own uploads
    let done: &[String] = if st.setup_open { &[] } else { &open.uploads_done };
    for (i, name) in done.iter().enumerate() {
        let text = tlf!("{name} uploaded", name);
        let r = ui.add(egui::Label::new(RichText::new(format!("✓ {text}")).small().color(t.render_green)).truncate());
        out.elem(format!("team.upload.done.{i}"), r.rect, text);
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
pub(super) fn progress_row(ui: &mut egui::Ui, t: &Tokens, name: &str, frac: f32, id: String, out: &mut Out) {
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
