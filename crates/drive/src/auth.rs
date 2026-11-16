//! Sign-in: the browser approves this Mac (PKCE, the same flow as the `infocus` CLI) and the
//! `ifd_` token is kept in the Keychain.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::DriveConfig;
use crate::error::{DriveError, Result};
use crate::http;

/// Keychain service and account for the Drive token.
pub const KEYCHAIN_SERVICE: &str = "InFocus Cut";
pub const KEYCHAIN_ACCOUNT: &str = "drive";
/// Prefix of a long-lived Drive token.
pub const TOKEN_PREFIX: &str = "ifd_";
/// How long sign-in waits for the browser by default.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_CALLBACK_REQUEST: usize = 16 * 1024;

/// Where the Drive token is kept. The app uses [`KeychainStore`]; tests use [`MemoryStore`].
pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<String>>;
    fn save(&self, token: &str) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

/// An in-memory token store (tests, and a session that must not touch the Keychain).
#[derive(Debug, Default)]
pub struct MemoryStore(Mutex<Option<String>>);

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryStore {
    fn load(&self) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap_or_else(PoisonError::into_inner).clone())
    }
    fn save(&self, token: &str) -> Result<()> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(token.to_string());
        Ok(())
    }
    fn clear(&self) -> Result<()> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }
}

/// The macOS Keychain (generic password, service `InFocus Cut`, account `drive` by default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeychainStore {
    account: String,
}

impl Default for KeychainStore {
    fn default() -> Self {
        Self { account: KEYCHAIN_ACCOUNT.to_string() }
    }
}

impl KeychainStore {
    /// The token under another Keychain account (a development copy of the app with its own data
    /// folder signs in separately).
    pub fn with_account(account: &str) -> Self {
        Self { account: account.to_string() }
    }
    pub fn account(&self) -> &str {
        &self.account
    }
}

#[cfg(target_os = "macos")]
impl TokenStore for KeychainStore {
    fn load(&self) -> Result<Option<String>> {
        use security_framework::passwords::get_generic_password;
        const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
        match get_generic_password(KEYCHAIN_SERVICE, &self.account) {
            Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| DriveError::Keychain("stored token is not text".into())),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(DriveError::Keychain(e.to_string())),
        }
    }
    fn save(&self, token: &str) -> Result<()> {
        security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, &self.account, token.as_bytes()).map_err(|e| DriveError::Keychain(e.to_string()))
    }
    fn clear(&self) -> Result<()> {
        const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
        match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, &self.account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(DriveError::Keychain(e.to_string())),
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl TokenStore for KeychainStore {
    fn load(&self) -> Result<Option<String>> {
        Err(DriveError::Keychain("the Keychain is only available on macOS".into()))
    }
    fn save(&self, _token: &str) -> Result<()> {
        Err(DriveError::Keychain("the Keychain is only available on macOS".into()))
    }
    fn clear(&self) -> Result<()> {
        Err(DriveError::Keychain("the Keychain is only available on macOS".into()))
    }
}

/// 32 random bytes, base64url without padding (43 characters).
pub fn random_string() -> Result<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| DriveError::Login(format!("no system randomness: {e}")))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

/// The PKCE S256 challenge for a verifier (RFC 7636 §4.2).
pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Sign-in options.
#[derive(Debug, Clone)]
pub struct LoginOptions {
    /// Shown on the Drive's consent page and in its list of signed-in devices.
    pub device: String,
    /// How long to wait for the browser.
    pub timeout: Duration,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self { device: "InFocus Cut".into(), timeout: LOGIN_TIMEOUT }
    }
}

/// A completed sign-in.
#[derive(Debug, Clone, Deserialize)]
pub struct SignedIn {
    pub token: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub device: String,
}

/// Signs in through the browser and stores the token. `open_browser` gets the `/cli/authorize` URL.
/// Blocks until the browser answers or `opts.timeout` passes: run it off the UI thread.
pub fn login(cfg: &DriveConfig, store: &dyn TokenStore, opts: &LoginOptions, open_browser: &dyn Fn(&str)) -> Result<SignedIn> {
    let state = random_string()?;
    let verifier = random_string()?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| DriveError::Login(format!("start the local sign-in listener: {e}")))?;
    let port = listener.local_addr()?.port();
    let mut authorize = url::Url::parse(&format!("{}/cli/authorize", cfg.drive_url)).map_err(|e| DriveError::Config(e.to_string()))?;
    authorize
        .query_pairs_mut()
        .append_pair("port", &port.to_string())
        .append_pair("state", &state)
        .append_pair("challenge", &pkce_challenge(&verifier))
        .append_pair("device", &opts.device);
    open_browser(authorize.as_str());
    let code = wait_for_code(&listener, &state, opts.timeout)?;
    let signed = exchange(cfg, &code, &verifier)?;
    store.save(&signed.token)?;
    Ok(signed)
}

