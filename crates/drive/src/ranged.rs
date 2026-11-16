//! Big originals download as a few byte ranges in parallel, which fills a fast link (the campus
//! LAN, or a long internet path where one TCP stream stalls) much better than one stream.
//!
//! - `<dest>.partial` is made the full size up front and each range is written at its offset.
//!   Because its length no longer says how much arrived, `<dest>.partial.parts` (JSON) records each
//!   range's progress and the Drive's ETag. It is written before the partial file grows, and only
//!   after the bytes it counts were flushed, so a crash never counts bytes that aren't there.
//! - Resuming reads that file: a range carries on where it stopped (`Range: bytes=N-M` with
//!   `If-Range`). A changed file on the Drive (another ETag, or a full `200` answer) starts over.
//! - A partial file from a one-stream download (no `.parts`) keeps its bytes as the first range.
//! - The Drive not honouring ranges falls back to the one-stream download.
//! - Every range holds a place among the client's media streams ([`crate::transfers`]).

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::client::DriveClient;
use crate::download::{Expected, finish, partial_path, with_suffix};
use crate::error::{DriveError, Result};
use crate::http;

/// Bytes a range writes between two saves of its progress.
const SAVE_EVERY: u64 = 32 * 1024 * 1024;
/// Tries per range for a dropped connection before the download fails (it resumes later).
const RANGE_TRIES: u32 = 3;
const COPY_BUF: usize = 256 * 1024;
/// Largest `.parts` file read (a few ranges of JSON).
const MAX_PARTS_FILE: u64 = 64 * 1024;

/// `<dest>.partial.parts`: the ranges of a parallel download.
pub fn parts_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".partial.parts")
}

/// One range: bytes `start..end`, of which the first `done` are on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Part {
    start: u64,
    end: u64,
    done: u64,
}

