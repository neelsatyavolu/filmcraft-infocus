//! Invitations, the team project list and the "New Team Project" form.

use egui::{Color32, RichText};

use super::{Out, PanelState, SECTION_GAP, footage_text, heading, primary_button};
use crate::team_ui::{TeamAction, TeamProjectRow, TeamRole, TeamView};
use crate::theme::Tokens;

/// Invitations to projects this member hasn't joined: at the top, with Join and Decline.
pub(super) fn invitations(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, out: &mut Out) {
    let invited: Vec<&TeamProjectRow> = view.projects.iter().filter(|p| p.role == TeamRole::Invited).collect();
    if invited.is_empty() {
        return;
    }
    heading(ui, t, tl!("Invitations"));
    for p in invited {
        let frame = egui::Frame::new().fill(t.accent.gamma_multiply(0.14)).corner_radius(4.0).inner_margin(egui::Margin::symmetric(8, 6));
        let r = frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&p.name).strong().color(t.text));
                    let by = match &p.invited_by {
                        Some(by) => tlf!("Invited by {name}", name = by),
                        None => tl!("You're invited to this project").to_string(),
                    };
                    ui.label(RichText::new(by).small().color(t.text_dim));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let r = ui.button(tl!("Decline"));
                    out.elem(format!("team.invitation.{}.decline", p.id), r.rect, tl!("Decline"));
                    if r.on_hover_text(tl!("Turn the invitation down. A teammate can invite you again.")).clicked() {
                        out.actions.push(TeamAction::Decline(p.id.clone()));
                    }
                    let r = primary_button(ui, t, tl!("Join"), true);
                    out.elem(format!("team.invitation.{}.join", p.id), r.rect, tl!("Join"));
                    if r.on_hover_text(tl!("Join and open the project. Footage downloads when you open a sequence.")).clicked() {
                        out.actions.push(TeamAction::Join(p.id.clone()));
                    }
                });
            });
        });
        out.elem(format!("team.invitation.{}", p.id), r.response.rect, p.name.clone());
        ui.add_space(4.0);
    }
    ui.add_space(SECTION_GAP);
}

/// The member's team projects, the New Team Project form and the empty state.
pub(super) fn list(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, in_team: bool, suggested: Option<&str>, st: &mut PanelState, out: &mut Out) {
    let mine: Vec<&TeamProjectRow> = view.projects.iter().filter(|p| p.role != TeamRole::Invited).collect();
    let invited = view.projects.len() > mine.len();
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!("Your team projects")).strong().color(t.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let r = ui.small_button(tl!("Refresh"));
            out.elem("team.refresh", r.rect, tl!("Refresh"));
            if r.on_hover_text(tl!("Check InFocus Drive for new invitations and changes.")).clicked() {
                out.actions.push(TeamAction::Refresh);
            }
            if !st.creating && !mine.is_empty() {
                new_button(ui, in_team, suggested, st, out);
            }
        });
    });
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top(), egui::Stroke::new(1.0, t.separator));
    ui.add_space(6.0);
    if st.creating {
        create_form(ui, t, in_team, st, out);
    }
    if mine.is_empty() {
        if !st.creating {
            empty_state(ui, t, in_team, invited, suggested, st, out);
        }
        return;
    }
    let open_id = view.open.as_ref().map(|o| o.id.as_str());
    for (i, p) in mine.into_iter().enumerate() {
        row(ui, t, p, view.user.as_deref(), open_id == Some(p.id.as_str()), i % 2 == 1, st, out);
    }
}

fn new_button(ui: &mut egui::Ui, in_team: bool, suggested: Option<&str>, st: &mut PanelState, out: &mut Out) {
    let r = ui.add_enabled(!in_team, egui::Button::new(tl!("New Team Project…")).small());
    out.elem("team.new", r.rect, tl!("New Team Project…"));
    let tip = if in_team {
        tl!("Leave the open team project first, then make a new one.")
    } else {
        tl!("Put the project you have open on InFocus Drive for your team.")
    };
    if r.on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
        st.creating = true;
        st.name = suggested.unwrap_or_default().to_string();
    }
}

