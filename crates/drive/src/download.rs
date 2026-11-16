//! Resumable media downloads. Bytes go to `<dest>.partial`; only a complete file that passes its
//! size (and, when known, SHA-256) check is renamed to `dest`. A cut connection leaves the partial
//! file, and the next call resumes from its length with an HTTP `Range` request.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::client::{DriveClient, project_path};
use crate::error::{DriveError, Result};
use crate::http;

/// Prefix of a team media reference (`drive:/Team Projects/…`); the rest is the Drive path.
pub const DRIVE_PREFIX: &str = "drive:/";
const COPY_BUF: usize = 256 * 1024;

/// What the finished file must match. The size the server reports is always checked too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    pub size: Option<u64>,
    /// Lowercase hex SHA-256 of the whole file.
    pub sha256: Option<String>,
}

/// `<dest>.partial`, where an unfinished download lives.
pub fn partial_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".partial")
}

/// Holds the server's validator (ETag) of the partial bytes, so a changed file restarts from zero.
fn validator_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".partial.etag")
}

fn with_suffix(dest: &Path, suffix: &str) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    dest.with_file_name(name)
}

/// Suffix of a project's media folder: `Team Projects/<name>.fcteam` keeps its originals in
/// `Team Projects/<name> Media/`.
pub const MEDIA_SUFFIX: &str = " Media";

/// The project's media folder (`Team Projects/<name> Media`) for its folder
/// (`Team Projects/<name>.fcteam`), or None when `folder` is not a `.fcteam` folder.
pub fn media_folder(folder: &str) -> Option<String> {
    let stem = folder.trim_end_matches('/').strip_suffix(".fcteam")?;
    (!stem.is_empty() && !stem.ends_with('/')).then(|| format!("{stem}{MEDIA_SUFFIX}"))
}

/// The path inside the project's media folder of a team media reference
/// (`drive:/Team Projects/<name> Media/Day 2/A001.mov` → `Day 2/A001.mov`), the form
/// [`DriveClient::download_media`] takes. None when the reference is outside that folder or invalid.
pub fn media_path_in(folder: &str, drive_ref: &str) -> Option<String> {
    let media = media_folder(folder)?;
    let full = drive_ref.strip_prefix(DRIVE_PREFIX).unwrap_or(drive_ref).trim_start_matches('/');
    let rel = full.strip_prefix(media.as_str())?.strip_prefix('/')?;
    valid_media_path(rel).ok().map(str::to_string)
}

/// A path inside the media folder: `/`-separated, no empty, `.` or `..` parts, no backslash or
/// control characters (the server refuses the same).
fn valid_media_path(path: &str) -> Result<&str> {
    let bad = path.is_empty() || path.split('/').any(|p| p.is_empty() || p == "." || p == "..") || path.chars().any(|c| c == '\\' || c.is_control());
    if bad { Err(DriveError::Invalid(format!("media path {path:?}"))) } else { Ok(path) }
}

/// Where a response's bytes start and how long the whole file is.
struct Span {
    start: u64,
    total: u64,
}

