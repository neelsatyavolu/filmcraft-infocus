//! Team panel (InFocus Cut): sign in to InFocus Drive, list the team projects with who is editing
//! and any cleanup notice, open one or make the current project a team project, and follow the
//! open project's connection and media downloads. It draws a [`TeamView`] the host's
//! [`TeamUi`](crate::team_ui::TeamUi) gives it and sends back [`TeamAction`]s; without that host
//! hook (the web app) it only says team editing needs the desktop app.
//!
//! Automation ids: `team.signIn`, `team.signOut`, `team.error`, `team.error.dismiss`,
//! `team.status`, `team.leave`, `team.download.<n>`, `team.failure.<n>`, `team.retry`,
//! `team.create.name`, `team.create`, `team.refresh`, `team.project.<id>`,
//! `team.project.<id>.open`, `team.project.<id>.cleanup`, `team.project.<id>.keep`,
//! `team.project.<id>.keepForever`.

use egui::{Align2, Color32, Rect, RichText, Sense, pos2, vec2};

use crate::FilmcraftApp;
use crate::team_ui::{TeamAction, TeamOpenView, TeamPhase, TeamProjectRow, TeamView};
use crate::theme::Tokens;

/// Automation elements collected while drawing: (id, rect, label).
type Elems = Vec<(String, Rect, String)>;

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    let Some(view) = app.hooks.team.as_ref().map(|team| team.view(&app.session)) else {
        crate::dock::placeholder(ui, rect, &t, tl!("Team editing is available in the desktop app."));
        return;
    };
    let in_team = app.session.team.is_some();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(8.0, 6.0))));
    let ui = &mut child;
    let mut elems: Elems = Vec::new();
    let mut actions: Vec<TeamAction> = Vec::new();
    egui::ScrollArea::vertical().id_salt("team-panel-scroll").auto_shrink([false, false]).show(ui, |ui| {
        account_row(ui, &t, &view, &mut elems, &mut actions);
        if let Some(e) = &view.error {
            error_row(ui, &t, e, &mut elems, &mut actions);
        }
        if let Some(busy) = &view.busy {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new(crate::i18n::t(busy)).color(t.text_dim));
            });
        }
        match view.phase {
            TeamPhase::SignedOut => signed_out(ui, &t, &mut elems, &mut actions),
            TeamPhase::Connecting => {}
            TeamPhase::SignedIn => {
                if let Some(open) = &view.open {
                    open_project(ui, &t, open, &mut elems, &mut actions);
                }
                create_row(ui, in_team, &mut elems, &mut actions);
                project_list(ui, &t, &view, &mut elems, &mut actions);
            }
        }
    });
    for (id, r, label) in elems {
        app.auto.add(&id, r, &label);
    }
    for action in actions {
        if let Some(team) = app.hooks.team.as_mut() {
            team.act(&mut app.session, action);
        }
    }
}

fn account_row(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, elems: &mut Elems, actions: &mut Vec<TeamAction>) {
    ui.horizontal(|ui| {
        let who = match (view.phase, &view.user) {
            (TeamPhase::SignedIn, Some(user)) => tlf!("Signed in as {user}", user),
            (TeamPhase::Connecting, _) => tl!("Connecting to InFocus Drive…").to_string(),
            _ => tl!("Not signed in").to_string(),
        };
        let r = ui.label(RichText::new(&who).color(t.text));
        elems.push(("team.status".into(), r.rect, who));
        if view.phase == TeamPhase::SignedIn {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = ui.button(tl!("Sign Out"));
                elems.push(("team.signOut".into(), r.rect, "Sign Out".into()));
                if r.clicked() {
                    actions.push(TeamAction::SignOut);
                }
            });
        }
    });
    ui.add_space(4.0);
}

fn error_row(ui: &mut egui::Ui, t: &Tokens, error: &str, elems: &mut Elems, actions: &mut Vec<TeamAction>) {
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 70.0).max(80.0);
        let r = ui.add_sized([width, 0.0], egui::Label::new(RichText::new(error).color(t.danger)).wrap());
        elems.push(("team.error".into(), r.rect, error.to_string()));
        let r = ui.small_button(tl!("Dismiss"));
        elems.push(("team.error.dismiss".into(), r.rect, "Dismiss".into()));
        if r.clicked() {
            actions.push(TeamAction::ClearError);
        }
    });
    ui.add_space(4.0);
}

