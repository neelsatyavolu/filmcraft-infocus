//! The controller's membership side: people to invite, invites, joining, declining and leaving a
//! team project, the "invite your teammates" step after creating one, adding footage to the open
//! project, and fetching the project list again now and then.

use std::path::Path;

use filmcraft_drive::{MAX_INVITES_PER_REQUEST, Person};
use filmcraft_engine::Session;
use serde_json::json;

use super::{LIST_REFRESH, Outcome, Phase, TeamSessionController, failed};

/// Footage files one "Add footage…" may bring in.
pub const MAX_FOOTAGE_FILES: usize = 200;

impl TeamSessionController {
    /// Drive members who can be invited (not including this member).
    pub fn people(&self) -> &[Person] {
        &self.people
    }

    /// Searches the Drive's members for `query` (the invite picker's search field); fewer than
    /// 2 characters just clears [`Self::people`]. An answer to an older search is dropped.
    pub fn search_people(&mut self, query: &str) {
        let query = query.trim().to_string();
        if query == self.people_query {
            return;
        }
        self.people_query.clone_from(&query);
        self.people.clear();
        if query.chars().count() < filmcraft_drive::DriveClient::PEOPLE_MIN_QUERY {
            return;
        }
        let Some(client) = self.client.clone() else { return };
        // an older Drive has no search: the picker then says no one matches
        self.spawn_quiet("Looking up people", move || {
            let found = client.people(&query).unwrap_or_else(|e| {
                log::info!("no people to invite: {e}");
                Vec::new()
            });
            Outcome::People(query, found)
        });
    }

    /// The project just created on this Mac, while the panel offers to invite teammates to it.
    pub fn invite_prompt(&self) -> Option<&str> {
        self.invite_prompt.as_deref()
    }

    /// Done with (or skipped) inviting teammates to the new project.
    pub fn dismiss_invite_prompt(&mut self) {
        self.invite_prompt = None;
    }

    /// Invites `users` (Drive user names) to project `id`; in batches the Drive accepts.
    pub fn invite(&mut self, id: &str, users: &[String]) {
        let users: Vec<String> = users.iter().filter(|u| !u.trim().is_empty()).cloned().collect();
        if users.is_empty() {
            return;
        }
        let Some(client) = self.signed_in_client() else { return };
        let id = id.to_string();
        self.spawn("Sending invitations", move || {
            for batch in users.chunks(MAX_INVITES_PER_REQUEST) {
                if let Err(e) = client.invite(&id, batch) {
                    return failed(e);
                }
            }
            Outcome::ListStale
        });
    }

    /// Takes back the invite of `user` to project `id`.
    pub fn cancel_invite(&mut self, id: &str, user: &str) {
        let Some(client) = self.signed_in_client() else { return };
        let (id, user) = (id.to_string(), user.to_string());
        self.spawn("Cancelling the invitation", move || client.cancel_invite(&id, &user).map_or_else(failed, |()| Outcome::ListStale));
    }

    /// Accepts the invitation to project `id`, then opens it.
    pub fn join(&mut self, id: &str) {
        let Some(client) = self.signed_in_client() else { return };
        let id = id.to_string();
        self.spawn("Joining the project", move || client.join(&id).map_or_else(failed, |joined| Outcome::Joined(joined.id)));
    }

    /// Turns down the invitation to project `id`.
    pub fn decline(&mut self, id: &str) {
        let Some(client) = self.signed_in_client() else { return };
        let id = id.to_string();
        self.spawn("Declining the invitation", move || client.decline(&id).map_or_else(failed, |()| Outcome::ListStale));
    }

    /// Leaves team project `id` for good (it no longer shows here; a teammate can invite this
    /// member again). When it is open here it is closed first: edits not on the Drive yet stay
    /// in this Mac's copy only.
    pub fn leave_team(&mut self, s: &mut Session, id: &str) {
        let Some(client) = self.signed_in_client() else { return };
        if self.open.as_ref().is_some_and(|o| o.info.id == id) {
            s.team_flush();
            self.close(s);
        }
        if self.invite_prompt.as_deref() == Some(id) {
            self.invite_prompt = None;
        }
        let id = id.to_string();
        self.spawn("Leaving the project", move || client.leave_project(&id).map_or_else(failed, |()| Outcome::ListStale));
    }

    /// "Add footage…": imports `paths` into the open team project. Each file then uploads to
    /// the project's media folder ([`crate::TeamUploads`]) so teammates can download it.
    /// Returns how many files were imported.
    pub fn add_footage(&mut self, s: &mut Session, paths: &[String]) -> Result<usize, String> {
        if self.open.is_none() || s.team.is_none() {
            return Err("Open a team project first, then add footage to it".into());
        }
        let paths: Vec<&String> = paths.iter().filter(|p| !p.trim().is_empty()).take(MAX_FOOTAGE_FILES).collect();
        if paths.is_empty() {
            return Ok(0);
        }
        for p in &paths {
            if !Path::new(p.as_str()).is_file() {
                return Err(format!("“{}” isn't a file on this Mac", file_label(p)));
            }
        }
        let before = s.project.items.len();
        s.execute("file.import", json!({ "paths": paths })).map_err(|e| format!("Could not add the footage: {e}"))?;
        Ok(s.project.items.len().saturating_sub(before))
    }

    /// Fetches the project list again every [`LIST_REFRESH`] while signed in.
    pub(super) fn refresh_now_and_then(&mut self) {
        if self.phase != Phase::SignedIn || !self.jobs.is_empty() {
            return;
        }
        if self.listed_at.is_none_or(|at| at.elapsed() >= LIST_REFRESH)
            && let Some(client) = self.client.clone()
        {
            self.listed_at = Some(std::time::Instant::now());
            self.spawn_quiet("Loading team projects", move || client.list_projects().map_or_else(failed, Outcome::Projects));
        }
    }

    #[cfg(test)]
    pub(crate) fn age_list(&mut self) {
        self.listed_at = std::time::Instant::now().checked_sub(LIST_REFRESH);
    }
}

fn file_label(path: &str) -> String {
    Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}
