//! [`DriveDownloader`]: `TeamMedia`'s `Downloader` over the Drive client's resumable downloads.

use std::path::Path;
use std::sync::Arc;

use filmcraft_drive::{DriveClient, DriveError, Expected, media_path_in};
use filmcraft_engine::team_media::{DownloadError, Downloader};

/// Fetches originals from a team project's `<name> Media/` folder.
pub struct DriveDownloader {
    client: Arc<DriveClient>,
    project: String,
    folder: String,
}

impl DriveDownloader {
    /// `folder` is the project's Drive folder (`Team Projects/<name>.fcteam`).
    pub fn new(client: Arc<DriveClient>, project: &str, folder: &str) -> Self {
        Self { client, project: project.to_string(), folder: folder.to_string() }
    }
}

impl Downloader for DriveDownloader {
    /// Resumes `<dest>.partial` with a Range request; `progress` returning false (TeamMedia
    /// paused, for an export) stops with [`DownloadError::Paused`] and keeps the partial file.
    fn download(&self, drive_ref: &str, dest: &Path, size: Option<u64>, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<u64, DownloadError> {
        let rel = media_path_in(&self.folder, drive_ref)
            .ok_or_else(|| DownloadError::Failed(format!("{drive_ref} is not in this project's media folder on the Drive")))?;
        let expected = Expected { size, sha256: None };
        self.client.download_media(&self.project, &rel, dest, &expected, progress).map_err(|e| match e {
            DriveError::Cancelled => DownloadError::Paused,
            other => DownloadError::Failed(other.to_string()),
        })
    }
}
