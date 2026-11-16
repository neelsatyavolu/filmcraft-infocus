//! The campus fast path. The Drive answers `GET /api/lan/proof` only on its direct LAN address, and
//! the internet route checks that proof before it hands out a short-lived LAN token (`ifl_`), so the
//! long-lived `ifd_` token never crosses the LAN in plain HTTP and a look-alike host gets nothing.

use std::time::Duration;

use serde::Deserialize;

use crate::auth::random_string;
use crate::config::DriveConfig;
use crate::http;

/// How long the LAN host may take to answer before the internet route is used.
pub const LAN_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// Prefix of a LAN-only token.
pub const LAN_TOKEN_PREFIX: &str = "ifl_";

/// Where requests go and with which bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    pub base: String,
    pub bearer: String,
    pub lan: bool,
}

impl Route {
    pub fn internet(cfg: &DriveConfig, token: &str) -> Self {
        Self { base: cfg.drive_url.clone(), bearer: token.to_string(), lan: false }
    }
}

#[derive(Deserialize)]
struct Proof {
    proof: String,
}

#[derive(Deserialize)]
struct LanToken {
    token: String,
}

/// The LAN route, when `cfg.lan_url` answers within [`LAN_PROBE_TIMEOUT`] and the Drive accepts its
/// proof. `None` sends everything over `drive_url`.
pub(crate) fn probe(cfg: &DriveConfig, token: &str, api: &ureq::Agent) -> Option<Route> {
    let lan = cfg.lan_url.as_deref()?;
    let nonce = random_string().ok()?;
    let quick = http::agent_with_connect(Some(LAN_PROBE_TIMEOUT), LAN_PROBE_TIMEOUT);
    let resp = quick.get(format!("{lan}/api/lan/proof")).query("nonce", &nonce).call().ok()?;
    let (status, bytes) = http::read_body(resp).ok()?;
    if status != 200 {
        return None;
    }
    let proof: Proof = serde_json::from_slice(&bytes).ok()?;
    let form = url::form_urlencoded::Serializer::new(String::new()).append_pair("nonce", &nonce).append_pair("proof", &proof.proof).finish();
    let resp = api
        .post(format!("{}/api/cli/lan-token", cfg.drive_url))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(form.as_bytes())
        .ok()?;
    let (status, bytes) = http::read_body(resp).ok()?;
    if status != 200 {
        return None;
    }
    let lan_token: LanToken = serde_json::from_slice(&bytes).ok()?;
    lan_token.token.starts_with(LAN_TOKEN_PREFIX).then(|| Route { base: lan.to_string(), bearer: lan_token.token, lan: true })
}
