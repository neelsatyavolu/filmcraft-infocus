use std::cell::RefCell;
use std::rc::Rc;

use egui::{pos2, vec2};
use filmcraft_engine::Session;

use super::*;
use crate::team_ui::{TeamOpenView, TeamPerson, TeamProjectRow, TeamRole, TeamUi};

/// A team host that shows a fixed view and records what the panel asks for.
struct Fake {
    view: Rc<RefCell<TeamView>>,
    acts: Rc<RefCell<Vec<TeamAction>>>,
}

impl TeamUi for Fake {
    fn poll(&mut self, _s: &mut Session) {}
    fn view(&self, _s: &Session) -> TeamView {
        self.view.borrow().clone()
    }
    fn act(&mut self, _s: &mut Session, action: TeamAction) {
        if action == TeamAction::DismissInvitePrompt {
            self.view.borrow_mut().invite_prompt = None;
        }
        self.acts.borrow_mut().push(action);
    }
    fn export_missing(&mut self, _s: &Session, _sequences: &[filmcraft_project::ItemId]) -> Vec<filmcraft_project::ItemId> {
        Vec::new()
    }
    fn set_exporting(&mut self, _exporting: bool) {}
    fn exporting(&self) -> bool {
        false
    }
    fn active(&self) -> bool {
        false
    }
}

type Acts = Rc<RefCell<Vec<TeamAction>>>;

fn app_with(view: TeamView) -> (FilmcraftApp, Acts, Rc<RefCell<TeamView>>) {
    let acts: Acts = Rc::default();
    let view = Rc::new(RefCell::new(view));
    let mut app = FilmcraftApp::new(Session::default());
    app.hooks.team = Some(Box::new(Fake { view: view.clone(), acts: acts.clone() }));
    (app, acts, view)
}

struct Harness {
    ctx: egui::Context,
    time: f64,
    width: f32,
}

impl Harness {
    fn frame(&mut self, app: &mut FilmcraftApp, events: Vec<egui::Event>) {
        app.auto.begin_frame();
        self.time += 0.05;
        let input = egui::RawInput {
            time: Some(self.time),
            events,
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(self.width, 900.0))),
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
    fn frames(&mut self, app: &mut FilmcraftApp, n: usize) {
        for _ in 0..n {
            self.frame(app, Vec::new());
        }
    }
    fn click(&mut self, app: &mut FilmcraftApp, id: &str) {
        let e = app.auto.find(id).unwrap_or_else(|| panic!("no {id}"));
        let p = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
        let b = |pressed| egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        for events in [vec![egui::Event::PointerMoved(p)], vec![b(true)], vec![b(false)], Vec::new()] {
            self.frame(app, events);
        }
    }
    fn type_text(&mut self, app: &mut FilmcraftApp, id: &str, text: &str) {
        self.click(app, id);
        self.frame(app, vec![egui::Event::Text(text.into())]);
        self.frame(app, Vec::new());
    }
}

fn harness() -> Harness {
    Harness { ctx: egui::Context::default(), time: 0.0, width: 420.0 }
}

fn label(app: &FilmcraftApp, id: &str) -> Option<String> {
    app.auto.find(id).map(|e| e.label.clone())
}

fn person(u: &str, name: &str) -> TeamPerson {
    TeamPerson { username: u.into(), name: name.into(), ..Default::default() }
}

fn signed_in(projects: Vec<TeamProjectRow>) -> TeamView {
    TeamView { phase: TeamPhase::SignedIn, user: Some("maya".into()), projects, ..Default::default() }
}

#[test]
fn team_panel_lists_projects_with_members_editors_and_footage() {
    let view = signed_in(vec![
        TeamProjectRow {
            id: "p1".into(),
            name: "Robotics".into(),
            members: vec![person("maya", "Maya"), person("ana", "")],
            invites: vec![person("leo", "Leo")],
            editors: vec!["ana".into()],
            editing: vec![("ana".into(), Some("Main Edit".into()))],
            media_count: 3,
            media_bytes: 1_500_000_000,
            ..Default::default()
        },
        TeamProjectRow { id: "p2".into(), name: "Homecoming".into(), moves_on: Some("Nov 30, 2026".into()), ..Default::default() },
    ]);
    let (mut app, acts, _) = app_with(view);
    let mut h = harness();
    h.frames(&mut app, 3);
    assert_eq!(label(&app, "team.status").as_deref(), Some("Signed in as maya"));
    assert_eq!(label(&app, "team.project.p1").as_deref(), Some("Robotics"));
    assert_eq!(label(&app, "team.project.p1.members").as_deref(), Some("Team: Maya, ana"), "a member without a display name shows by user name");
    assert_eq!(label(&app, "team.project.p1.footage").as_deref(), Some("3 files · 1.5 GB"));
    assert_eq!(label(&app, "team.project.p2.footage").as_deref(), Some("No footage yet"));
    assert_eq!(label(&app, "team.project.p2.cleanup").as_deref(), Some("Moves to the Drive's recycle bin on Nov 30, 2026"));
    assert!(app.auto.find("team.project.p1.cleanup").is_none(), "no notice, no banner");
    assert!(app.auto.find("team.empty").is_none());
    h.click(&mut app, "team.project.p2.keep");
    h.click(&mut app, "team.project.p1.open");
    h.click(&mut app, "team.project.p1.invited.leo.cancel");
    assert_eq!(*acts.borrow(), vec![TeamAction::Keep("p2".into()), TeamAction::Open("p1".into()), TeamAction::CancelInvite("p1".into(), "leo".into())]);
}

