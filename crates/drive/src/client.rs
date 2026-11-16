//! `DriveClient`: one method per team project route (`/api/team/…` in infocus-drive).

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::TokenStore;
use crate::config::DriveConfig;
use crate::device::{DEVICE_HEADER, local_device_name, sanitize_device_name};
use crate::error::{DriveError, Result};
use crate::events::EventStream;
use crate::http;
use crate::lan::{self, Route};

/// Largest id the server accepts (ids stay JSON-safe).
pub const MAX_ID: u64 = (1 << 53) - 1;
const API_TIMEOUT: Duration = Duration::from_secs(120);

/// This app's version and project schema, sent with every write so the server can refuse old apps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    version: String,
    schema_version: u32,
    device: String,
}

impl AppInfo {
    /// `version` looks like `1.2.3` (one to four numeric parts), as the server requires. The device
    /// name is this Mac's computer name ([`local_device_name`]); see [`AppInfo::with_device`].
    pub fn new(version: &str, schema_version: u32) -> Result<Self> {
        let parts: Vec<&str> = version.split('.').collect();
        let ok = (1..=4).contains(&parts.len()) && parts.iter().all(|p| (1..=6).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()));
        if !ok {
            return Err(DriveError::Invalid(format!("app version {version:?} must look like 1.2.3")));
        }
        Ok(Self { version: version.to_string(), schema_version, device: local_device_name() })
    }

    /// The same app with another device name (sanitized like [`sanitize_device_name`]).
    pub fn with_device(self, device: &str) -> Self {
        Self { device: sanitize_device_name(device), ..self }
    }
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
    /// The name sent as `X-Drive-Device` with lock requests (empty: no header).
    pub fn device(&self) -> &str {
        &self.device
    }
}

/// A live sequence lock.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LockState {
    pub holder: String,
    /// Unix seconds.
    pub expires_at: f64,
}

/// Where a project stands in the Drive's daily cleanup (`cleanup` in the list and GET). The
/// project list shows a warning while `move_after` is set; "Keep" is [`DriveClient::open`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Cleanup {
    /// Unix seconds when the "not opened for 23 days" notice went out.
    #[serde(default)]
    pub notice_sent_at: Option<f64>,
    /// Earliest Unix seconds the project may move to `#recycle`.
    #[serde(default)]
    pub move_after: Option<f64>,
    /// "Keep forever": cleanup never moves it.
    #[serde(default)]
    pub keep: bool,
}

/// One entry of `GET /api/team/projects`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    /// Drive-relative folder, `Team Projects/<name>.fcteam`.
    pub folder: String,
    #[serde(default)]
    pub locks: BTreeMap<u64, LockState>,
    /// Unix seconds of the last open by anyone (0 when never).
    #[serde(default)]
    pub last_open: f64,
    /// None when no cleanup notice is pending and the project isn't kept forever.
    #[serde(default)]
    pub cleanup: Option<Cleanup>,
}

/// Answer of `POST /api/team/projects`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreatedProject {
    pub id: String,
    /// This member's id slot in the new project.
    pub slot: u16,
}

/// One sequence of a project and its revision.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SequencePart {
    pub rev: u64,
    /// The sequence item's JSON.
    pub json: Value,
}

/// `GET …/shared`: the shared part and its revision.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SharedPart {
    pub rev: u64,
    pub shared: Value,
}

/// The answer to a conditional GET (`If-None-Match`).
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched<T> {
    /// 304: unchanged since the ETag that was sent.
    NotModified,
    /// The part, with the ETag to send next time.
    Fresh { value: T, etag: Option<String> },
}

/// `GET /api/team/projects/{id}`: the whole project (the server also records the open).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProjectData {
    pub id: String,
    pub name: String,
    /// `meta.json` (`schema_version`, `min_app_version`, `created_by`, `file_hashes`, …).
    pub meta: Value,
    /// The shared part: the project JSON with sequence items removed.
    pub shared: Value,
    pub sequences: BTreeMap<u64, SequencePart>,
    pub shared_rev: u64,
    #[serde(default)]
    pub locks: BTreeMap<u64, LockState>,
    /// This member's id slot.
    pub slot: u16,
    /// None when no cleanup notice is pending and the project isn't kept forever.
    #[serde(default)]
    pub cleanup: Option<Cleanup>,
}

/// A granted lock.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LockGrant {
    pub token: String,
    pub expires_at: f64,
}

