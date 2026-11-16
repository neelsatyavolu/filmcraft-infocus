//! InFocus Drive client for InFocus Cut team projects.
//!
//! - [`DriveConfig`]: where the Drive is (config file or `INFOCUS_DRIVE_URL`, never hard-coded).
//! - [`login`]: browser sign-in with PKCE against `/cli/authorize` and `/api/cli/token`; the `ifd_`
//!   token goes to a [`TokenStore`] ([`KeychainStore`] in the app, [`MemoryStore`] in tests).
//! - [`DriveClient`]: one blocking method per `/api/team/…` route, the change feed ([`EventStream`])
//!   and resumable downloads ([`DriveClient::download`]). It uses the LAN route when it answers
//!   within 300 ms and falls back to the internet route.
//!
//! Network code only: no UI and no engine types, so the engine never depends on this crate.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod auth;
mod client;
mod config;
mod download;
mod error;
mod events;
mod http;
mod lan;

pub use auth::{
    KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE, KeychainStore, LOGIN_TIMEOUT, LoginOptions, MemoryStore, SignedIn, TOKEN_PREFIX, TokenStore, login, logout,
    pkce_challenge,
};
pub use client::{AppInfo, CreatedProject, DriveClient, LockGrant, LockState, MAX_ID, ProjectData, ProjectSummary, SequencePart, SharedChange};
pub use config::{DRIVE_URL_ENV, DriveConfig};
pub use download::{DRIVE_PREFIX, Expected, partial_path, sha256_hex};
pub use error::{DriveError, Result};
pub use events::{EventStream, SseEvent, SseParser, TeamEvent};
pub use lan::{LAN_PROBE_TIMEOUT, LAN_TOKEN_PREFIX};

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