/// No team projects yet: say what a team project is and what to do next.
fn empty_state(ui: &mut egui::Ui, t: &Tokens, in_team: bool, invited: bool, suggested: Option<&str>, st: &mut PanelState, out: &mut Out) {
    ui.add_space(8.0);
    ui.vertical_centered(|ui| {
        ui.set_max_width(360.0);
        let title = tl!("No team projects yet");
        let r = ui.label(RichText::new(title).strong().color(t.text));
        out.elem("team.empty", r.rect, title);
        let body = if invited {
            tl!("Join an invitation above, or make a team project from the project you have open.")
        } else {
            tl!("Make one from the project you have open, then invite your teammates. Or ask a teammate to invite you: invitations show up here.")
        };
        ui.add(egui::Label::new(RichText::new(body).color(t.text_dim)).wrap());
        ui.add_space(8.0);
        let r = primary_button(ui, t, tl!("New Team Project…"), !in_team);
        out.elem("team.new", r.rect, tl!("New Team Project…"));
        if r.clicked() {
            st.creating = true;
            st.name = suggested.unwrap_or_default().to_string();
        }
    });
    ui.add_space(8.0);
}

/// Name the new team project; it is made from the open project.
fn create_form(ui: &mut egui::Ui, t: &Tokens, in_team: bool, st: &mut PanelState, out: &mut Out) {
    let frame = egui::Frame::new().fill(t.field_bg).stroke(egui::Stroke::new(1.0, t.separator)).corner_radius(4.0).inner_margin(egui::Margin::same(10));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(tl!("New team project")).strong().color(t.text));
        ui.add(
            egui::Label::new(
                RichText::new(tl!(
                    "Your open project, its sequences and its footage go to InFocus Drive. Footage uploads in the background while you keep editing."
                ))
                .small()
                .color(t.text_dim),
            )
            .wrap(),
        );
        ui.add_space(6.0);
        let r = ui.add(egui::TextEdit::singleline(&mut st.name).hint_text(tl!("Project name, like “Homecoming Video”")).desired_width(ui.available_width()));
        out.elem("team.create.name", r.rect, st.name.clone());
        let submit = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let ok = !st.name.trim().is_empty() && !in_team;
            let r = primary_button(ui, t, tl!("Create and Invite…"), ok);
            out.elem("team.create", r.rect, tl!("Create and Invite…"));
            if (r.clicked() || submit) && ok {
                out.actions.push(TeamAction::CreateFromCurrent(st.name.trim().to_string()));
                st.creating = false;
                st.name.clear();
            }
            let r = ui.button(tl!("Cancel"));
            out.elem("team.create.cancel", r.rect, tl!("Cancel"));
            if r.clicked() {
                st.creating = false;
                st.name.clear();
            }
        });
    });
    ui.add_space(SECTION_GAP);
}

/// "Ana is editing Main Edit", "You're editing a sequence": who is editing what, one line each.
fn editing_lines(p: &TeamProjectRow, me: Option<&str>) -> Vec<String> {
    let line = |who: &str, seq: Option<&str>| match (me == Some(who), seq) {
        (true, Some(seq)) => tlf!("You're editing {sequence}", sequence = seq),
        (true, None) => tl!("You're editing a sequence").to_string(),
        (false, Some(seq)) => tlf!("{name} is editing {sequence}", name = who, sequence = seq),
        (false, None) => tlf!("{name} is editing a sequence", name = who),
    };
    if !p.editing.is_empty() {
        return p.editing.iter().map(|(who, seq)| line(who, seq.as_deref())).collect();
    }
    p.editors.iter().map(|who| line(who, None)).collect()
}

#[allow(clippy::too_many_arguments)]
fn row(ui: &mut egui::Ui, t: &Tokens, p: &TeamProjectRow, me: Option<&str>, is_open: bool, alt: bool, st: &mut PanelState, out: &mut Out) {
    let fill = if is_open {
        t.row_selected.gamma_multiply(0.5)
    } else if alt {
        t.row_alt
    } else {
        Color32::TRANSPARENT
    };
    let frame = egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(8, 6)).corner_radius(4.0);
    let r = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(&p.name).strong().color(t.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if is_open {
                    ui.label(RichText::new(tl!("Open now")).small().color(t.text_dim));
                } else {
                    let r = ui.button(tl!("Open"));
                    out.elem(format!("team.project.{}.open", p.id), r.rect, tl!("Open"));
                    if r.on_hover_text(tl!("Open this project here. Footage downloads when you open a sequence.")).clicked() {
                        out.actions.push(TeamAction::Open(p.id.clone()));
                    }
                }
            });
        });
        let small = |ui: &mut egui::Ui, text: &str, color| ui.add(egui::Label::new(RichText::new(text).small().color(color)).wrap());
        if !p.members.is_empty() {
            let names: Vec<&str> = p.members.iter().map(|m| if m.name.is_empty() { m.username.as_str() } else { m.name.as_str() }).collect();
            let text = tlf!("Team: {names}", names = names.join(", "));
            let r = small(ui, &text, t.text_dim);
            out.elem(format!("team.project.{}.members", p.id), r.rect, text);
        } else if p.role == TeamRole::Open {
            small(ui, tl!("Open to everyone on InFocus Drive"), t.text_dim);
        }
        for inv in &p.invites {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(tlf!("Invited: {name}", name = if inv.name.is_empty() { &inv.username } else { &inv.name })).small().color(t.text_faint),
                );
                let r = ui.small_button(tl!("Cancel"));
                out.elem(format!("team.project.{}.invited.{}.cancel", p.id, inv.username), r.rect, tl!("Cancel"));
                if r.on_hover_text(tl!("Take this invitation back.")).clicked() {
                    out.actions.push(TeamAction::CancelInvite(p.id.clone(), inv.username.clone()));
                }
            });
        }
        let lines = editing_lines(p, me);
        if lines.is_empty() {
            small(ui, tl!("No one is editing right now"), t.text_faint);
        }
        for line in lines {
            small(ui, &line, t.render_green);
        }
        let footage = footage_text(p.media_count, p.media_bytes);
        let r = small(ui, &footage, t.text_dim);
        out.elem(format!("team.project.{}.footage", p.id), r.rect, footage);
        cleanup(ui, t, p, out);
        actions(ui, t, p, st, out);
    });
    out.elem(format!("team.project.{}", p.id), r.response.rect, p.name.clone());
    ui.add_space(2.0);
}

