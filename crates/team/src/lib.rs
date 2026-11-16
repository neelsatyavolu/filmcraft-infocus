//! Team projects for InFocus Cut: pure logic, no network and no UI.
//!
//! A team project is stored as one shared part (settings, bins, media items, transcripts…) and one
//! part per sequence, so each sequence can be locked and saved on its own ([`split`], [`merge`],
//! [`sequence_json`], [`replace_sequence`]). Local file paths and proxies never leave the Mac.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

mod doc;

pub use doc::{DRIVE_PREFIX, LocalMedia, TeamDoc, TeamError, merge, replace_sequence, sequence_json, split};

#[cfg(test)]
mod tests;
