//! Shared HTTP plumbing: the ureq agent, body limits and mapping server refusals to [`DriveError`].

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::error::{DriveError, Result};

/// Largest JSON answer read (a whole project with every sequence).
pub(crate) const MAX_JSON_BYTES: u64 = 512 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A blocking agent: pure-Rust TLS with OS certificate checks, HTTP errors returned as responses,
/// and no redirects (a redirect would carry the bearer token to another origin).
pub(crate) fn agent(global_timeout: Option<Duration>) -> ureq::Agent {
    agent_with_connect(global_timeout, CONNECT_TIMEOUT)
}

pub(crate) fn agent_with_connect(global_timeout: Option<Duration>, connect: Duration) -> ureq::Agent {
    let provider = Arc::new(rustls_rustcrypto::provider());
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .unversioned_rustls_crypto_provider(provider)
                .build(),
        )
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .timeout_connect(Some(connect))
        .timeout_global(global_timeout)
        .user_agent("InFocus Cut")
        .build()
        .into()
}

/// A transport failure (no HTTP answer).
pub(crate) fn transport(e: ureq::Error) -> DriveError {
    DriveError::Network(e.to_string())
}

/// True when the request certainly never reached the server, so it is safe to send again elsewhere.
pub(crate) fn never_sent(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
        ureq::Error::Timeout(t) => matches!(t, ureq::Timeout::Connect | ureq::Timeout::Resolve),
        ureq::Error::Io(io) => {
            matches!(io.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::HostUnreachable | std::io::ErrorKind::NetworkUnreachable)
        }
        _ => false,
    }
}

/// Reads the whole (bounded) body.
pub(crate) fn read_body(mut resp: ureq::http::Response<ureq::Body>) -> Result<(u16, Vec<u8>)> {
    let status = resp.status().as_u16();
    let bytes = resp.body_mut().with_config().limit(MAX_JSON_BYTES).read_to_vec().map_err(transport)?;
    Ok((status, bytes))
}

/// The human message in an error body: FastAPI's `detail` or the team API's `error`.
pub(crate) fn message(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    ["detail", "error"].iter().find_map(|k| v.get(*k).and_then(Value::as_str).map(str::to_string))
}

/// Maps a non-2xx answer to an error.
pub(crate) fn error_for(status: u16, body: &[u8]) -> DriveError {
    let msg = message(body).unwrap_or_else(|| format!("HTTP {status}"));
    match status {
        401 => DriveError::Unauthorized(msg),
        403 => DriveError::Forbidden(msg),
        404 => DriveError::NotFound(msg),
        409 => conflict(body).unwrap_or(DriveError::Http { status, message: msg }),
        426 => {
            let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            DriveError::UpdateRequired {
                min_app_version: v.get("min_app_version").and_then(Value::as_str).unwrap_or("a newer version").to_string(),
                schema_version: v.get("schema_version").and_then(Value::as_u64).unwrap_or(0),
            }
        }
        _ => DriveError::Http { status, message: msg },
    }
}

/// The team API's 409 bodies: `{holder, expires_at}` (lock taken), `{error: "lock lost"}`,
/// `{error: "stale", rev}`, `{error: "item is used by a sequence", item}` and the hash check's
/// `{error: "project files were changed outside InFocus Cut"}`.
fn conflict(body: &[u8]) -> Option<DriveError> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object()?;
    if obj.contains_key("holder") {
        return Some(DriveError::HeldBy {
            holder: obj.get("holder").and_then(Value::as_str).map(str::to_string),
            expires_at: obj.get("expires_at").and_then(Value::as_f64).unwrap_or(0.0),
        });
    }
    let error = obj.get("error").and_then(Value::as_str)?;
    Some(match error {
        "lock lost" => DriveError::LockLost,
        "stale" => DriveError::Stale { rev: obj.get("rev").and_then(Value::as_u64).unwrap_or(0) },
        "item is used by a sequence" => DriveError::ItemInUse { item: obj.get("item").and_then(Value::as_u64).unwrap_or(0) },
        e if e.contains("changed outside") => DriveError::FilesChanged(e.to_string()),
        _ => return None,
    })
}

/// Parses a 2xx JSON body.
pub(crate) fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| DriveError::BadResponse(e.to_string()))
}