/// Revokes the stored token on the Drive (best effort) and removes it from the store.
pub fn logout(cfg: &DriveConfig, store: &dyn TokenStore) -> Result<()> {
    if let Some(token) = store.load()? {
        let agent = http::agent(Some(Duration::from_secs(10)));
        // Revocation failing (offline, already revoked) must not keep the token on this Mac.
        let _ = agent.post(format!("{}/api/cli/logout", cfg.drive_url)).header("Authorization", format!("Bearer {token}")).send_empty();
    }
    store.clear()
}

fn exchange(cfg: &DriveConfig, code: &str, verifier: &str) -> Result<SignedIn> {
    let body = serde_json::json!({ "code": code, "verifier": verifier });
    let agent = http::agent(Some(Duration::from_secs(30)));
    let resp = agent
        .post(format!("{}/api/cli/token", cfg.drive_url))
        .header("Content-Type", "application/json")
        .send(body.to_string().as_bytes())
        .map_err(http::transport)?;
    let (status, bytes) = http::read_body(resp)?;
    if status != 200 {
        return Err(DriveError::Login(http::message(&bytes).unwrap_or_else(|| format!("HTTP {status}"))));
    }
    let signed: SignedIn = serde_json::from_slice(&bytes).map_err(|e| DriveError::BadResponse(e.to_string()))?;
    if !signed.token.starts_with(TOKEN_PREFIX) {
        return Err(DriveError::BadResponse("the Drive did not return a sign-in token".into()));
    }
    Ok(signed)
}

/// Waits for `GET /callback?code=…&state=…` on the loopback listener.
fn wait_for_code(listener: &TcpListener, state: &str, timeout: Duration) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(outcome) = handle_callback(stream, state) {
                    return outcome;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(DriveError::Login("timed out waiting for approval in the browser".into()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(DriveError::Login(format!("local sign-in listener: {e}"))),
        }
    }
}

/// Handles one browser request. `None` means "not our callback, keep waiting".
fn handle_callback(mut stream: TcpStream, state: &str) -> Option<Result<String>> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let target = read_request_target(&mut stream)?;
    let parsed = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if parsed.path() != "/callback" {
        respond(&mut stream, 404, "Not found.");
        return None;
    }
    let query = |key: &str| parsed.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned());
    if !constant_time_eq(query("state").unwrap_or_default().as_bytes(), state.as_bytes()) {
        respond(&mut stream, 400, "This sign-in link doesn't match InFocus Cut. Sign in again from the app.");
        return None;
    }
    if query("error").as_deref() == Some("access_denied") {
        respond(&mut stream, 200, "Denied. InFocus Cut was not signed in; you can close this tab.");
        return Some(Err(DriveError::Login("access was denied in the browser".into())));
    }
    match query("code").filter(|c| !c.is_empty()) {
        Some(code) => {
            respond(&mut stream, 200, "InFocus Cut is signed in. You can close this tab.");
            Some(Ok(code))
        }
        None => {
            respond(&mut stream, 400, "Sign-in failed. Sign in again from the app.");
            Some(Err(DriveError::Login("the browser did not return a sign-in code".into())))
        }
    }
}

fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(2).any(|w| w == b"\r\n") {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 || buf.len() > MAX_CALLBACK_REQUEST {
            return None;
        }
        buf.extend_from_slice(chunk.get(..n)?);
    }
    let line = String::from_utf8_lossy(&buf);
    let mut parts = line.lines().next()?.split(' ');
    (parts.next()? == "GET").then_some(())?;
    parts.next().filter(|t| t.starts_with('/')).map(str::to_string)
}

fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>InFocus Cut</title>\
         <body style=\"font:16px system-ui;margin:15vh auto;max-width:420px;text-align:center\">\
         <h1 style=\"font-size:20px\">InFocus Cut</h1><p>{message}</p></body>"
    );
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
         Referrer-Policy: no-referrer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body.as_bytes()));
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
