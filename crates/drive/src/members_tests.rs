//! People, invites, join, decline and leave against the mock Drive.

use serde_json::json;

use crate::mock::{MockServer, Reply};
use crate::*;

const PID: &str = "0123456789abcdef0123456789abcdef";

fn client(server: &MockServer) -> DriveClient {
    let app = AppInfo::new("0.20.1", 17).unwrap().with_device("Studio Mac");
    DriveClient::connect(&DriveConfig::new(&server.url, None).unwrap(), "ifd_test", app).unwrap()
}

#[test]
fn list_reads_members_invites_and_footage() {
    let server = MockServer::start(|_| {
        Reply::json(
            200,
            json!([
                {"id": PID, "name": "A", "folder": "Team Projects/A.fcteam", "role": "member",
                 "members": [{"username": "student1", "name": "Student One"}, {"username": "student2", "name": ""}],
                 "invites": [{"username": "student3", "name": "Student Three", "by": "student1"}],
                 "invited_by": null, "media_count": 3, "media_bytes": 1234},
                {"id": PID, "name": "B", "folder": "Team Projects/B.fcteam", "role": "invited", "invited_by": "Student One"},
                {"id": PID, "name": "C", "folder": "Team Projects/C.fcteam", "role": "somethingnew"},
                // an older Drive: no member fields at all
                {"id": PID, "name": "D", "folder": "Team Projects/D.fcteam"},
            ]),
        )
    });
    let list = client(&server).list_projects().unwrap();
    assert_eq!(list[0].role, Role::Member);
    assert_eq!(list[0].members.len(), 2);
    assert_eq!(list[0].members[1].shown(), "student2", "no display name: the user name");
    assert_eq!(list[0].invites[0], Invite { username: "student3".into(), name: "Student Three".into(), by: "student1".into() });
    assert_eq!((list[0].media_count, list[0].media_bytes), (3, 1234));
    assert_eq!(list[1].role, Role::Invited);
    assert_eq!(list[1].invited_by.as_deref(), Some("Student One"));
    assert_eq!(list[2].role, Role::Open, "an unknown role reads as open");
    assert_eq!(list[3].role, Role::Open);
    assert!(list[3].members.is_empty() && list[3].invites.is_empty());
}

#[test]
fn people_invite_cancel_join_decline_leave() {
    let server = MockServer::start(|req| match (req.method.as_str(), req.path()) {
        ("GET", "/api/team/people") => {
            Reply::json(200, json!({"people": [{"username": "student2", "name": "Student Two"}, {"username": "student3"}, {"username": ""}]}))
        }
        ("POST", p) if p.ends_with("/invites") => Reply::json(200, json!({"invites": [{"user": "student2", "by": "student1", "at": 5}]})),
        ("DELETE", p) if p.contains("/invites/") => Reply::Full(204, Vec::new(), Vec::new()),
        ("POST", p) if p.ends_with("/join") => Reply::json(200, json!({"id": PID, "slot": 4})),
        ("POST", p) if p.ends_with("/decline") || p.ends_with("/leave") => Reply::Full(204, Vec::new(), Vec::new()),
        _ => Reply::json(404, json!({"detail": "x"})),
    });
    let c = client(&server);
    let people = c.people().unwrap();
    assert_eq!(people.len(), 2, "nameless entries are dropped");
    assert_eq!(people[1].shown(), "student3");
    let invites = c.invite(PID, &["student2".into()]).unwrap();
    assert_eq!(invites[0].username, "student2", "the stored `user` field reads as the user name");
    c.cancel_invite(PID, "student 2").unwrap();
    assert_eq!(c.join(PID).unwrap(), CreatedProject { id: PID.into(), slot: 4 });
    c.decline(PID).unwrap();
    c.leave_project(PID).unwrap();
    let reqs = server.requests();
    assert_eq!(reqs[1].json(), json!({"users": ["student2"]}));
    assert_eq!(reqs[2].path(), format!("/api/team/projects/{PID}/invites/student%202"));
    assert_eq!(reqs[3].header(DEVICE_HEADER), Some("Studio Mac"), "joining gives this Mac an id slot");
    assert_eq!(reqs[4].path(), format!("/api/team/projects/{PID}/decline"));
    assert_eq!(reqs[5].path(), format!("/api/team/projects/{PID}/leave"));
}

#[test]
fn invite_limits_never_reach_the_network() {
    let server = MockServer::start(|_| Reply::json(500, json!({})));
    let c = client(&server);
    assert!(matches!(c.invite(PID, &[]), Err(DriveError::Invalid(_))));
    let many: Vec<String> = (0..=MAX_INVITES_PER_REQUEST).map(|i| format!("student{i}")).collect();
    assert!(matches!(c.invite(PID, &many), Err(DriveError::Invalid(_))));
    assert!(matches!(c.invite(PID, &["../x".into()]), Err(DriveError::Invalid(_))));
    assert!(matches!(c.cancel_invite(PID, ".."), Err(DriveError::Invalid(_))));
    assert!(matches!(c.join("../etc"), Err(DriveError::Invalid(_))));
    assert!(server.requests().is_empty());
}

#[test]
fn full_team_and_last_member_are_their_own_errors() {
    let server = MockServer::start(|req| match req.path() {
        p if p.ends_with("/invites") => Reply::json(409, json!({"error": "team_full", "message": "This project already has 12 people."})),
        _ => Reply::json(409, json!({"error": "last_member", "message": "You're the last member."})),
    });
    let c = client(&server);
    let e = c.invite(PID, &["student2".into()]).unwrap_err();
    assert!(matches!(&e, DriveError::TeamFull(m) if m == "This project already has 12 people."), "{e:?}");
    assert_eq!(e.to_string(), "This project already has 12 people.");
    assert!(matches!(c.leave_project(PID), Err(DriveError::LastMember(_))));
}

#[test]
fn presence_reads_online_and_last_seen_from_the_overview() {
    let server = MockServer::start(|req| {
        assert_eq!(req.path(), format!("/api/team/projects/{PID}/overview"));
        Reply::json(
            200,
            json!({"id": PID, "members": [
                {"username": "student1", "name": "Student One", "online": true, "last_seen": 1700000000.5, "editing": [3]},
                {"username": "student2", "name": "Student Two", "online": false, "last_seen": 0.0},
            ]}),
        )
    });
    let people = client(&server).presence(PID).unwrap();
    assert_eq!(people.len(), 2);
    assert_eq!((people[0].online, people[0].last_seen), (Some(true), Some(1700000000.5)));
    assert_eq!(people[1].online, Some(false));
}