fn cleanup(ui: &mut egui::Ui, t: &Tokens, p: &TeamProjectRow, out: &mut Out) {
    let Some(day) = &p.moves_on else { return };
    if p.keep_forever {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        let text = tlf!("Moves to the Drive's recycle bin on {date}", date = day);
        let r = ui.label(RichText::new(&text).small().color(t.render_yellow));
        out.elem(format!("team.project.{}.cleanup", p.id), r.rect, text);
        let r = ui.small_button(tl!("Keep"));
        out.elem(format!("team.project.{}.keep", p.id), r.rect, tl!("Keep"));
        if r.on_hover_text(tl!("Keeps the project for another 30 days.")).clicked() {
            out.actions.push(TeamAction::Keep(p.id.clone()));
        }
    });
}

/// Invite…, Keep forever and Leave Project (with a confirmation).
fn actions(ui: &mut egui::Ui, t: &Tokens, p: &TeamProjectRow, st: &mut PanelState, out: &mut Out) {
    ui.add_space(2.0);
    if st.confirm_leave.as_deref() == Some(p.id.as_str()) {
        ui.add(
            egui::Label::new(
                RichText::new(tl!("Leave this project? It disappears from your list, and your teammates keep working on it. Someone can invite you back."))
                    .small()
                    .color(t.text),
            )
            .wrap(),
        );
        ui.horizontal(|ui| {
            let r = ui.button(tl!("Leave Project"));
            out.elem("team.confirm.leave", r.rect, tl!("Leave Project"));
            if r.clicked() {
                out.actions.push(TeamAction::LeaveTeam(p.id.clone()));
                st.confirm_leave = None;
            }
            let r = ui.button(tl!("Cancel"));
            out.elem("team.confirm.cancel", r.rect, tl!("Cancel"));
            if r.clicked() {
                st.confirm_leave = None;
            }
        });
        return;
    }
    ui.horizontal(|ui| {
        if p.role == TeamRole::Member {
            let r = ui.small_button(tl!("Invite…"));
            out.elem(format!("team.project.{}.invite", p.id), r.rect, tl!("Invite…"));
            if r.on_hover_text(tl!("Invite teammates to this project.")).clicked() {
                st.invite_for = Some(p.id.clone());
                st.picked.clear();
                st.query.clear();
                out.actions.push(TeamAction::LoadPeople);
            }
        }
        let mut keep = p.keep_forever;
        let r = ui.checkbox(&mut keep, RichText::new(tl!("Keep forever")).small());
        out.elem(format!("team.project.{}.keepForever", p.id), r.rect, tl!("Keep forever"));
        if r.on_hover_text(tl!("Never move this project to the recycle bin. Only its creator or a Drive admin can change this.")).changed() {
            out.actions.push(TeamAction::SetKeepForever(p.id.clone(), keep));
        }
        if p.role == TeamRole::Member {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = ui.small_button(RichText::new(tl!("Leave Project…")).color(t.text_dim));
                out.elem(format!("team.project.{}.leaveTeam", p.id), r.rect, tl!("Leave Project…"));
                if r.clicked() {
                    st.confirm_leave = Some(p.id.clone());
                }
            });
        }
    });
}
