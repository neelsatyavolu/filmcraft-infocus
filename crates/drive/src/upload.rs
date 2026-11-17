//! Resumable media uploads into a team project's media folder (`Team Projects/<name> Media/`).
//!
//! `POST …/media/upload/init` opens a chunked session, chunks go to `PUT /api/upload/chunk`, the
//! session's state is `GET /api/upload/status`, and `POST …/media/upload/complete` puts the file in
//! place. A session survives a dropped connection (the server keeps it for a day): pass its id back
//! to [`DriveClient::upload_media`] and only the missing chunks are sent.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::client::{Body, DriveClient, Method, project_path};
use crate::download::valid_media_path;
use crate::error::{DriveError, Result};
use crate::http;

/// `expect_mtime_ns` for "the file must not exist yet": completing never overwrites.
const MUST_NOT_EXIST: i64 = -1;
/// Tries per chunk when the network drops a request (the chunk PUT is safe to repeat).
const CHUNK_TRIES: u32 = 3;
/// Wait before the second try; the third waits twice as long.
const CHUNK_RETRY_WAIT: Duration = Duration::from_millis(400);

/// Largest chunk this client sends or accepts from the server (the server caps chunks at 32 MiB).
pub const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// A chunked upload session, from init or status.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UploadSession {
    pub upload_id: String,
    pub chunk_size: u64,
    pub total_chunks: u64,
    pub size: u64,
    /// Chunk indices the server already has.
    #[serde(default)]
    pub received: Vec<u64>,
}

impl UploadSession {
    /// The byte range `(offset, len)` of chunk `index`, or None when the session's numbers don't
    /// add up (a hostile or damaged answer) or `index` is out of range.
    pub fn chunk_range(&self, index: u64) -> Option<(u64, u64)> {
        if self.chunk_size == 0
            || self.chunk_size > MAX_CHUNK_SIZE
            || self.size == 0
            || self.total_chunks != self.size.div_ceil(self.chunk_size)
            || index >= self.total_chunks
        {
            return None;
        }
        let offset = index.checked_mul(self.chunk_size)?;
        Some((offset, self.chunk_size.min(self.size.checked_sub(offset)?)))
    }

    fn valid_for(&self, size: u64) -> bool {
        self.size == size && self.chunk_range(0).is_some() && self.received.iter().all(|&i| i < self.total_chunks)
    }
}

/// The uploaded file, as the Drive lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UploadedMedia {
    #[serde(default)]
    pub name: String,
    /// Drive-relative path (`Team Projects/<name> Media/…`).
    pub path: String,
    pub size: u64,
    /// For a later overwrite guard (`expect_mtime_ns`).
    #[serde(default)]
    pub mtime_ns: Option<i64>,
}

/// Where an upload stands, for [`DriveClient::upload_media`]'s progress callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadProgress<'a> {
    /// Keep this to resume after a failure or a pause.
    pub upload_id: &'a str,
    pub done: u64,
    pub total: u64,
}

/// Session ids are `secrets.token_urlsafe` strings.
fn upload_id(id: &str) -> Result<&str> {
    if (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        Ok(id)
    } else {
        Err(DriveError::Invalid(format!("upload id {id:?}")))
    }
}

fn query(pairs: &[(&str, &str)]) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        q.append_pair(k, v);
    }
    q.finish()
}

impl DriveClient {
    /// Opens an upload of `size` bytes named `name` into the media folder, in its subfolder `dir`
    /// (made when missing). `chunk_size` is a wish; the server picks (1 to 32 MiB).
    pub fn media_upload_init(&self, project: &str, name: &str, size: u64, dir: Option<&str>, chunk_size: Option<u64>) -> Result<UploadSession> {
        let mut body = json!({ "name": name, "size": size });
        if let Some(dir) = dir {
            body["path"] = json!(dir);
        }
        if let Some(cs) = chunk_size {
            body["chunk_size"] = json!(cs);
        }
        let session: UploadSession = http::parse(&self.call(Method::Post, &format!("{}/media/upload/init", project_path(project)?), Some(&body))?)?;
        if !session.valid_for(size) {
            return Err(DriveError::BadResponse(format!(
                "upload session for {} bytes in {} chunks of {}",
                session.size, session.total_chunks, session.chunk_size
            )));
        }
        upload_id(&session.upload_id)?;
        Ok(session)
    }

    /// The session's state: which chunks the server has. `NotFound` once it expired.
    pub fn upload_status(&self, id: &str) -> Result<UploadSession> {
        let path = format!("/api/upload/status?{}", query(&[("upload_id", upload_id(id)?)]));
        http::parse(&self.call(Method::Get, &path, None)?)
    }

