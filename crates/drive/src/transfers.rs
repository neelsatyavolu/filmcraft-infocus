//! How hard one [`DriveClient`](crate::DriveClient) pushes the Drive: how many parallel ranges a
//! big download is split into, how many chunks an upload sends at once, and one limit on the media
//! streams open at the same time for everything this client moves, so a few Macs never swamp the
//! NAS.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// A download this big (bytes) or bigger is split into ranges fetched in parallel.
pub const PARALLEL_MIN_BYTES: u64 = 48 * 1024 * 1024;
/// Ranges one download is split into at most.
pub const MAX_PARTS: u64 = 4;
/// Chunks one upload sends at once.
pub const UPLOAD_STREAMS: usize = 3;
/// Upload chunk size asked of the Drive (it allows 1 to 32 MiB): smaller than the Drive's 32 MiB
/// default so parallel chunks don't hold too much memory.
pub const UPLOAD_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
/// Media streams (download ranges and upload chunks) open at once for one client.
pub const MAX_MEDIA_STREAMS: usize = 8;

/// The transfer settings of a client. [`Transfers::default`] is what the app uses;
/// [`Transfers::serial`] is the old one-stream behaviour (for comparing speeds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfers {
    /// Ranges a big download is split into (1: one stream, as before).
    pub parts: u64,
    /// Downloads at least this big are split.
    pub parallel_min_bytes: u64,
    /// Chunks an upload sends at once (1: one after another, as before).
    pub upload_streams: usize,
    /// Chunk size to ask for; None takes the Drive's default.
    pub upload_chunk: Option<u64>,
    /// Media streams open at once for the whole client.
    pub max_streams: usize,
}

impl Default for Transfers {
    fn default() -> Self {
        Self {
            parts: MAX_PARTS,
            parallel_min_bytes: PARALLEL_MIN_BYTES,
            upload_streams: UPLOAD_STREAMS,
            upload_chunk: Some(UPLOAD_CHUNK_BYTES),
            max_streams: MAX_MEDIA_STREAMS,
        }
    }
}

impl Transfers {
    /// One stream per file and the Drive's default chunk size: how the client worked before.
    pub fn serial() -> Self {
        Self { parts: 1, parallel_min_bytes: u64::MAX, upload_streams: 1, upload_chunk: None, max_streams: MAX_MEDIA_STREAMS }
    }

    /// The same settings with values a caller can't break the client with.
    pub(crate) fn sane(self) -> Self {
        Self {
            parts: self.parts.clamp(1, 16),
            parallel_min_bytes: self.parallel_min_bytes.max(1),
            upload_streams: self.upload_streams.clamp(1, 16),
            upload_chunk: self.upload_chunk.map(|c| c.clamp(1024 * 1024, 32 * 1024 * 1024)),
            max_streams: self.max_streams.clamp(1, 64),
        }
    }
}

/// Counts the media streams open at once.
#[derive(Debug, Default)]
pub(crate) struct Streams {
    open: Mutex<usize>,
    freed: Condvar,
}

/// One open stream; dropping it frees the place.
pub(crate) struct Permit<'a>(&'a Streams);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut open = self.0.open.lock().unwrap_or_else(PoisonError::into_inner);
        *open = open.saturating_sub(1);
        self.0.freed.notify_one();
    }
}

impl Streams {
    /// Waits for a place among `max`; `keep_going` is asked every 100 ms while waiting, and None
    /// comes back when it says stop.
    pub(crate) fn acquire(&self, max: usize, keep_going: &mut dyn FnMut() -> bool) -> Option<Permit<'_>> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if *open < max.max(1) {
                *open += 1;
                return Some(Permit(self));
            }
            let (guard, _) = self.freed.wait_timeout(open, Duration::from_millis(100)).unwrap_or_else(PoisonError::into_inner);
            open = guard;
            if !keep_going() {
                return None;
            }
        }
    }
}