fn signed_out(ui: &mut egui::Ui, t: &Tokens, elems: &mut Elems, actions: &mut Vec<TeamAction>) {
    ui.add_space(6.0);
    ui.label(RichText::new(tl!("Sign in to InFocus Drive to edit projects with your team.")).color(t.text_dim));
    ui.add_space(6.0);
    let r = ui.button(tl!("Sign In…"));
    elems.push(("team.signIn".into(), r.rect, "Sign In…".into()));
    if r.clicked() {
        actions.push(TeamAction::SignIn);
    }
    ui.label(RichText::new(tl!("Your browser opens to finish signing in.")).small().color(t.text_faint));
}

fn open_project(ui: &mut egui::Ui, t: &Tokens, open: &TeamOpenView, elems: &mut Elems, actions: &mut Vec<TeamAction>) {
    section(ui, t, tl!("Open project"));
    ui.horizontal(|ui| {
        let (dot, state) = if open.feed_live { (t.render_green, tl!("Live")) } else { (t.render_yellow, tl!("Reconnecting…")) };
        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 4.0, dot);
        let r = ui.label(RichText::new(&open.name).strong().color(t.text));
        ui.label(RichText::new(state).color(t.text_dim));
        elems.push(("team.open".into(), r.rect, format!("{} ({state})", open.name)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let r = ui.button(tl!("Leave"));
            elems.push(("team.leave".into(), r.rect, "Leave".into()));
            if r.on_hover_text(tl!("Stop editing as a team. Your locks are released.")).clicked() {
                actions.push(TeamAction::Close);
            }
        });
    });
    if let Some(e) = &open.link_error
        && !open.feed_live
    {
        ui.label(RichText::new(e).small().color(t.text_faint));
    }
    if open.exporting {
        ui.label(RichText::new(tl!("Downloads are paused while exporting.")).small().color(t.text_dim));
    }
    for (i, (name, frac)) in open.downloads.iter().enumerate() {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
        let pct = format!("{:.0} %", frac.clamp(0.0, 1.0) * 100.0);
        ui.painter().text(r.min + vec2(0.0, 2.0), Align2::LEFT_TOP, name, Tokens::ui(12.0), t.text);
        ui.painter().text(pos2(r.max.x, r.min.y + 2.0), Align2::RIGHT_TOP, &pct, Tokens::ui(11.0), t.text_dim);
        let bar = Rect::from_min_max(pos2(r.min.x, r.max.y - 8.0), pos2(r.max.x, r.max.y - 3.0));
        ui.painter().rect_filled(bar, 2.0, t.field_bg);
        ui.painter().rect_filled(Rect::from_min_max(bar.min, pos2(bar.min.x + bar.width() * frac.clamp(0.0, 1.0), bar.max.y)), 2.0, t.accent);
        elems.push((format!("team.download.{i}"), r, format!("{name}: {pct}")));
    }
    if !open.failures.is_empty() {
        for (i, (name, why)) in open.failures.iter().enumerate() {
            let text = tlf!("{name} didn't download: {why}", name, why);
            let r = ui.add(egui::Label::new(RichText::new(&text).small().color(t.danger)).wrap());
            elems.push((format!("team.failure.{i}"), r.rect, text));
        }
        let r = ui.button(tl!("Retry"));
        elems.push(("team.retry".into(), r.rect, "Retry".into()));
        if r.clicked() {
            actions.push(TeamAction::RetryDownloads);
        }
    }
    ui.add_space(8.0);
}

fn create_row(ui: &mut egui::Ui, in_team: bool, elems: &mut Elems, actions: &mut Vec<TeamAction>) {
    if in_team {
        return;
    }
    let draft_id = egui::Id::unique("team-create-name");
    let mut name = ui.data_mut(|d| d.get_temp::<String>(draft_id)).unwrap_or_default();
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 150.0).max(80.0);
        let r = ui.add(egui::TextEdit::singleline(&mut name).hint_text(tl!("Team project name")).desired_width(width));
        elems.push(("team.create.name".into(), r.rect, name.clone()));
        let ok = !name.trim().is_empty();
        let r = ui.add_enabled(ok, egui::Button::new(tl!("Create Team Project")));
        elems.push(("team.create".into(), r.rect, "Create Team Project".into()));
        if r.on_hover_text(tl!("Puts the open project on InFocus Drive as a team project.")).clicked() && ok {
            actions.push(TeamAction::CreateFromCurrent(name.trim().to_string()));
            name.clear();
        }
    });
    ui.data_mut(|d| d.insert_temp(draft_id, name));
    ui.add_space(8.0);
}