impl Part {
    fn left(&self) -> u64 {
        self.end.saturating_sub(self.start).saturating_sub(self.done)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Plan {
    size: u64,
    etag: String,
    parts: Vec<Part>,
}

impl Plan {
    /// The ranges cover `0..size` exactly, in order, each with a sane `done`.
    fn valid(&self) -> bool {
        let mut at = 0u64;
        for p in &self.parts {
            if p.start != at || p.end <= p.start || p.done > p.end - p.start {
                return false;
            }
            at = p.end;
        }
        at == self.size && !self.parts.is_empty() && self.parts.len() <= 64
    }

    fn done(&self) -> u64 {
        self.parts.iter().fold(0u64, |acc, p| acc.saturating_add(p.done))
    }

    /// `size` split into `n` ranges, the first `have` bytes already there (a one-stream partial).
    fn new(size: u64, etag: String, n: u64, have: u64) -> Self {
        let have = have.min(size);
        let mut parts = Vec::new();
        if have > 0 {
            parts.push(Part { start: 0, end: have, done: have });
        }
        let rest = size - have;
        let n = n.clamp(1, rest.max(1));
        let step = rest.div_ceil(n).max(1);
        let mut at = have;
        while at < size {
            let end = at.saturating_add(step).min(size);
            parts.push(Part { start: at, end, done: 0 });
            at = end;
        }
        Self { size, etag, parts }
    }
}

fn load_plan(path: &Path) -> Option<Plan> {
    let len = std::fs::metadata(path).ok()?.len();
    if len > MAX_PARTS_FILE {
        return None;
    }
    let plan: Plan = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    plan.valid().then_some(plan)
}

/// Writes the plan next to the partial file (through a temporary file, so it is whole or old).
fn save_plan(path: &Path, plan: &Plan) -> Result<()> {
    let tmp = with_suffix(path, ".tmp");
    let bytes = serde_json::to_vec(plan).map_err(|e| DriveError::Invalid(e.to_string()))?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Forgets a parallel download (its partial bytes and ranges).
fn forget(dest: &Path) {
    let _ = std::fs::remove_file(parts_path(dest));
    let _ = std::fs::remove_file(partial_path(dest));
}

/// What the probe learnt about the file on the Drive.
pub(crate) enum Probe {
    /// Ranges work: the size and validator.
    Ranges { size: u64, etag: String },
    /// The Drive sent the whole file or no validator: use one stream.
    NoRanges,
}

/// Shared between the range threads and the thread reporting progress.
struct Shared {
    plan: Mutex<Plan>,
    done: AtomicU64,
    stop: AtomicBool,
    changed: AtomicBool,
    error: Mutex<Option<DriveError>>,
}

impl Shared {
    fn fail(&self, e: DriveError) {
        self.stop.store(true, Ordering::Relaxed);
        let mut slot = self.error.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(e);
        }
    }
}

impl DriveClient {
    /// Asks for the first byte: the Drive's size and ETag for the file, and whether it honours
    /// ranges.
    pub(crate) fn probe_media(&self, route: &str, query: &str) -> Result<Probe> {
        let resp = self.open_stream(&format!("{route}?{query}"), &[("Range", "bytes=0-0".to_string())])?;
        let status = resp.status().as_u16();
        let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
        let range = resp.headers().get("content-range").and_then(|v| v.to_str().ok()).map(str::to_string);
        // read (at most a byte) so the connection can go back to the pool
        let (_, body) = http::read_body(resp)?;
        match status {
            206 => {
                let size = range
                    .as_deref()
                    .and_then(|r| r.trim().strip_prefix("bytes 0-0/"))
                    .and_then(|t| t.trim().parse::<u64>().ok())
                    .ok_or_else(|| DriveError::BadResponse("download has no valid Content-Range".into()))?;
                Ok(match etag.filter(|e| !e.trim().is_empty()) {
                    Some(etag) => Probe::Ranges { size, etag },
                    None => Probe::NoRanges,
                })
            }
            200 => Ok(Probe::NoRanges),
            _ => Err(http::error_for(status, &body)),
        }
    }

    /// The parallel download of a file the probe sized at `size` with validator `etag`. See the
    /// module docs; the arguments are [`DriveClient::download_media`]'s.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn download_ranges(
        &self,
        route: &str,
        query: &str,
        dest: &Path,
        size: u64,
        etag: String,
        expected: &Expected,
        on_progress: &mut dyn FnMut(u64, u64) -> bool,
    ) -> Result<u64> {
        if expected.size.is_some_and(|s| s != size) {
            forget(dest);
            return Err(DriveError::Verify(format!("the Drive has {size} bytes, expected {}", expected.size.unwrap_or(0))));
        }
        if size == 0 {
            return Err(DriveError::BadResponse("the Drive says the file is empty".into()));
        }
        let parts_file = parts_path(dest);
        let partial = partial_path(dest);
        let plan = match load_plan(&parts_file) {
            Some(p) if p.size == size && p.etag == etag && partial.is_file() => p,
            _ => {
                // a one-stream partial of the same file keeps its bytes
                let have = std::fs::read_to_string(with_suffix(dest, ".partial.etag"))
                    .ok()
                    .filter(|tag| tag.trim() == etag)
                    .and_then(|_| std::fs::metadata(&partial).ok())
                    .map_or(0, |m| m.len());
                let have = if have < size { have } else { 0 };
                Plan::new(size, etag.clone(), self.transfers().parts, have)
            }
        };
        // the plan is on disk before the file grows, so its length never passes for progress
        save_plan(&parts_file, &plan)?;
        let _ = std::fs::remove_file(with_suffix(dest, ".partial.etag"));
        let file = OpenOptions::new().create(true).truncate(false).write(true).read(true).open(&partial)?;
        if file.metadata()?.len() != size {
            file.set_len(size)?;
        }
        let shared = Shared {
            done: AtomicU64::new(plan.done()),
            plan: Mutex::new(plan.clone()),
            stop: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            error: Mutex::new(None),
        };
        let todo: Vec<usize> = plan.parts.iter().enumerate().filter(|(_, p)| p.left() > 0).map(|(i, _)| i).collect();
        let mut cancelled = false;
        std::thread::scope(|scope| {
            let workers: Vec<_> = todo
                .iter()
                .map(|&i| {
                    let shared = &shared;
                    let partial = &partial;
                    let parts_file = &parts_file;
                    let etag = etag.as_str();
                    std::thread::Builder::new().name("drive-range".into()).spawn_scoped(scope, move || {
                        let run =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.fetch_range(route, query, partial, parts_file, etag, i, shared)));
                        match run {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => shared.fail(e),
                            Err(_) => shared.fail(DriveError::Network("a download stream stopped unexpectedly".into())),
                        }
                    })
                })
                .collect();
            for w in &workers {
                if let Err(e) = w {
                    shared.fail(DriveError::Io(std::io::Error::other(e.to_string())));
                }
            }
            // report progress here, on the caller's thread, until every range is done
            loop {
                let running = workers.iter().any(|w| w.as_ref().is_ok_and(|h| !h.is_finished()));
                if !on_progress(shared.done.load(Ordering::Relaxed).min(size), size) {
                    cancelled = true;
                    shared.stop.store(true, Ordering::Relaxed);
                }
                if !running {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let plan = shared.plan.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if shared.changed.load(Ordering::Relaxed) {
            forget(dest);
            return Err(DriveError::Verify("the file on the Drive changed; downloading again".into()));
        }
        file.sync_data()?;
        save_plan(&parts_file, &plan)?;
        if let Some(e) = shared.error.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Err(e);
        }
        if cancelled {
            return Err(DriveError::Cancelled);
        }
        if plan.done() != size {
            return Err(DriveError::Network(format!("stopped after {} of {size} bytes", plan.done())));
        }
        let n = finish(file, dest, size, expected);
        let _ = std::fs::remove_file(&parts_file);
        if n.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        n
    }

    /// Fetches what is left of range `i`, trying a dropped connection again a few times.
    #[allow(clippy::too_many_arguments)]
    fn fetch_range(&self, route: &str, query: &str, partial: &Path, parts_file: &Path, etag: &str, i: usize, shared: &Shared) -> Result<()> {
        let mut tries = 0;
        loop {
            let Some(_permit) = self.streams.acquire(self.transfers().max_streams, &mut || !shared.stop.load(Ordering::Relaxed)) else { return Ok(()) };
            match self.fetch_range_once(route, query, partial, parts_file, etag, i, shared) {
                Ok(()) => return Ok(()),
                Err(DriveError::Network(_)) if tries + 1 < RANGE_TRIES && !shared.stop.load(Ordering::Relaxed) => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(300 * u64::from(tries)));
                }
                Err(e) => return Err(e),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn fetch_range_once(&self, route: &str, query: &str, partial: &Path, parts_file: &Path, etag: &str, i: usize, shared: &Shared) -> Result<()> {
        let part = shared.plan.lock().unwrap_or_else(PoisonError::into_inner).parts.get(i).copied().ok_or_else(|| DriveError::Invalid("range".into()))?;
        if part.left() == 0 || shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let from = part.start.saturating_add(part.done);
        let last = part.end.saturating_sub(1);
        let headers = [("Range", format!("bytes={from}-{last}")), ("If-Range", etag.to_string())];
        let mut resp = self.open_stream(&format!("{route}?{query}"), &headers)?;
        let status = resp.status().as_u16();
        match status {
            206 => {}
            200 => {
                // the validator no longer matches: the file changed on the Drive
                shared.changed.store(true, Ordering::Relaxed);
                shared.stop.store(true, Ordering::Relaxed);
                return Ok(());
            }
            _ => {
                let (_, bytes) = http::read_body(resp)?;
                return Err(http::error_for(status, &bytes));
            }
        }
        let starts_at = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().strip_prefix("bytes "))
            .and_then(|v| v.split_once('-'))
            .and_then(|(s, _)| s.trim().parse::<u64>().ok());
        if starts_at != Some(from) {
            return Err(DriveError::BadResponse(format!("range starts at {starts_at:?} (asked for {from})")));
        }
        let want = part.left();
        let mut file = OpenOptions::new().write(true).open(partial)?;
        file.seek(SeekFrom::Start(from))?;
        let mut reader = resp.body_mut().with_config().limit(want).reader();
        let mut buf = vec![0u8; COPY_BUF];
        let (mut got, mut unsaved) = (0u64, 0u64);
        let record = |file: &mut std::fs::File, got: u64| -> Result<()> {
            file.flush()?;
            file.sync_data()?;
            let mut plan = shared.plan.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(p) = plan.parts.get_mut(i) {
                p.done = part.done.saturating_add(got).min(p.end - p.start);
            }
            save_plan(parts_file, &plan)
        };
        while got < want {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    record(&mut file, got)?;
                    return Err(DriveError::Network(e.to_string()));
                }
            };
            let n64 = (n as u64).min(want - got);
            file.write_all(buf.get(..n64 as usize).unwrap_or_default())?;
            got += n64;
            unsaved += n64;
            shared.done.fetch_add(n64, Ordering::Relaxed);
            if unsaved >= SAVE_EVERY {
                record(&mut file, got)?;
                unsaved = 0;
            }
        }
        record(&mut file, got)?;
        if got < want && !shared.stop.load(Ordering::Relaxed) {
            return Err(DriveError::Network(format!("connection closed after {got} of {want} bytes")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_splits_and_keeps_a_one_stream_partial() {
        let p = Plan::new(100, "e".into(), 4, 0);
        assert!(p.valid());
        assert_eq!(p.parts.len(), 4);
        assert_eq!(p.parts.iter().map(|p| p.end - p.start).collect::<Vec<_>>(), vec![25, 25, 25, 25]);
        let p = Plan::new(100, "e".into(), 4, 30);
        assert!(p.valid());
        assert_eq!(p.parts[0], Part { start: 0, end: 30, done: 30 });
        assert_eq!(p.done(), 30);
        let tiny = Plan::new(3, "e".into(), 4, 0);
        assert!(tiny.valid() && tiny.parts.len() == 3);
    }

    #[test]
    fn hostile_plans_are_refused() {
        let mut p = Plan::new(100, "e".into(), 2, 0);
        p.parts[1].done = 1000;
        assert!(!p.valid());
        let mut p = Plan::new(100, "e".into(), 2, 0);
        p.parts[1].start = 10;
        assert!(!p.valid());
        let p = Plan { size: 10, etag: "e".into(), parts: vec![] };
        assert!(!p.valid());
    }
}