#[test]
fn signed_out_offers_sign_in() {
    let (mut app, acts, _) = app_with(TeamView::default());
    let mut h = harness();
    h.frames(&mut app, 3);
    assert_eq!(label(&app, "team.status").as_deref(), Some("Not signed in"));
    assert!(app.auto.find("team.signOut").is_none());
    assert_eq!(label(&app, "team.signIn").as_deref(), Some("Sign in with InFocus Drive"));
    h.click(&mut app, "team.signIn");
    assert_eq!(*acts.borrow(), vec![TeamAction::SignIn]);
}

#[test]
fn account_chip_names_the_member_and_switches_account() {
    let mut view = signed_in(Vec::new());
    view.user_name = Some("Maya Lee".into());
    view.dashboard_url = Some("https://editing.example.com".into());
    let (mut app, acts, _) = app_with(view);
    let mut h = harness();
    h.frames(&mut app, 3);
    assert_eq!(label(&app, "team.status").as_deref(), Some("Signed in as Maya Lee (maya)"));
    h.click(&mut app, "team.switchAccount");
    h.click(&mut app, "team.dashboard");
    h.click(&mut app, "team.signOut");
    assert_eq!(*acts.borrow(), vec![TeamAction::SwitchAccount, TeamAction::OpenDashboard, TeamAction::SignOut]);
}

#[test]
fn members_show_who_is_online() {
    let mut ana = person("ana", "Ana");
    ana.online = Some(true);
    let mut leo = person("leo", "Leo");
    leo.last_seen = Some(1_000);
    let view = signed_in(vec![TeamProjectRow { id: "p1".into(), name: "Robotics".into(), members: vec![ana, leo], ..Default::default() }]);
    let (mut app, _, _) = app_with(view);
    let mut h = harness();
    h.frames(&mut app, 3);
    assert_eq!(label(&app, "team.project.p1.member.ana").as_deref(), Some("Ana: Online now"));
    assert!(label(&app, "team.project.p1.member.leo").is_some_and(|l| l.starts_with("Leo: Last seen")));
    assert_eq!(projects::presence_text(None, Some(1_000), 1_000 + 3 * 3600).as_deref(), Some("Last seen 3 hours ago"));
    assert_eq!(projects::presence_text(None, Some(1_000), 1_030).as_deref(), Some("Last seen just now"));
    assert_eq!(projects::presence_text(None, None, 5), None);
}

#[test]
fn errors_say_what_to_do_next() {
    assert!(next_step("Can't reach InFocus Drive: connection refused").is_some_and(|s| s.contains("online")));
    assert!(next_step("Sign in to InFocus Drive again (expired)").is_some());
    assert_eq!(next_step("Can't reach InFocus Drive right now. Check your connection, or try again in a minute."), None, "already says what to do");
    assert_eq!(next_step("This project already has 12 people."), None, "the Drive's own words are enough");
}

#[test]
fn an_invitation_can_be_joined_or_declined() {
    let view = signed_in(vec![TeamProjectRow {
        id: "p9".into(),
        name: "Spring Musical".into(),
        role: TeamRole::Invited,
        invited_by: Some("Ana".into()),
        ..Default::default()
    }]);
    let (mut app, acts, _) = app_with(view);
    let mut h = harness();
    h.frames(&mut app, 3);
    assert_eq!(label(&app, "team.invitation.p9").as_deref(), Some("Spring Musical"));
    assert!(app.auto.find("team.project.p9").is_none(), "an invitation isn't one of your projects yet");
    assert!(app.auto.find("team.empty").is_some(), "with only an invitation the list says what to do");
    h.click(&mut app, "team.invitation.p9.join");
    h.click(&mut app, "team.invitation.p9.decline");
    assert_eq!(*acts.borrow(), vec![TeamAction::Join("p9".into()), TeamAction::Decline("p9".into())]);
}

