//! Where InFocus Drive is. Hosts come from a config file or the environment, never from source.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{DriveError, Result};

/// Environment variable (read at run time, then at build time) naming the Drive when no config file exists.
pub const DRIVE_URL_ENV: &str = "INFOCUS_DRIVE_URL";
/// Environment variable naming the project dashboard website (else the config file's
/// `dashboard_url`, else [`DEFAULT_DASHBOARD_URL`]).
pub const DASHBOARD_URL_ENV: &str = "INFOCUS_EDITING_URL";
/// The team project dashboard members open from the Team panel.
pub const DEFAULT_DASHBOARD_URL: &str = "https://editing.infocuspaly.com";
/// Largest config file read.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Where the Drive lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveConfig {
    /// The Drive over the internet (`https://…`; plain `http` only for a loopback test server).
    pub drive_url: String,
    /// The same Drive on the local network, preferred when it answers quickly.
    pub lan_url: Option<String>,
    /// The team project dashboard website ("Open Dashboard" in the Team panel).
    pub dashboard_url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    drive_url: String,
    #[serde(default)]
    lan_url: Option<String>,
    #[serde(default)]
    dashboard_url: Option<String>,
}

impl DriveConfig {
    /// Builds a config from URLs, checking and normalising them.
    pub fn new(drive_url: &str, lan_url: Option<&str>) -> Result<Self> {
        let drive_url = normalize(drive_url, false)?;
        let lan_url = match lan_url.map(str::trim).filter(|s| !s.is_empty()) {
            Some(lan) => Some(normalize(lan, true)?),
            None => None,
        };
        Ok(Self { drive_url, lan_url, dashboard_url: DEFAULT_DASHBOARD_URL.into() })
    }

    /// The same config with another dashboard address (checked like the Drive's).
    pub fn with_dashboard(self, url: &str) -> Result<Self> {
        Ok(Self { dashboard_url: normalize(url, false)?, ..self })
    }

    /// `~/Library/Application Support/InFocus Cut/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join("Library/Application Support/InFocus Cut/config.toml"))
    }

    /// Loads the config file if present, else `INFOCUS_DRIVE_URL` from the environment, else the
    /// value baked in at build time.
    pub fn load() -> Result<Self> {
        let env = std::env::var(DRIVE_URL_ENV).ok().or_else(|| option_env!("INFOCUS_DRIVE_URL").map(str::to_string));
        let cfg = Self::load_from(Self::default_path().as_deref(), env.as_deref())?;
        match std::env::var(DASHBOARD_URL_ENV).ok().filter(|u| !u.trim().is_empty()) {
            Some(url) => cfg.with_dashboard(&url),
            None => Ok(cfg),
        }
    }

    /// [`DriveConfig::load`] with the file path and environment value given (for tests).
    pub fn load_from(path: Option<&Path>, env_url: Option<&str>) -> Result<Self> {
        if let Some(path) = path.filter(|p| p.is_file()) {
            return Self::from_file(path);
        }
        match env_url.map(str::trim).filter(|s| !s.is_empty()) {
            Some(url) => Self::new(url, None),
            None => Err(DriveError::Config(format!(
                "no Drive address: create {} or set {DRIVE_URL_ENV}",
                Self::default_path().map(|p| p.display().to_string()).unwrap_or_else(|| "config.toml".into())
            ))),
        }
    }

    /// Reads a TOML config file (`drive_url`, optional `lan_url` and `dashboard_url`).
    pub fn from_file(path: &Path) -> Result<Self> {
        let len = std::fs::metadata(path)?.len();
        if len > MAX_CONFIG_BYTES {
            return Err(DriveError::Config(format!("{} is too large", path.display())));
        }
        let text = std::fs::read_to_string(path)?;
        Self::from_toml(&text).map_err(|e| DriveError::Config(format!("{}: {e}", path.display())))
    }

    /// Parses config TOML text.
    pub fn from_toml(text: &str) -> Result<Self> {
        let file: ConfigFile = toml::from_str(text).map_err(|e| DriveError::Config(e.to_string()))?;
        let cfg = Self::new(&file.drive_url, file.lan_url.as_deref())?;
        match file.dashboard_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            Some(url) => cfg.with_dashboard(url),
            None => Ok(cfg),
        }
    }
}

/// Checks a base URL and returns it without a trailing slash. `allow_http` permits plain HTTP on any
/// host (the LAN route); otherwise plain HTTP is accepted only for loopback test servers.
fn normalize(raw: &str, allow_http: bool) -> Result<String> {
    let parsed = url::Url::parse(raw.trim()).map_err(|e| DriveError::Config(format!("{raw:?} is not a URL ({e})")))?;
    let host = parsed.host_str().unwrap_or_default();
    if host.is_empty() {
        return Err(DriveError::Config(format!("{raw:?} has no host")));
    }
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "[::1]";
    match parsed.scheme() {
        "https" => {}
        "http" if allow_http || loopback => {}
        other => return Err(DriveError::Config(format!("{raw:?}: use https (got {other})"))),
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(DriveError::Config(format!("{raw:?}: give only the scheme, host, port and path")));
    }
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}
