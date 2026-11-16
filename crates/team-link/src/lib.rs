//! InFocus Cut: connects a team project session to InFocus Drive.
//!
//! - [`DriveTeamLink`]: the engine's [`TeamLink`](filmcraft_engine::team::TeamLink) over a
//!   [`DriveClient`](filmcraft_drive::DriveClient). Requests run in order on a worker thread; the
//!   change feed runs on its own thread, reconnects with backoff when the stream ends, and polls
//!   the project every 5 s while it is down. Nothing here blocks the UI thread.
//! - [`DriveDownloader`]: the engine's [`Downloader`](filmcraft_engine::team_media::Downloader)
//!   over `download_media`, so `TeamMedia` can fetch originals (and pause them).
//! - [`TeamSessionController`]: what the UI calls: sign in and out, list, create, open and keep
//!   projects, and the export guard. Its network work runs on background threads; the UI calls
//!   [`TeamSessionController::poll`] once per frame.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod controller;
mod downloader;
mod feed;
mod link;

pub use controller::{ControllerConfig, OpenTeamProject, Phase, TeamSessionController, default_cache_dir, open_in_browser};
pub use downloader::DriveDownloader;
pub use feed::FeedState;
pub use link::{DriveTeamLink, LinkStatus, link_error};

#[cfg(test)]
mod controller_tests;
#[cfg(test)]
mod tests;