/// A change to the shared part (`POST …/shared`). Upserts and deletes merge by id; `bins`, when set,
/// replaces the whole root bin and needs `base_rev` to be current. `fields` replaces project-level
/// fields whole, the last writer winning per field; the server takes only its allowlist
/// (`settings`, `custom_colors`, `custom_color_names`, `luts`, `transcripts`, `search_bins`,
/// `source_graphics`, `narrations`, `notes`) and refuses `null`. Leave this Mac's scratch disks
/// and ingest destination out of `settings`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SharedChange {
    pub base_rev: u64,
    pub upserts: BTreeMap<u64, Value>,
    pub deletes: Vec<u64>,
    pub bins: Option<Value>,
    pub fields: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
    Put,
    Delete,
}

/// The InFocus Drive client. Blocking; share it between threads (`Send + Sync`).
pub struct DriveClient {
    cfg: DriveConfig,
    token: String,
    app: AppInfo,
    api: ureq::Agent,
    pub(crate) stream: ureq::Agent,
    route: Mutex<Route>,
}

impl DriveClient {
    /// A client for `cfg` with a Drive token. Uses the LAN route when it answers within 300 ms
    /// and proves it is this Drive; otherwise the internet route.
    pub fn connect(cfg: &DriveConfig, token: &str, app: AppInfo) -> Result<Self> {
        if token.trim().is_empty() {
            return Err(DriveError::Unauthorized("not signed in".into()));
        }
        let api = http::agent(Some(API_TIMEOUT));
        let route = lan::probe(cfg, token, &api).unwrap_or_else(|| Route::internet(cfg, token));
        Ok(Self { cfg: cfg.clone(), token: token.to_string(), app, api, stream: http::agent(None), route: Mutex::new(route) })
    }

    /// [`DriveClient::connect`] with the stored token; `Unauthorized` when none is stored.
    pub fn from_store(cfg: &DriveConfig, store: &dyn TokenStore, app: AppInfo) -> Result<Self> {
        let token = store.load()?.ok_or_else(|| DriveError::Unauthorized("not signed in".into()))?;
        Self::connect(cfg, &token, app)
    }

    /// True while requests go over the LAN.
    pub fn on_lan(&self) -> bool {
        self.current().lan
    }

    /// The base URL requests currently go to.
    pub fn base_url(&self) -> String {
        self.current().base
    }

    /// Probes the LAN again (for example after the network changed). Returns [`Self::on_lan`].
    pub fn refresh_route(&self) -> bool {
        let route = lan::probe(&self.cfg, &self.token, &self.api).unwrap_or_else(|| Route::internet(&self.cfg, &self.token));
        let lan = route.lan;
        *self.route.lock().unwrap_or_else(PoisonError::into_inner) = route;
        lan
    }