impl DriveClient {
    /// Downloads an original from the project's media folder (`media_path` is the path inside
    /// `<name> Media/`, see [`media_path_in`]) to `dest`, resuming `<dest>.partial` if it exists.
    /// Bytes go to `<dest>.partial`, which is renamed to `dest` only once complete and checked; the
    /// caller must not rename it again. `on_progress(done, total)` runs as bytes arrive; returning
    /// `false` stops with [`DriveError::Cancelled`] and keeps the partial file. Returns the size.
    pub fn download_media(
        &self,
        project: &str,
        media_path: &str,
        dest: &Path,
        expected: &Expected,
        on_progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<u64> {
        let rel = valid_media_path(media_path)?;
        let route = format!("{}/media", project_path(project)?);
        let partial = partial_path(dest);
        let mut file = OpenOptions::new().create(true).read(true).append(true).open(&partial)?;
        let mut have = file.metadata()?.len();
        if expected.size.is_some_and(|size| have > size) {
            have = restart(&file, dest)?;
        }
        if have > 0 && expected.size == Some(have) {
            return finish(file, dest, have, expected);
        }
        let query = url::form_urlencoded::Serializer::new(String::new()).append_pair("path", rel).finish();
        let mut headers = Vec::new();
        if have > 0 {
            headers.push(("Range", format!("bytes={have}-")));
            if let Ok(tag) = std::fs::read_to_string(validator_path(dest)) {
                headers.push(("If-Range", tag.trim().to_string()));
            }
        }
        let mut resp = self.open_stream(&format!("{route}?{query}"), &headers)?;
        let status = resp.status().as_u16();
        let span = match status {
            206 if have > 0 => content_range(&resp)?,
            200 => {
                // Full body: the server ignored the range or the file changed since the partial began.
                have = restart(&file, dest)?;
                Span { start: 0, total: content_length(&resp)? }
            }
            416 if have > 0 => {
                // Nothing after `have`: complete if the server's size matches, else start over next time.
                let total = unsatisfied_total(&resp);
                if total == Some(have) {
                    return finish(file, dest, have, expected);
                }
                restart(&file, dest)?;
                return Err(DriveError::Verify("the file on the Drive changed; downloading again".into()));
            }
            _ => {
                let (_, bytes) = http::read_body(resp)?;
                return Err(http::error_for(status, &bytes));
            }
        };
        if span.start != have || span.total < have {
            restart(&file, dest)?;
            return Err(DriveError::BadResponse(format!("range starts at {} (asked for {have})", span.start)));
        }
        if expected.size.is_some_and(|size| size != span.total) {
            restart(&file, dest)?;
            return Err(DriveError::Verify(format!("the Drive has {} bytes, expected {}", span.total, expected.size.unwrap_or(0))));
        }
        if let Some(tag) = resp.headers().get("etag").and_then(|v| v.to_str().ok()) {
            std::fs::write(validator_path(dest), tag)?;
        }
        let mut reader = resp.body_mut().with_config().limit(span.total.saturating_sub(have)).reader();
        let mut buf = vec![0u8; COPY_BUF];
        let mut done = have;
        on_progress(done, span.total);
        while done < span.total {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    file.flush()?;
                    return Err(DriveError::Network(e.to_string()));
                }
            };
            file.write_all(buf.get(..n).unwrap_or_default())?;
            done = done.saturating_add(n as u64);
            if !on_progress(done, span.total) {
                file.flush()?;
                return Err(DriveError::Cancelled);
            }
        }
        if done != span.total {
            file.flush()?;
            return Err(DriveError::Network(format!("connection closed after {done} of {} bytes", span.total)));
        }
        finish(file, dest, done, expected)
    }
}

/// Empties the partial file (and forgets its validator). Returns 0.
fn restart(file: &File, dest: &Path) -> Result<u64> {
    file.set_len(0)?;
    let _ = std::fs::remove_file(validator_path(dest));
    Ok(0)
}

/// Checks the complete partial file and renames it to `dest`. A failed check deletes the bytes, so
/// a bad file can never become an original.
fn finish(mut file: File, dest: &Path, total: u64, expected: &Expected) -> Result<u64> {
    file.flush()?;
    file.sync_all()?;
    let len = file.metadata()?.len();
    let partial = partial_path(dest);
    let fail = |msg: String| -> Result<u64> {
        let _ = std::fs::remove_file(&partial);
        let _ = std::fs::remove_file(validator_path(dest));
        Err(DriveError::Verify(msg))
    };
    if len != total || expected.size.is_some_and(|s| s != len) {
        return fail(format!("got {len} bytes, expected {}", expected.size.unwrap_or(total)));
    }
    if let Some(want) = &expected.sha256 {
        file.seek(SeekFrom::Start(0))?;
        let got = sha256_hex(&mut file)?;
        if !got.eq_ignore_ascii_case(want.trim()) {
            return fail(format!("SHA-256 {got} does not match {want}"));
        }
    }
    drop(file);
    std::fs::rename(&partial, dest)?;
    let _ = std::fs::remove_file(validator_path(dest));
    Ok(len)
}

/// Lowercase hex SHA-256 of everything `reader` yields.
pub fn sha256_hex(reader: &mut dyn Read) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn header<'a>(resp: &'a ureq::http::Response<ureq::Body>, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

fn content_length(resp: &ureq::http::Response<ureq::Body>) -> Result<u64> {
    header(resp, "content-length").and_then(|v| v.trim().parse().ok()).ok_or_else(|| DriveError::BadResponse("download has no Content-Length".into()))
}

/// `Content-Range: bytes <start>-<end>/<total>`.
fn content_range(resp: &ureq::http::Response<ureq::Body>) -> Result<Span> {
    let bad = || DriveError::BadResponse("download has no valid Content-Range".into());
    let value = header(resp, "content-range").ok_or_else(bad)?;
    let (range, total) = value.trim().strip_prefix("bytes ").and_then(|r| r.split_once('/')).ok_or_else(bad)?;
    let (start, end) = range.split_once('-').ok_or_else(bad)?;
    let start: u64 = start.trim().parse().map_err(|_| bad())?;
    let end: u64 = end.trim().parse().map_err(|_| bad())?;
    let total: u64 = total.trim().parse().map_err(|_| bad())?;
    if end < start || end >= total {
        return Err(bad());
    }
    Ok(Span { start, total })
}

/// `Content-Range: bytes */<total>` on a 416.
fn unsatisfied_total(resp: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    header(resp, "content-range")?.trim().strip_prefix("bytes */")?.trim().parse().ok()
}