fn project_list(ui: &mut egui::Ui, t: &Tokens, view: &TeamView, elems: &mut Elems, actions: &mut Vec<TeamAction>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(tl!("Team projects")).strong().color(t.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let r = ui.small_button(tl!("Refresh"));
            elems.push(("team.refresh".into(), r.rect, "Refresh".into()));
            if r.clicked() {
                actions.push(TeamAction::Refresh);
            }
        });
    });
    ui.separator();
    if view.projects.is_empty() {
        ui.label(RichText::new(tl!("No team projects yet.")).color(t.text_faint));
        return;
    }
    let open_id = view.open.as_ref().map(|o| o.id.as_str());
    for (i, p) in view.projects.iter().enumerate() {
        project_row(ui, t, p, open_id == Some(p.id.as_str()), i % 2 == 1, elems, actions);
    }
}

fn project_row(ui: &mut egui::Ui, t: &Tokens, p: &TeamProjectRow, is_open: bool, alt: bool, elems: &mut Elems, actions: &mut Vec<TeamAction>) {
    let frame = egui::Frame::new().fill(if alt { t.row_alt } else { Color32::TRANSPARENT }).inner_margin(egui::Margin::symmetric(6, 4)).corner_radius(3.0);
    let r = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(&p.name).strong().color(t.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if is_open {
                    ui.label(RichText::new(tl!("Open")).color(t.text_dim));
                } else {
                    let r = ui.button(tl!("Open"));
                    elems.push((format!("team.project.{}.open", p.id), r.rect, "Open".into()));
                    if r.clicked() {
                        actions.push(TeamAction::Open(p.id.clone()));
                    }
                }
            });
        });
        let editing = if p.editors.is_empty() { tl!("No one is editing").to_string() } else { tlf!("Editing now: {names}", names = p.editors.join(", ")) };
        ui.label(RichText::new(editing).small().color(t.text_dim));
        if let Some(day) = &p.moves_on
            && !p.keep_forever
        {
            ui.horizontal(|ui| {
                let text = tlf!("Moves to the Drive's recycle bin on {date}", date = day);
                let r = ui.label(RichText::new(&text).small().color(t.render_yellow));
                elems.push((format!("team.project.{}.cleanup", p.id), r.rect, text));
                let r = ui.small_button(tl!("Keep"));
                elems.push((format!("team.project.{}.keep", p.id), r.rect, "Keep".into()));
                if r.on_hover_text(tl!("Keeps the project for another 30 days.")).clicked() {
                    actions.push(TeamAction::Keep(p.id.clone()));
                }
            });
        }
        let mut keep = p.keep_forever;
        let r = ui.checkbox(&mut keep, RichText::new(tl!("Keep forever")).small());
        elems.push((format!("team.project.{}.keepForever", p.id), r.rect, "Keep forever".into()));
        if r.on_hover_text(tl!("Never move this project to the recycle bin. Only its creator or a Drive admin can change this.")).changed() {
            actions.push(TeamAction::SetKeepForever(p.id.clone(), keep));
        }
    });
    elems.push((format!("team.project.{}", p.id), r.response.rect, p.name.clone()));
}