    pub(crate) fn current(&self) -> Route {
        self.route.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Leaves the LAN route after it failed; later calls use the internet route.
    pub(crate) fn fall_back(&self) -> Route {
        let route = Route::internet(&self.cfg, &self.token);
        *self.route.lock().unwrap_or_else(PoisonError::into_inner) = route.clone();
        route
    }

    /// The signed-in member's Drive user name, the name lock holders and teammates' updates go by.
    pub fn me(&self) -> Result<String> {
        let v: Value = http::parse(&self.call(Method::Get, "/api/me", None)?)?;
        if v.get("authenticated").and_then(Value::as_bool) == Some(false) {
            return Err(DriveError::Unauthorized("not signed in".into()));
        }
        v.get("nas_username")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .ok_or_else(|| DriveError::BadResponse("the Drive did not say who is signed in".into()))
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectSummary>> {
        http::parse(&self.call(Method::Get, "/api/team/projects", None)?)
    }

    /// Creates a team project from a FilmCraft project JSON (or an empty one).
    pub fn create_project(&self, name: &str, project: Option<&Value>) -> Result<CreatedProject> {
        let mut body = self.versions();
        body["name"] = json!(name);
        if let Some(p) = project {
            body["project"] = p.clone();
        }
        http::parse(&self.call(Method::Post, "/api/team/projects", Some(&body))?)
    }

    /// The whole project. The Drive records this as an open (it restarts the 30-day cleanup
    /// clock): use it to open or refresh a project, and [`Self::get_sequence`] /
    /// [`Self::get_shared`] to follow changes.
    pub fn get_project(&self, project: &str) -> Result<ProjectData> {
        http::parse(&self.call(Method::Get, &project_path(project)?, None)?)
    }

    /// One sequence and its revision, without recording an open (for change-feed refetches).
    /// With `etag` (from the last answer) an unchanged sequence is [`Fetched::NotModified`].
    /// [`DriveError::NotFound`] once it was deleted.
    pub fn get_sequence(&self, project: &str, seq: u64, etag: Option<&str>) -> Result<Fetched<SequencePart>> {
        self.get_cached(&seq_path(project, seq)?, etag)
    }

    /// The shared part and its revision, without recording an open (for change-feed refetches).
    pub fn get_shared(&self, project: &str, etag: Option<&str>) -> Result<Fetched<SharedPart>> {
        self.get_cached(&format!("{}/shared", project_path(project)?), etag)
    }

    /// Takes a sequence lock; [`DriveError::HeldBy`] when a teammate holds it.
    pub fn take_lock(&self, project: &str, seq: u64) -> Result<LockGrant> {
        http::parse(&self.lock_call(Method::Post, &format!("{}/lock", seq_path(project, seq)?), None)?)
    }

    /// Extends a lock (every 20 s). Returns the new expiry; [`DriveError::LockLost`] when it is gone.
    pub fn heartbeat(&self, project: &str, seq: u64, token: &str) -> Result<f64> {
        #[derive(Deserialize)]
        struct Beat {
            expires_at: f64,
        }
        let beat: Beat =
            http::parse(&self.lock_call(Method::Post, &format!("{}/lock/heartbeat", seq_path(project, seq)?), Some(&json!({ "token": token })))?)?;
        Ok(beat.expires_at)
    }

    pub fn release(&self, project: &str, seq: u64, token: &str) -> Result<()> {
        self.lock_call(Method::Delete, &format!("{}/lock", seq_path(project, seq)?), Some(&json!({ "token": token })))?;
        Ok(())
    }

    /// Drive admins only.
    pub fn force_release(&self, project: &str, seq: u64) -> Result<()> {
        self.lock_call(Method::Post, &format!("{}/lock/force", seq_path(project, seq)?), None)?;
        Ok(())
    }

    /// Saves a sequence. Returns the new revision. Errors: [`DriveError::LockLost`],
    /// [`DriveError::Stale`], [`DriveError::UpdateRequired`].
    pub fn put_sequence(&self, project: &str, seq: u64, token: &str, base_rev: u64, sequence: &Value) -> Result<u64> {
        let mut body = self.versions();
        body["token"] = json!(token);
        body["base_rev"] = json!(base_rev);
        body["json"] = sequence.clone();
        rev(&self.call(Method::Put, &seq_path(project, seq)?, Some(&body))?)
    }

    /// Deletes a sequence this member holds the lock for (the lock goes with it). Returns the
    /// sequence's last revision: revisions keep counting, so a sequence put back later with the
    /// same id is saved with this as its `base_rev`. Errors: [`DriveError::LockLost`],
    /// [`DriveError::Stale`], [`DriveError::ItemInUse`] (another sequence nests it),
    /// [`DriveError::FilesChanged`].
    pub fn delete_sequence(&self, project: &str, seq: u64, token: &str, base_rev: u64) -> Result<u64> {
        rev(&self.lock_call(Method::Delete, &seq_path(project, seq)?, Some(&json!({ "token": token, "base_rev": base_rev })))?)
    }

    /// Merges a change into the shared part. Returns the new shared revision.
    pub fn post_shared(&self, project: &str, change: &SharedChange) -> Result<u64> {
        if change.fields.values().any(Value::is_null) {
            return Err(DriveError::Invalid("a shared field can't be null".into()));
        }
        let mut body = json!({ "base_rev": change.base_rev, "upserts": change.upserts, "deletes": change.deletes });
        if let Some(bins) = &change.bins {
            body["bins"] = bins.clone();
        }
        if !change.fields.is_empty() {
            body["fields"] = json!(change.fields);
        }
        rev(&self.call(Method::Post, &format!("{}/shared", project_path(project)?), Some(&body))?)
    }

    /// Marks a project "Keep forever" (creator or admin).
    pub fn keep(&self, project: &str, keep: bool) -> Result<bool> {
        let v: Value = http::parse(&self.call(Method::Post, &format!("{}/keep", project_path(project)?), Some(&json!({ "keep": keep })))?)?;
        Ok(v.get("keep").and_then(Value::as_bool).unwrap_or(keep))
    }

    /// Resets the project's cleanup clock (the "Keep" button on a cleanup notice). Returns the
    /// last-open time.
    pub fn open(&self, project: &str) -> Result<f64> {
        let v: Value = http::parse(&self.call(Method::Post, &format!("{}/open", project_path(project)?), None)?)?;
        Ok(v.get("last_open").and_then(Value::as_f64).unwrap_or(0.0))
    }

    /// Tells the Drive an export is reading this project's media (cleanup skips it for up to 6 h).
    pub fn export_begin(&self, project: &str) -> Result<()> {
        self.call(Method::Post, &format!("/api/team/exports/{}/begin", project_id(project)?), None)?;
        Ok(())
    }

    pub fn export_end(&self, project: &str) -> Result<()> {
        self.call(Method::Post, &format!("/api/team/exports/{}/end", project_id(project)?), None)?;
        Ok(())
    }

    /// Opens the project's change feed. The iterator blocks; run it on its own thread.
    pub fn events(&self, project: &str) -> Result<EventStream> {
        let path = format!("{}/events", project_path(project)?);
        let resp = self.open_stream(&path, &[("Accept", "text/event-stream".to_string())])?;
        let status = resp.status().as_u16();
        if status != 200 {
            let (_, bytes) = http::read_body(resp)?;
            return Err(http::error_for(status, &bytes));
        }
        Ok(EventStream::new(Box::new(resp.into_body().into_reader())))
    }

    fn versions(&self) -> Value {
        json!({ "app_version": self.app.version, "schema_version": self.app.schema_version })
    }

    /// A JSON call with LAN fallback. Returns the body of a 2xx answer.
    pub(crate) fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Vec<u8>> {
        self.request(method, path, body.map_or(Body::Empty, Body::Json), &[])
    }

    /// A lock route: also names this Mac (`X-Drive-Device`).
    fn lock_call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Vec<u8>> {
        let device = self.app.device();
        let headers: Vec<(&str, String)> = if device.is_empty() { Vec::new() } else { vec![(DEVICE_HEADER, device.to_string())] };
        self.request(method, path, body.map_or(Body::Empty, Body::Json), &headers)
    }

    /// Any API call with LAN fallback. Returns the body of a 2xx answer.
    pub(crate) fn request(&self, method: Method, path: &str, body: Body<'_>, headers: &[(&str, String)]) -> Result<Vec<u8>> {
        let route = self.current();
        let (status, bytes) = match self.call_on(&route, method, path, body, headers) {
            Ok((401, _)) if route.lan => self.call_on(&self.fall_back(), method, path, body, headers).map_err(http::transport)?,
            Ok(answer) => answer,
            Err(e) if route.lan => {
                self.fall_back();
                // Resend only what is safe: reads, or a request that never left this Mac.
                if method == Method::Get || http::never_sent(&e) {
                    self.call_on(&self.current(), method, path, body, headers).map_err(http::transport)?
                } else {
                    return Err(http::transport(e));
                }
            }
            Err(e) => return Err(http::transport(e)),
        };
        if (200..300).contains(&status) { Ok(bytes) } else { Err(http::error_for(status, &bytes)) }
    }

    fn call_on(
        &self,
        route: &Route,
        method: Method,
        path: &str,
        body: Body<'_>,
        headers: &[(&str, String)],
    ) -> std::result::Result<(u16, Vec<u8>), ureq::Error> {
        let url = format!("{}{path}", route.base);
        let auth = format!("Bearer {}", route.bearer);
        // Raw bodies (upload chunks) can be large and slow: no overall timeout for them.
        let agent = if matches!(body, Body::Bytes(_)) { &self.stream } else { &self.api };
        let mut resp = match method {
            Method::Get => {
                let mut req = agent.get(&url).header("Authorization", &auth);
                for (k, v) in headers {
                    req = req.header(*k, v);
                }
                req.call()?
            }
            Method::Post => send(with_headers(agent.post(&url).header("Authorization", &auth), headers), body)?,
            Method::Put => send(with_headers(agent.put(&url).header("Authorization", &auth), headers), body)?,
            Method::Delete => send(with_headers(agent.delete(&url).header("Authorization", &auth).force_send_body(), headers), body)?,
        };
        let status = resp.status().as_u16();
        let bytes = resp.body_mut().with_config().limit(http::MAX_JSON_BYTES).read_to_vec()?;
        Ok((status, bytes))
    }

    /// A JSON GET with `If-None-Match` and LAN fallback.
    fn get_cached<T: serde::de::DeserializeOwned>(&self, path: &str, etag: Option<&str>) -> Result<Fetched<T>> {
        let headers: Vec<(&str, String)> = etag.map(|e| vec![("If-None-Match", e.to_string())]).unwrap_or_default();
        let route = self.current();
        let resp = match self.get_on(&route, path, &headers) {
            Ok(resp) if route.lan && resp.status().as_u16() == 401 => self.get_on(&self.fall_back(), path, &headers).map_err(http::transport)?,
            Ok(resp) => resp,
            Err(_) if route.lan => self.get_on(&self.fall_back(), path, &headers).map_err(http::transport)?,
            Err(e) => return Err(http::transport(e)),
        };
        let tag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
        let (status, bytes) = http::read_body(resp)?;
        match status {
            304 => Ok(Fetched::NotModified),
            200..=299 => Ok(Fetched::Fresh { value: http::parse(&bytes)?, etag: tag }),
            _ => Err(http::error_for(status, &bytes)),
        }
    }

    fn get_on(&self, route: &Route, path: &str, headers: &[(&str, String)]) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let mut req = self.api.get(format!("{}{path}", route.base)).header("Authorization", format!("Bearer {}", route.bearer));
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        req.call()
    }

    /// A streaming GET (event feed, downloads) with LAN fallback. Any status is returned.
    pub(crate) fn open_stream(&self, path_and_query: &str, headers: &[(&str, String)]) -> Result<ureq::http::Response<ureq::Body>> {
        let route = self.current();
        match self.stream_on(&route, path_and_query, headers) {
            Ok(resp) if route.lan && resp.status().as_u16() == 401 => self.stream_on(&self.fall_back(), path_and_query, headers).map_err(http::transport),
            Ok(resp) => Ok(resp),
            Err(_) if route.lan => self.stream_on(&self.fall_back(), path_and_query, headers).map_err(http::transport),
            Err(e) => Err(http::transport(e)),
        }
    }

    fn stream_on(&self, route: &Route, path_and_query: &str, headers: &[(&str, String)]) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let mut req = self.stream.get(format!("{}{path_and_query}", route.base)).header("Authorization", format!("Bearer {}", route.bearer));
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        req.call()
    }
}

