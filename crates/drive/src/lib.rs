//! InFocus Drive client for InFocus Cut team projects.
//!
//! - [`DriveConfig`]: where the Drive is (config file or `INFOCUS_DRIVE_URL`, never hard-coded).
//! - [`login`]: browser sign-in with PKCE against `/cli/authorize` and `/api/cli/token`; the `ifd_`
//!   token goes to a [`TokenStore`] ([`KeychainStore`] in the app, [`MemoryStore`] in tests).
//! - [`DriveClient`]: one blocking method per `/api/team/…` route, the change feed ([`EventStream`]),
//!   resumable media downloads ([`DriveClient::download_media`]) and uploads
//!   ([`DriveClient::upload_media`]), and project membership: people to invite, invites, join,
//!   decline and leave. Lock requests name this Mac (`X-Drive-Device`). It uses the
//!   LAN route when it answers within 300 ms and falls back to the internet route.
//!
//! Network code only: no UI and no engine types, so the engine never depends on this crate.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod auth;
mod client;
mod config;
mod device;
mod download;
mod error;
mod events;
mod http;
mod lan;
mod members;
mod upload;

pub use auth::{
    KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE, KeychainStore, LOGIN_TIMEOUT, LoginOptions, MemoryStore, SignedIn, TOKEN_PREFIX, TokenStore, login, logout,
    pkce_challenge,
};
pub use client::{
    AppInfo, Cleanup, CreatedProject, DriveClient, Fetched, LockGrant, LockState, MAX_ID, ProjectData, ProjectSummary, SequencePart, SharedChange, SharedPart,
};
pub use config::{DRIVE_URL_ENV, DriveConfig};
pub use device::{DEVICE_HEADER, MAX_DEVICE_LEN, local_device_name, sanitize_device_name};
pub use download::{DRIVE_PREFIX, Expected, MEDIA_SUFFIX, media_folder, media_path_in, partial_path, sha256_hex};
pub use error::{DriveError, Result};
pub use events::{EventStream, SseEvent, SseParser, TeamEvent};
pub use lan::{LAN_PROBE_TIMEOUT, LAN_TOKEN_PREFIX};
pub use members::{Invite, MAX_INVITES_PER_REQUEST, Person, Role};
pub use upload::{MAX_CHUNK_SIZE, UploadProgress, UploadSession, UploadedMedia};

#[cfg(test)]
mod members_tests;
/// A tiny in-process HTTP server for tests (here and in crates that drive a `DriveClient`).
#[cfg(any(test, feature = "test-util"))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub mod mock;
#[cfg(test)]
mod tests;