fn section(ui: &mut egui::Ui, t: &Tokens, title: &str) {
    ui.label(RichText::new(title).small().color(t.text_faint));
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use filmcraft_engine::Session;

    use super::*;
    use crate::team_ui::TeamUi;

    /// A team host that shows a fixed view and records what the panel asks for.
    struct Fake {
        view: TeamView,
        acts: Rc<RefCell<Vec<TeamAction>>>,
    }

    impl TeamUi for Fake {
        fn poll(&mut self, _s: &mut Session) {}
        fn view(&self, _s: &Session) -> TeamView {
            self.view.clone()
        }
        fn act(&mut self, _s: &mut Session, action: TeamAction) {
            self.acts.borrow_mut().push(action);
        }
        fn active(&self) -> bool {
            false
        }
    }

    fn app_with(view: TeamView) -> (FilmcraftApp, Rc<RefCell<Vec<TeamAction>>>) {
        let acts: Rc<RefCell<Vec<TeamAction>>> = Rc::default();
        let mut app = FilmcraftApp::new(Session::default());
        app.hooks.team = Some(Box::new(Fake { view, acts: acts.clone() }));
        (app, acts)
    }

    struct Harness {
        ctx: egui::Context,
        time: f64,
    }

    impl Harness {
        fn frame(&mut self, app: &mut FilmcraftApp, events: Vec<egui::Event>) {
            app.auto.begin_frame();
            self.time += 0.05;
            let input = egui::RawInput {
                time: Some(self.time),
                events,
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 600.0))),
                ..Default::default()
            };
            self.ctx
                .run_ui(input, |ui| {
                    let rect = ui.max_rect();
                    show(app, ui, rect);
                })
                .textures_delta
                .clear();
        }
        fn click(&mut self, app: &mut FilmcraftApp, id: &str) {
            let e = app.auto.find(id).unwrap_or_else(|| panic!("no {id}"));
            let p = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
            let b = |pressed| egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
            for events in [vec![egui::Event::PointerMoved(p)], vec![b(true)], vec![b(false)], Vec::new()] {
                self.frame(app, events);
            }
        }
    }

    fn harness() -> Harness {
        Harness { ctx: egui::Context::default(), time: 0.0 }
    }

    fn label(app: &FilmcraftApp, id: &str) -> Option<String> {
        app.auto.find(id).map(|e| e.label.clone())
    }

    #[test]
    fn team_panel_lists_projects_from_fake_client() {
        let view = TeamView {
            phase: TeamPhase::SignedIn,
            user: Some("maya".into()),
            projects: vec![
                TeamProjectRow { id: "p1".into(), name: "Robotics".into(), editors: vec!["ana".into()], moves_on: None, keep_forever: false },
                TeamProjectRow { id: "p2".into(), name: "Homecoming".into(), editors: vec![], moves_on: Some("Nov 30, 2026".into()), keep_forever: false },
            ],
            ..Default::default()
        };
        let (mut app, acts) = app_with(view);
        let mut h = harness();
        for _ in 0..3 {
            h.frame(&mut app, Vec::new());
        }
        assert_eq!(label(&app, "team.status").as_deref(), Some("Signed in as maya"));
        assert_eq!(label(&app, "team.project.p1").as_deref(), Some("Robotics"));
        assert_eq!(label(&app, "team.project.p2").as_deref(), Some("Homecoming"));
        assert_eq!(label(&app, "team.project.p2.cleanup").as_deref(), Some("Moves to the Drive's recycle bin on Nov 30, 2026"));
        assert!(app.auto.find("team.project.p1.cleanup").is_none(), "no notice, no banner");
        h.click(&mut app, "team.project.p2.keep");
        h.click(&mut app, "team.project.p1.open");
        assert_eq!(*acts.borrow(), vec![TeamAction::Keep("p2".into()), TeamAction::Open("p1".into())]);
    }

    #[test]
    fn signed_out_offers_sign_in() {
        let (mut app, acts) = app_with(TeamView::default());
        let mut h = harness();
        for _ in 0..3 {
            h.frame(&mut app, Vec::new());
        }
        assert_eq!(label(&app, "team.status").as_deref(), Some("Not signed in"));
        assert!(app.auto.find("team.signOut").is_none());
        h.click(&mut app, "team.signIn");
        assert_eq!(*acts.borrow(), vec![TeamAction::SignIn]);
    }

    #[test]
    fn open_project_shows_downloads_failures_and_connection() {
        let view = TeamView {
            phase: TeamPhase::SignedIn,
            user: Some("maya".into()),
            error: Some("The Drive refused: only the creator can change this".into()),
            open: Some(TeamOpenView {
                id: "p1".into(),
                name: "Robotics".into(),
                feed_live: false,
                link_error: Some("connection reset".into()),
                downloads: vec![("A001.mov".into(), 0.25)],
                failures: vec![("A002.mov".into(), "disk full".into())],
                exporting: false,
            }),
            projects: vec![TeamProjectRow { id: "p1".into(), name: "Robotics".into(), ..Default::default() }],
            ..Default::default()
        };
        let (mut app, acts) = app_with(view);
        let mut h = harness();
        for _ in 0..3 {
            h.frame(&mut app, Vec::new());
        }
        assert_eq!(label(&app, "team.open").as_deref(), Some("Robotics (Reconnecting…)"));
        assert_eq!(label(&app, "team.download.0").as_deref(), Some("A001.mov: 25 %"));
        assert_eq!(label(&app, "team.failure.0").as_deref(), Some("A002.mov didn't download: disk full"));
        assert!(app.auto.find("team.project.p1.open").is_none(), "the open project has no Open button");
        h.click(&mut app, "team.retry");
        h.click(&mut app, "team.error.dismiss");
        assert_eq!(*acts.borrow(), vec![TeamAction::RetryDownloads, TeamAction::ClearError]);
    }

    #[test]
    fn without_the_host_hook_there_is_no_team_panel() {
        let app = FilmcraftApp::new(Session::default());
        let team = crate::menus::panel_command_id(crate::dock::PanelKind::Team);
        assert!(crate::menus::menu_items(&app).iter().all(|it| it.id != team));
        let (app, _) = app_with(TeamView::default());
        assert!(crate::menus::menu_items(&app).iter().any(|it| it.id == team));
    }
}