#[test]
fn new_project_ends_on_the_invite_step() {
    let (mut app, acts, view) = app_with(signed_in(Vec::new()));
    let mut h = harness();
    h.frames(&mut app, 3);
    assert_eq!(label(&app, "team.empty").as_deref(), Some("No team projects yet"));
    h.click(&mut app, "team.new");
    h.type_text(&mut app, "team.create.name", "Robotics");
    h.click(&mut app, "team.create");
    assert_eq!(*acts.borrow(), vec![TeamAction::CreateFromCurrent("Robotics".into())]);

    // the Drive made it: the host asks to invite teammates
    {
        let mut v = view.borrow_mut();
        v.projects = vec![TeamProjectRow { id: "p1".into(), name: "Robotics".into(), members: vec![person("maya", "Maya")], ..Default::default() }];
        v.people = vec![person("ana", "Ana Ruiz"), person("leo", "Leo"), person("maya2", "")];
        v.invite_prompt = Some("p1".into());
        v.open = Some(TeamOpenView { id: "p1".into(), name: "Robotics".into(), feed_live: true, ..Default::default() });
    }
    h.frames(&mut app, 2);
    // first the footage step: drop or choose files, each upload with its own bar
    assert!(app.auto.find("team.invite.send").is_none(), "footage comes before inviting");
    assert!(app.auto.find("team.setup.drop").is_some());
    view.borrow_mut().open.as_mut().unwrap().uploads = vec![("A001.mov".into(), 0.4)];
    h.frames(&mut app, 2);
    assert_eq!(label(&app, "team.setup.upload.0").as_deref(), Some("Uploading A001.mov: 40 %"));
    h.click(&mut app, "team.setup.next");
    h.frames(&mut app, 2);
    assert!(app.auto.find("team.setup.drop").is_none());
    assert!(app.auto.find("team.invite.person.ana").is_some());
    assert!(app.auto.find("team.invite.person.maya").is_none(), "members aren't offered");
    assert_eq!(label(&app, "team.invite.skip").as_deref(), Some("Skip for Now"));
    // search narrows the list
    h.type_text(&mut app, "team.invite.search", "ruiz");
    assert!(app.auto.find("team.invite.person.leo").is_none());
    h.click(&mut app, "team.invite.person.ana");
    assert_eq!(label(&app, "team.invite.send").as_deref(), Some("Invite 1 Person"));
    h.click(&mut app, "team.invite.send");
    let acts = acts.borrow();
    // typing searches the Drive (it never hands out everyone)
    assert!(acts.contains(&TeamAction::SearchPeople("ruiz".into())));
    let rest: Vec<&TeamAction> = acts[1..].iter().filter(|a| !matches!(a, TeamAction::SearchPeople(_))).collect();
    assert_eq!(rest, [&TeamAction::Invite("p1".into(), vec!["ana".into()]), &TeamAction::DismissInvitePrompt]);
    drop(acts);
    h.frames(&mut app, 2);
    assert!(app.auto.find("team.invite.send").is_none(), "the step closes");
}

#[test]
fn leaving_a_project_asks_first() {
    let view = signed_in(vec![TeamProjectRow { id: "p1".into(), name: "Robotics".into(), members: vec![person("maya", "Maya")], ..Default::default() }]);
    let (mut app, acts, _) = app_with(view);
    let mut h = harness();
    h.frames(&mut app, 3);
    h.click(&mut app, "team.project.p1.leaveTeam");
    assert!(acts.borrow().is_empty(), "nothing happens before the confirmation");
    h.click(&mut app, "team.confirm.cancel");
    h.click(&mut app, "team.project.p1.leaveTeam");
    h.click(&mut app, "team.confirm.leave");
    assert_eq!(*acts.borrow(), vec![TeamAction::LeaveTeam("p1".into())]);
}

