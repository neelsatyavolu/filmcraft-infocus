//! Signing in, as the member sees it: who is signed in (with their name), Switch Account, the
//! dashboard website's address, and sign-in failures said in plain words (offline, the Drive not
//! answering, an account that isn't an InFocus Drive member).

use filmcraft_drive::{DEFAULT_DASHBOARD_URL, DriveError};
use filmcraft_engine::Session;

use super::{Outcome, Phase, TeamSessionController};

/// This Mac has no network (or no name lookup).
pub const OFFLINE: &str = "This Mac looks offline. Connect to Wi-Fi or the school network, then try again.";
/// The Drive didn't answer.
pub const UNREACHABLE: &str = "Can't reach InFocus Drive right now. Check your connection, or try again in a minute.";
/// The Drive refused the account.
pub const NOT_A_MEMBER: &str = "This account isn't an InFocus Drive member. Sign in with your school account, or ask an InFocus admin to add you.";
/// Cancelled in the browser.
pub const DENIED: &str = "Sign-in was cancelled in the browser.";
/// The browser never came back.
pub const TIMED_OUT: &str =
    "Sign-in didn't finish in the browser. Try again, and if the Drive said your account isn't a member, ask an InFocus admin to add you.";

impl TeamSessionController {
    /// The signed-in person's name (None when the Drive has only the user name).
    pub fn user_name(&self) -> Option<&str> {
        self.me_name.as_deref()
    }

    /// The team project dashboard website (config `dashboard_url`, `INFOCUS_EDITING_URL`, or
    /// the default).
    pub fn dashboard_url(&self) -> String {
        self.cfg.drive.as_ref().map_or_else(|_| DEFAULT_DASHBOARD_URL.to_string(), |d| d.dashboard_url.clone())
    }

    /// Opens the dashboard website in the browser.
    pub fn open_dashboard(&self) {
        (self.cfg.open_browser)(&self.dashboard_url());
    }

    /// Signs out (as [`Self::sign_out`]), then starts signing in again with another account.
    pub fn switch_account(&mut self, s: &mut Session) {
        self.sign_out(s);
        self.sign_in_after_sign_out = true;
    }

    /// Signing out finished: carry on with Switch Account.
    pub(super) fn signed_out_now(&mut self) {
        if std::mem::take(&mut self.sign_in_after_sign_out) && self.phase == Phase::SignedOut {
            self.start_sign_in(true);
        }
    }
}

/// A finished sign-in: a connection failure there is said in plain words too.
pub(super) fn signed_in(out: Outcome) -> Outcome {
    match out {
        Outcome::Failed { message, signed_out } => Outcome::Failed { message: plain(&message).unwrap_or(message), signed_out },
        other => other,
    }
}

/// A failed browser sign-in, in plain words.
pub(super) fn sign_in_failed(e: DriveError) -> Outcome {
    let message = match &e {
        DriveError::Login(why) if why.contains("denied") => DENIED.to_string(),
        DriveError::Login(why) if why.contains("timed out") => TIMED_OUT.to_string(),
        DriveError::Forbidden(_) => NOT_A_MEMBER.to_string(),
        DriveError::Login(why) | DriveError::Network(why) => offline_or_unreachable(why).to_string(),
        other => plain(&other.to_string()).unwrap_or_else(|| other.to_string()),
    };
    Outcome::Failed { message, signed_out: true }
}

/// The plain-words version of a connection error message, when there is one.
fn plain(message: &str) -> Option<String> {
    if let Some(why) = message.strip_prefix("Can't reach InFocus Drive: ") {
        return Some(offline_or_unreachable(why).to_string());
    }
    if message.starts_with("You don't have access") {
        return Some(NOT_A_MEMBER.to_string());
    }
    None
}

/// No name lookup or no route means this Mac is offline; anything else, the Drive didn't answer.
pub(super) fn offline_or_unreachable(why: &str) -> &'static str {
    let why = why.to_ascii_lowercase();
    let offline = ["host not found", "resolve", "dns", "lookup", "network is unreachable", "no route to host", "not connected"];
    if offline.iter().any(|w| why.contains(w)) { OFFLINE } else { UNREACHABLE }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(out: Outcome) -> String {
        match out {
            Outcome::Failed { message, .. } => message,
            _ => panic!("not a failure"),
        }
    }

    #[test]
    fn sign_in_failures_are_said_plainly() {
        assert_eq!(message(sign_in_failed(DriveError::Network("host not found".into()))), OFFLINE);
        assert_eq!(message(sign_in_failed(DriveError::Network("connection refused".into()))), UNREACHABLE);
        assert_eq!(message(sign_in_failed(DriveError::Forbidden("no".into()))), NOT_A_MEMBER);
        assert_eq!(message(sign_in_failed(DriveError::Login("access was denied in the browser".into()))), DENIED);
        assert_eq!(message(sign_in_failed(DriveError::Login("timed out waiting for approval in the browser".into()))), TIMED_OUT);
        let connect = Outcome::Failed { message: DriveError::Network("dns error: lookup failed".into()).to_string(), signed_out: false };
        assert_eq!(message(signed_in(connect)), OFFLINE);
        let other = Outcome::Failed { message: "Update InFocus Cut".into(), signed_out: false };
        assert_eq!(message(signed_in(other)), "Update InFocus Cut");
    }
}
