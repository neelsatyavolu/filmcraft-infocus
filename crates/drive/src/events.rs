//! The team project change feed: a server-sent events parser and the typed events.

use std::io::Read;

use serde::Deserialize;

use crate::error::{DriveError, Result};

/// Longest line accepted from the feed; a longer one means the stream is not ours.
const MAX_LINE: usize = 1024 * 1024;
/// Largest `data` payload of one event.
const MAX_DATA: usize = 4 * 1024 * 1024;

/// One event from `GET /api/team/projects/{id}/events`.
#[derive(Debug, Clone, PartialEq)]
pub enum TeamEvent {
    /// A sequence was saved; fetch it unless it is ours.
    SequenceUpdated { seq: u64, rev: u64, by: String },
    /// A sequence lock was taken (`holder` set) or released (`None`).
    LockChanged { seq: u64, holder: Option<String> },
    /// A sequence was deleted (its lock goes with it; a `lock_changed` follows). `rev` keeps
    /// counting from the sequence's last revision.
    SequenceDeleted { seq: u64, rev: u64, by: String },
    /// The shared part changed; `fields` names the project-level fields the change replaced.
    SharedUpdated { rev: u64, fields: Vec<String> },
    /// An event this client does not know (a newer server); safe to ignore.
    Other { event: String, data: String },
}

/// One raw server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Incremental `text/event-stream` parser: feed it bytes as they arrive, in any chunking.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
    after_cr: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes and returns every event they complete.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>> {
        let mut out = Vec::new();
        for &b in bytes {
            let after_cr = std::mem::replace(&mut self.after_cr, false);
            match b {
                b'\n' if after_cr => {} // the LF of a CRLF pair
                b'\n' | b'\r' => {
                    self.after_cr = b == b'\r';
                    let line = std::mem::take(&mut self.line);
                    if let Some(ev) = self.line_done(&line)? {
                        out.push(ev);
                    }
                }
                _ => {
                    if self.line.len() >= MAX_LINE {
                        return Err(DriveError::BadResponse("event stream line too long".into()));
                    }
                    self.line.push(b);
                }
            }
        }
        Ok(out)
    }

    fn line_done(&mut self, line: &[u8]) -> Result<Option<SseEvent>> {
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            let data = std::mem::take(&mut self.data);
            let had = std::mem::replace(&mut self.has_data, false);
            if !had {
                return Ok(None);
            }
            let event = if event.is_empty() { "message".to_string() } else { event };
            return Ok(Some(SseEvent { event, data }));
        }
        if line.first() == Some(&b':') {
            return Ok(None); // comment (subscribed / keepalive)
        }
        let text = String::from_utf8_lossy(line);
        let (field, value) = match text.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (text.as_ref(), ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                if self.data.len().saturating_add(value.len()) > MAX_DATA {
                    return Err(DriveError::BadResponse("event too large".into()));
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {} // id, retry and unknown fields are not used
        }
        Ok(None)
    }
}

#[derive(Deserialize)]
struct SeqUpdated {
    seq: u64,
    rev: u64,
    #[serde(default)]
    by: String,
}

#[derive(Deserialize)]
struct LockChanged {
    seq: u64,
    holder: Option<String>,
}

#[derive(Deserialize)]
struct SharedUpdated {
    rev: u64,
    #[serde(default)]
    fields: Vec<String>,
}

impl TeamEvent {
    /// Types a raw event. Known events with a malformed payload are an error.
    pub fn from_sse(ev: SseEvent) -> Result<Self> {
        let bad = |e: serde_json::Error| DriveError::BadResponse(format!("{} event: {e}", ev.event));
        Ok(match ev.event.as_str() {
            "sequence_updated" => {
                let d: SeqUpdated = serde_json::from_str(&ev.data).map_err(bad)?;
                TeamEvent::SequenceUpdated { seq: d.seq, rev: d.rev, by: d.by }
            }
            "sequence_deleted" => {
                let d: SeqUpdated = serde_json::from_str(&ev.data).map_err(bad)?;
                TeamEvent::SequenceDeleted { seq: d.seq, rev: d.rev, by: d.by }
            }
            "lock_changed" => {
                let d: LockChanged = serde_json::from_str(&ev.data).map_err(bad)?;
                TeamEvent::LockChanged { seq: d.seq, holder: d.holder }
            }
            "shared_updated" => {
                let d: SharedUpdated = serde_json::from_str(&ev.data).map_err(bad)?;
                TeamEvent::SharedUpdated { rev: d.rev, fields: d.fields }
            }
            _ => TeamEvent::Other { event: ev.event, data: ev.data },
        })
    }
}

/// A blocking iterator over a project's change feed. It ends when the server closes the stream;
/// reconnect then (and poll every 5 s while disconnected).
pub struct EventStream {
    reader: Box<dyn Read + Send>,
    parser: SseParser,
    pending: std::collections::VecDeque<SseEvent>,
    done: bool,
}

impl EventStream {
    pub(crate) fn new(reader: Box<dyn Read + Send>) -> Self {
        Self { reader, parser: SseParser::new(), pending: Default::default(), done: false }
    }
}

impl Iterator for EventStream {
    type Item = Result<TeamEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut buf = [0u8; 8192];
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(TeamEvent::from_sse(ev));
            }
            if self.done {
                return None;
            }
            match self.reader.read(&mut buf) {
                Ok(0) => {
                    self.done = true;
                }
                Ok(n) => match self.parser.push(buf.get(..n).unwrap_or_default()) {
                    Ok(evs) => self.pending.extend(evs),
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.done = true;
                    return Some(Err(DriveError::Network(e.to_string())));
                }
            }
        }
    }
}
