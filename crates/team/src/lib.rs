//! Team projects for InFocus Cut: pure logic, no network and no UI.
//!
//! A team project is stored as one shared part (settings, bins, media items, transcripts…) and one
//! part per sequence, so each sequence can be locked and saved on its own ([`split`], [`merge`],
//! [`sequence_json`], [`replace_sequence`]). Local file paths and proxies never leave the Mac.
//! [`classify`] tells which of those parts an edit changed, and [`changed_fields`] which
//! project-level fields ([`SHARED_FIELDS`]). [`cache`] holds the rules for this
//! Mac's media cache.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod cache;
mod changes;
mod doc;
mod fields;

pub use changes::{ChangeSet, classify};
pub use doc::{DRIVE_PREFIX, LocalMedia, TeamDoc, TeamError, merge, replace_sequence, sequence_json, split};
pub use fields::{SHARED_FIELDS, changed_fields, copy_fields, field_values};

#[cfg(test)]
mod cache_tests;
#[cfg(test)]
mod fields_tests;
#[cfg(test)]
mod tests;