#[test]
fn open_project_shows_downloads_proxies_failures_and_connection() {
    let mut view = signed_in(vec![TeamProjectRow { id: "p1".into(), name: "Robotics".into(), media_count: 1, media_bytes: 2_000_000, ..Default::default() }]);
    view.error = Some("The Drive refused: only the creator can change this".into());
    view.open = Some(TeamOpenView {
        id: "p1".into(),
        name: "Robotics".into(),
        feed_live: false,
        link_error: Some("connection reset".into()),
        downloads: vec![("A001.mov".into(), 0.25)],
        proxies: vec![("A000.mov".into(), 0.75)],
        failures: vec![("A002.mov".into(), "disk full".into())],
        uploads: vec![("B001.mov".into(), 0.5), ("B003.mov".into(), 0.6)],
        uploads_done: vec!["B004.mov".into()],
        uploads_resumed: vec!["B003.mov".into()],
        upload_failures: vec![("B002.mov".into(), "file is gone".into())],
        incoming: vec![("C001.mov".into(), "maya".into())],
        exporting: false,
        drive_only: vec![("Day 1/D001.mov".into(), 3_000_000)],
    });
    let (mut app, acts, _) = app_with(view);
    let mut h = harness();
    h.frames(&mut app, 3);
    assert_eq!(label(&app, "team.open").as_deref(), Some("Robotics (Reconnecting…)"));
    assert_eq!(label(&app, "team.download.0").as_deref(), Some("Downloading A001.mov: 25 %"));
    assert_eq!(label(&app, "team.proxy.0").as_deref(), Some("Making a proxy of A000.mov: 75 %"));
    assert_eq!(label(&app, "team.failure.0").as_deref(), Some("A002.mov didn't download: disk full"));
    assert_eq!(label(&app, "team.upload.0").as_deref(), Some("Uploading B001.mov: 50 %"));
    assert_eq!(label(&app, "team.upload.1").as_deref(), Some("Resuming upload of B003.mov: 60 %"));
    assert_eq!(label(&app, "team.upload.done.0").as_deref(), Some("B004.mov uploaded"));
    assert_eq!(label(&app, "team.upload_failure.0").as_deref(), Some("B002.mov didn't upload: file is gone"));
    assert_eq!(label(&app, "team.incoming.0").as_deref(), Some("C001.mov: uploading from maya"));
    assert!(app.auto.find("team.project.p1.open").is_none(), "the open project has no Open button");
    assert!(app.auto.find("team.footage.add").is_some());
    assert_eq!(label(&app, "team.drive_only").as_deref(), Some("1 new file on InFocus Drive · 3 MB"));
    h.click(&mut app, "team.retry");
    h.click(&mut app, "team.error.dismiss");
    h.click(&mut app, "team.drive_only.add");
    assert_eq!(*acts.borrow(), vec![TeamAction::RetryDownloads, TeamAction::ClearError, TeamAction::AddDriveFootage]);
}

/// In a narrow panel the buttons go under the text instead of over it; a little wider, long text
/// wraps beside them. Either way nothing overlaps or sticks out of the panel.
#[test]
fn narrow_panels_keep_text_and_buttons_apart() {
    for width in [200.0, 300.0] {
        let mut view = signed_in(vec![TeamProjectRow {
            id: "p1".into(),
            name: "A Very Long Homecoming Project Name".into(),
            role: TeamRole::Invited,
            invited_by: Some("Ana".into()),
            ..Default::default()
        }]);
        view.user = Some("a-rather-long-user-name".into());
        view.open = Some(TeamOpenView { id: "p2".into(), name: "Spring Musical Rehearsals And Final Cut".into(), feed_live: true, ..Default::default() });
        let (mut app, _, _) = app_with(view);
        let mut h = harness();
        h.width = width;
        h.frames(&mut app, 3);
        let rect = |id: &str| {
            let e = app.auto.find(id).unwrap_or_else(|| panic!("no {id}"));
            Rect::from_min_size(pos2(e.rect[0], e.rect[1]), vec2(e.rect[2], e.rect[3]))
        };
        for (text, button) in [("team.status", "team.signOut"), ("team.open", "team.leave")] {
            assert!(!rect(text).intersects(rect(button)), "{width}: {button} overlaps {text}");
            assert!(rect(button).min.y < rect(text).max.y + 40.0, "{width}: {button} drifted away from {text}: {:?}", rect(button));
        }
        for id in ["team.signOut", "team.leave", "team.invitation.p1.join", "team.invitation.p1.decline", "team.footage.add"] {
            assert!(rect(id).max.x <= width + 0.5, "{width}: {id} sticks out of the panel: {:?}", rect(id));
        }
    }
}

#[test]
fn sizes_read_plainly() {
    assert_eq!(size_text(0), "0 KB");
    assert_eq!(size_text(1), "1 KB");
    assert_eq!(size_text(340_000_000), "340 MB");
    assert_eq!(size_text(1_234_000_000), "1.2 GB");
    assert_eq!(footage_text(1, 5_000_000), "1 file · 5 MB");
}

#[test]
fn without_the_host_hook_there_is_no_team_panel() {
    let app = FilmcraftApp::new(Session::default());
    let team = crate::menus::panel_command_id(crate::dock::PanelKind::Team);
    assert!(crate::menus::menu_items(&app).iter().all(|it| it.id != team));
    let (app, _, _) = app_with(TeamView::default());
    assert!(crate::menus::menu_items(&app).iter().any(|it| it.id == team));
}