    /// Sends chunk `index` (safe to send again: it replaces the same chunk).
    pub fn upload_chunk(&self, id: &str, index: u64, bytes: &[u8]) -> Result<()> {
        let path = format!("/api/upload/chunk?{}", query(&[("upload_id", upload_id(id)?), ("index", &index.to_string())]));
        self.request(Method::Put, &path, Body::Bytes(bytes), &[])?;
        Ok(())
    }

    /// [`Self::upload_chunk`], tried again after a network error ([`CHUNK_TRIES`] tries in
    /// all, with a short wait between), so one dropped request on flaky Wi-Fi doesn't end a
    /// whole parallel upload. `keep_going` false stops the waiting.
    fn upload_chunk_retrying(&self, id: &str, index: u64, bytes: &[u8], keep_going: &mut dyn FnMut() -> bool) -> Result<()> {
        let mut tries: u32 = 0;
        loop {
            tries += 1;
            match self.upload_chunk(id, index, bytes) {
                Err(DriveError::Network(why)) if tries < CHUNK_TRIES => {
                    log::info!("upload chunk {index} failed ({why}); trying again");
                    let wait = CHUNK_RETRY_WAIT * tries;
                    let until = std::time::Instant::now() + wait;
                    while std::time::Instant::now() < until {
                        if !keep_going() {
                            return Err(DriveError::Cancelled);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                other => return other,
            }
        }
    }

    /// Drops the session and its chunks.
    pub fn upload_abort(&self, id: &str) -> Result<()> {
        self.request(Method::Post, "/api/upload/abort", Body::Form(&[("upload_id", upload_id(id)?)]), &[])?;
        Ok(())
    }

    /// Puts the uploaded file in place. `expect_mtime_ns` guards an overwrite (the file must still
    /// have that modification time); None for a new file.
    pub fn media_upload_complete(&self, project: &str, id: &str, expect_mtime_ns: Option<i64>) -> Result<UploadedMedia> {
        let mut body = json!({ "upload_id": upload_id(id)? });
        if let Some(m) = expect_mtime_ns {
            body["expect_mtime_ns"] = json!(m);
        }
        http::parse(&self.call(Method::Post, &format!("{}/media/upload/complete", project_path(project)?), Some(&body))?)
    }

    /// Uploads `src` as `name` into the media folder (subfolder `dir`). With `resume` (an id from an
    /// earlier [`UploadProgress`]) it asks the server which chunks arrived and sends only the rest;
    /// an expired or mismatched session starts over. `on_progress` returning `false` stops with
    /// [`DriveError::Cancelled`] and the session stays open for resuming.
    ///
    /// Never overwrites: when a file with that name is already there (a teammate's), the session is
    /// dropped and the answer is [`DriveError::NameTaken`], so the caller can pick another name.
    pub fn upload_media(
        &self,
        project: &str,
        src: &Path,
        name: &str,
        dir: Option<&str>,
        resume: Option<&str>,
        on_progress: &mut dyn FnMut(&UploadProgress<'_>) -> bool,
    ) -> Result<UploadedMedia> {
        let file = File::open(src)?;
        let size = file.metadata()?.len();
        if size == 0 {
            return Err(DriveError::Invalid("an empty file can't be uploaded as media".into()));
        }
        let resumed = match resume {
            Some(id) => match self.upload_status(id) {
                Ok(s) if s.upload_id == id && s.valid_for(size) => Some(s),
                Ok(_) => {
                    let _ = self.upload_abort(id);
                    None
                }
                Err(DriveError::NotFound(_)) => None,
                Err(e) => return Err(e),
            },
            None => None,
        };
        let session = match resumed {
            Some(s) => s,
            None => self.media_upload_init(project, name, size, dir, self.transfers().upload_chunk)?,
        };
        let have: std::collections::BTreeSet<u64> = session.received.iter().copied().collect();
        let done = have.iter().filter_map(|&i| session.chunk_range(i)).fold(0u64, |acc, (_, len)| acc.saturating_add(len));
        let progress = |done| UploadProgress { upload_id: &session.upload_id, done, total: size };
        if !on_progress(&progress(done)) {
            return Err(DriveError::Cancelled);
        }
        drop(file);
        let todo: Vec<u64> = (0..session.total_chunks).filter(|i| !have.contains(i)).collect();
        self.send_chunks(&session, src, &todo, done, &mut |done| on_progress(&progress(done)))?;
        match self.media_upload_complete(project, &session.upload_id, Some(MUST_NOT_EXIST)) {
            // create-only: the one conflict `complete` has is a file already at that name
            Err(DriveError::NameTaken(msg) | DriveError::Conflict(msg)) => {
                let _ = self.upload_abort(&session.upload_id);
                Err(DriveError::NameTaken(msg))
            }
            other => other,
        }
    }

    /// Sends chunks `todo` of `src`, [`crate::Transfers::upload_streams`] at once, each holding a
    /// place among the client's media streams. `on_progress(done)` runs on this thread every
    /// 100 ms (`done` starts at `done`); false stops with [`DriveError::Cancelled`].
    fn send_chunks(&self, session: &UploadSession, src: &Path, todo: &[u64], done: u64, on_progress: &mut dyn FnMut(u64) -> bool) -> Result<()> {
        if self.transfers().upload_streams <= 1 {
            return self.send_chunks_in_turn(session, src, todo, done, on_progress);
        }
        let queue = Mutex::new(todo.iter().copied().collect::<VecDeque<u64>>());
        let sent = AtomicU64::new(done);
        let stop = AtomicBool::new(false);
        let error: Mutex<Option<DriveError>> = Mutex::new(None);
        let fail = |e: DriveError| {
            stop.store(true, Ordering::Relaxed);
            let mut slot = error.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.is_none() {
                *slot = Some(e);
            }
        };
        let worker = || -> Result<()> {
            let mut file = File::open(src)?;
            loop {
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let Some(index) = queue.lock().unwrap_or_else(PoisonError::into_inner).pop_front() else { return Ok(()) };
                let (offset, len) = session.chunk_range(index).ok_or_else(|| DriveError::BadResponse("upload session".into()))?;
                let mut buf = vec![0u8; usize::try_from(len).map_err(|_| DriveError::BadResponse("chunk size".into()))?];
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut buf)?;
                let Some(_permit) = self.streams.acquire(self.transfers().max_streams, &mut || !stop.load(Ordering::Relaxed)) else { return Ok(()) };
                self.upload_chunk_retrying(&session.upload_id, index, &buf, &mut || !stop.load(Ordering::Relaxed))?;
                sent.fetch_add(len, Ordering::Relaxed);
            }
        };
        let streams = self.transfers().upload_streams.clamp(1, todo.len().max(1));
        let mut cancelled = false;
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..streams)
                .map(|_| {
                    std::thread::Builder::new().name("drive-upload-chunk".into()).spawn_scoped(scope, || {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(worker)) {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => fail(e),
                            Err(_) => fail(DriveError::Network("an upload stream stopped unexpectedly".into())),
                        }
                    })
                })
                .collect();
            for h in &handles {
                if let Err(e) = h {
                    fail(DriveError::Io(std::io::Error::other(e.to_string())));
                }
            }
            loop {
                let running = handles.iter().any(|h| h.as_ref().is_ok_and(|h| !h.is_finished()));
                if !on_progress(sent.load(Ordering::Relaxed)) {
                    cancelled = true;
                    stop.store(true, Ordering::Relaxed);
                }
                if !running {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        if let Some(e) = error.into_inner().unwrap_or_else(PoisonError::into_inner) {
            return Err(e);
        }
        if cancelled || !queue.into_inner().unwrap_or_else(PoisonError::into_inner).is_empty() {
            return Err(DriveError::Cancelled);
        }
        Ok(())
    }

    /// One chunk after another, asking `on_progress` after each.
    fn send_chunks_in_turn(&self, session: &UploadSession, src: &Path, todo: &[u64], mut done: u64, on_progress: &mut dyn FnMut(u64) -> bool) -> Result<()> {
        let mut file = File::open(src)?;
        for &index in todo {
            let (offset, len) = session.chunk_range(index).ok_or_else(|| DriveError::BadResponse("upload session".into()))?;
            let mut buf = vec![0u8; usize::try_from(len).map_err(|_| DriveError::BadResponse("chunk size".into()))?];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut buf)?;
            let Some(_permit) = self.streams.acquire(self.transfers().max_streams, &mut || on_progress(done)) else { return Err(DriveError::Cancelled) };
            self.upload_chunk_retrying(&session.upload_id, index, &buf, &mut || on_progress(done))?;
            done = done.saturating_add(len);
            if !on_progress(done) {
                return Err(DriveError::Cancelled);
            }
        }
        Ok(())
    }

    /// Whether the media folder already has a file at `media_path` (a path inside `<name> Media/`):
    /// asks for its first byte.
    pub fn media_exists(&self, project: &str, media_path: &str) -> Result<bool> {
        let rel = valid_media_path(media_path)?;
        let path = format!("{}/media?{}", project_path(project)?, query(&[("path", rel)]));
        let resp = self.open_stream(&path, &[("Range", "bytes=0-0".to_string())])?;
        match resp.status().as_u16() {
            200 | 206 | 416 => Ok(true),
            404 => Ok(false),
            status => {
                let (_, bytes) = http::read_body(resp)?;
                Err(http::error_for(status, &bytes))
            }
        }
    }
}
