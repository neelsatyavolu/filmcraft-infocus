//! Errors from the Drive client. Each server refusal that the app must react to has its own variant.

use thiserror::Error;

/// Everything that can go wrong talking to InFocus Drive.
#[derive(Debug, Error)]
pub enum DriveError {
    /// The configuration file or environment is missing or invalid.
    #[error("InFocus Drive settings: {0}")]
    Config(String),
    /// No token is stored, or the server no longer accepts it (401). Sign in again.
    #[error("Sign in to InFocus Drive again ({0})")]
    Unauthorized(String),
    /// The member cannot open this project or folder (403).
    #[error("You don't have access: {0}")]
    Forbidden(String),
    /// The project, sequence or file does not exist (404).
    #[error("Not found: {0}")]
    NotFound(String),
    /// Someone else holds the sequence lock (409 on take). `device` is set only when the holder is
    /// this member on another Mac (the server keeps a teammate's device name private).
    #[error("{}", held_by_message(holder.as_deref(), device.as_deref()))]
    HeldBy { holder: Option<String>, expires_at: f64, device: Option<String> },
    /// The lock token is no longer current: it expired or an admin released it (409).
    #[error("The lock on this sequence was lost")]
    LockLost,
    /// The base revision is out of date (409); `rev` is the server's current revision.
    #[error("Someone saved a newer version (revision {rev})")]
    Stale { rev: u64 },
    /// A shared item cannot be deleted while a sequence uses it (409).
    #[error("Item {item} is used by a sequence and can't be deleted")]
    ItemInUse { item: u64 },
    /// The project's files were changed outside InFocus Cut, so the server refuses them (409).
    #[error("{0}")]
    FilesChanged(String),
    /// Another team project already has this name (409).
    #[error("{0}")]
    NameTaken(String),
    /// The project's metadata on the Drive is damaged (409).
    #[error("{0}")]
    InvalidProject(String),
    /// Any other team conflict (409 `conflict`), with the server's message.
    #[error("{0}")]
    Conflict(String),
    /// The project has no id slot left for another member or Mac (409 `no_slots`).
    #[error("{0}")]
    NoSlots(String),
    /// The member has too many change feeds open (429 `too_many_streams`); polling covers it.
    #[error("{0}")]
    TooManyStreams(String),
    /// The Drive doesn't know this app's project schema yet (400 `schema_unsupported`).
    #[error("This InFocus Drive is older than InFocus Cut (it knows project schema {max_schema_version}): ask for the Drive to be updated")]
    SchemaUnsupported { max_schema_version: u64 },
    /// The request is over one of the Drive's limits (413).
    #[error("Too much for InFocus Drive at once: {0}")]
    TooLarge(String),
    /// This app is older than the project requires (426). Update InFocus Cut.
    #[error("Update InFocus Cut to edit this project (needs {min_app_version} or later)")]
    UpdateRequired { min_app_version: String, schema_version: u64 },
    /// Any other HTTP error status.
    #[error("InFocus Drive answered {status}: {message}")]
    Http { status: u16, message: String },
    /// The server could not be reached or the connection broke.
    #[error("Can't reach InFocus Drive: {0}")]
    Network(String),
    /// The server's answer was not what this client expects.
    #[error("Unexpected answer from InFocus Drive: {0}")]
    BadResponse(String),
    /// A local file operation failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// A downloaded file failed its size or hash check; nothing was kept.
    #[error("Download check failed: {0}")]
    Verify(String),
    /// The caller stopped a download; the partial file is kept for resuming.
    #[error("Download paused")]
    Cancelled,
    /// Browser sign-in failed, was denied or timed out.
    #[error("Sign-in: {0}")]
    Login(String),
    /// Reading or writing the stored token failed.
    #[error("Keychain: {0}")]
    Keychain(String),
    /// A caller-supplied value (project id, sequence id, version) is invalid.
    #[error("Invalid value: {0}")]
    Invalid(String),
}

impl DriveError {
    /// True for errors that mean the connection itself failed (so another route may work).
    pub fn is_network(&self) -> bool {
        matches!(self, DriveError::Network(_))
    }
}

fn held_by_message(holder: Option<&str>, device: Option<&str>) -> String {
    match device {
        Some(device) => format!("You are editing this sequence on {device}"),
        None => format!("{} is editing this sequence", holder.unwrap_or("Someone")),
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, DriveError>;
