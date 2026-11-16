//! Who is in a team project: the people a member can invite (`GET /api/team/people`), invites,
//! joining, declining and leaving (`/api/team/projects/{id}/invites|join|decline|leave`).
//!
//! A project whose `meta.json` has no member list is an older project open to every Drive member
//! ([`Role::Open`]); the others are used only by their members.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{CreatedProject, DriveClient, Method, project_path};
use crate::error::{DriveError, Result};
use crate::http;

/// Most names one invite request may carry (the Drive refuses more).
pub const MAX_INVITES_PER_REQUEST: usize = 10;
/// Longest user name sent in a URL.
const MAX_USERNAME_LEN: usize = 128;

/// The signed-in member's place in a project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Member,
    /// Invited, not joined yet: the project shows as an invitation.
    Invited,
    /// An older project without a member list: every Drive member may use it (also what an
    /// older Drive that doesn't send roles means).
    #[default]
    #[serde(other)]
    Open,
}

/// A Drive member (`GET /api/team/people`, a project's members).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Person {
    pub username: String,
    /// Display name (the user name when the Drive has none).
    #[serde(default)]
    pub name: String,
}

impl Person {
    /// The name to show: the display name, or the user name without one.
    pub fn shown(&self) -> &str {
        if self.name.trim().is_empty() { &self.username } else { &self.name }
    }
}

/// A pending invite. The Drive stores `user`; lists send `username`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Invite {
    #[serde(alias = "user")]
    pub username: String,
    #[serde(default)]
    pub name: String,
    /// Who sent it (user name or display name, as the Drive gives it).
    #[serde(default)]
    pub by: String,
}

impl Invite {
    /// The name to show: the display name, or the user name without one.
    pub fn shown(&self) -> &str {
        if self.name.trim().is_empty() { &self.username } else { &self.name }
    }
}

impl DriveClient {
    /// Drive members this member can invite (not including this member).
    pub fn people(&self) -> Result<Vec<Person>> {
        #[derive(Deserialize)]
        struct People {
            #[serde(default)]
            people: Vec<Person>,
        }
        let list: People = http::parse(&self.call(Method::Get, "/api/team/people", None)?)?;
        Ok(list.people.into_iter().filter(|p| !p.username.is_empty()).collect())
    }

    /// Invites `users` (1 to [`MAX_INVITES_PER_REQUEST`] user names) to a project this member is
    /// in. Returns the project's invites. [`DriveError::TeamFull`] past the Drive's team size.
    pub fn invite(&self, project: &str, users: &[String]) -> Result<Vec<Invite>> {
        if users.is_empty() || users.len() > MAX_INVITES_PER_REQUEST {
            return Err(DriveError::Invalid(format!("invite 1 to {MAX_INVITES_PER_REQUEST} people at a time")));
        }
        for u in users {
            username(u)?;
        }
        #[derive(Deserialize)]
        struct Invites {
            #[serde(default)]
            invites: Vec<Invite>,
        }
        let path = format!("{}/invites", project_path(project)?);
        let answer: Invites = http::parse(&self.call(Method::Post, &path, Some(&json!({ "users": users })))?)?;
        Ok(answer.invites)
    }

    /// Takes back the invite of `user` (any member may).
    pub fn cancel_invite(&self, project: &str, user: &str) -> Result<()> {
        let path = format!("{}/invites/{}", project_path(project)?, encode_segment(username(user)?));
        self.call(Method::Delete, &path, None)?;
        Ok(())
    }

    /// Accepts an invite: this member becomes a member (with an id slot for this Mac).
    pub fn join(&self, project: &str) -> Result<CreatedProject> {
        let body: Value = json!({});
        http::parse(&self.device_call(Method::Post, &format!("{}/join", project_path(project)?), Some(&body))?)
    }

    /// Turns an invite down.
    pub fn decline(&self, project: &str) -> Result<()> {
        self.call(Method::Post, &format!("{}/decline", project_path(project)?), None)?;
        Ok(())
    }

    /// Leaves a project (its locks held by this member are released). [`DriveError::LastMember`]
    /// for the last member.
    pub fn leave_project(&self, project: &str) -> Result<()> {
        self.call(Method::Post, &format!("{}/leave", project_path(project)?), None)?;
        Ok(())
    }
}

/// A user name that is safe to send: not empty, bounded, no control characters.
fn username(u: &str) -> Result<&str> {
    if u.is_empty() || u == "." || u == ".." || u.len() > MAX_USERNAME_LEN || u.chars().any(char::is_control) || u.contains('/') {
        return Err(DriveError::Invalid(format!("user name {u:?}")));
    }
    Ok(u)
}

/// Percent-encodes one URL path segment (everything but unreserved ASCII).
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(encode_segment("student1"), "student1");
        assert_eq!(encode_segment("a b?#é"), "a%20b%3F%23%C3%A9");
        assert_eq!(encode_segment(".."), "..");
    }

    #[test]
    fn hostile_user_names_are_refused() {
        assert!(username("").is_err());
        assert!(username("a/b").is_err());
        assert!(username("..").is_err());
        assert!(username("a\nb").is_err());
        assert!(username(&"x".repeat(MAX_USERNAME_LEN + 1)).is_err());
        assert!(username("student1").is_ok());
    }
}