/// A request body.
#[derive(Clone, Copy)]
pub(crate) enum Body<'a> {
    Empty,
    Json(&'a Value),
    /// `application/x-www-form-urlencoded` fields.
    Form(&'a [(&'a str, &'a str)]),
    /// Raw octets.
    Bytes(&'a [u8]),
}

fn with_headers(mut req: ureq::RequestBuilder<ureq::typestate::WithBody>, headers: &[(&str, String)]) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    req
}

fn send(req: ureq::RequestBuilder<ureq::typestate::WithBody>, body: Body<'_>) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    match body {
        Body::Json(v) => req.header("Content-Type", "application/json").send(v.to_string().as_bytes()),
        Body::Form(fields) => req.send_form(fields.iter().copied()),
        Body::Bytes(bytes) => req.header("Content-Type", "application/octet-stream").send(bytes),
        Body::Empty => req.send_empty(),
    }
}

fn rev(bytes: &[u8]) -> Result<u64> {
    #[derive(Deserialize)]
    struct Rev {
        rev: u64,
    }
    Ok(http::parse::<Rev>(bytes)?.rev)
}

/// Project ids are 32 lowercase hex digits (`uuid4().hex`); anything else never reaches a URL.
fn project_id(id: &str) -> Result<&str> {
    if id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        Ok(id)
    } else {
        Err(DriveError::Invalid(format!("project id {id:?}")))
    }
}

pub(crate) fn project_path(id: &str) -> Result<String> {
    Ok(format!("/api/team/projects/{}", project_id(id)?))
}

fn seq_path(project: &str, seq: u64) -> Result<String> {
    if !(1..=MAX_ID).contains(&seq) {
        return Err(DriveError::Invalid(format!("sequence id {seq}")));
    }
    Ok(format!("{}/sequences/{seq}", project_path(project)?))
}
