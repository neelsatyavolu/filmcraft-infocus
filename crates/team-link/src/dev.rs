//! Development overrides, in debug builds only, so a test script can run two copies of InFocus Cut
//! on one Mac against a local development Drive without a browser or a shared login:
//!
//! - `INFOCUS_DRIVE_TOKEN_FILE=<path>`: the Drive token is read from (and kept in) this file
//!   instead of the Keychain.
//! - `FILMCRAFT_DATA_DIR=<dir>`: each data folder signs in under its own Keychain account and keeps
//!   its team media in `<dir>/team-media`, so two copies never share a login or a media cache.
//!
//! A release build ignores both and always uses the Keychain account `drive` and
//! `~/Library/Caches/InFocus Cut/media`.

use std::path::PathBuf;
use std::sync::Arc;

use filmcraft_drive::{KeychainStore, TokenStore};

/// The token file override (debug builds).
pub const TOKEN_FILE_ENV: &str = "INFOCUS_DRIVE_TOKEN_FILE";
/// The data folder override shared with the rest of the app.
pub const DATA_DIR_ENV: &str = "FILMCRAFT_DATA_DIR";

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Where the app keeps the Drive token.
pub fn token_store() -> Arc<dyn TokenStore> {
    if cfg!(debug_assertions) {
        if let Some(file) = env_path(TOKEN_FILE_ENV) {
            return Arc::new(FileTokenStore(file));
        }
        if let Some(dir) = env_path(DATA_DIR_ENV) {
            return Arc::new(KeychainStore::with_account(&account_for(&dir)));
        }
    }
    Arc::new(KeychainStore::default())
}

/// The team media cache of a development data folder (debug builds), else `None`.
pub fn cache_dir() -> Option<PathBuf> {
    if cfg!(debug_assertions) { env_path(DATA_DIR_ENV).map(|d| d.join("team-media")) } else { None }
}

/// `drive-<16 hex digits of the folder's SHA-256>`: one Keychain account per data folder.
fn account_for(dir: &std::path::Path) -> String {
    let path = dir.to_string_lossy();
    let hex = filmcraft_drive::sha256_hex(&mut path.as_bytes()).unwrap_or_default();
    format!("drive-{}", hex.get(..16).unwrap_or("dev"))
}

/// A token kept in a plain file (development only; mode 0600 on Unix).
pub struct FileTokenStore(pub PathBuf);

impl TokenStore for FileTokenStore {
    fn load(&self) -> filmcraft_drive::Result<Option<String>> {
        match std::fs::read_to_string(&self.0) {
            Ok(text) => Ok(Some(text.trim().to_string()).filter(|t| !t.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, token: &str) -> filmcraft_drive::Result<()> {
        if let Some(dir) = self.0.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.0, token)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    fn clear(&self) -> filmcraft_drive::Result<()> {
        match std::fs::remove_file(&self.0) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore(dir.path().join("sub/token"));
        assert_eq!(store.load().unwrap(), None);
        store.save("ifd_abc").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("ifd_abc"));
        std::fs::write(&store.0, "ifd_abc\n").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("ifd_abc"), "a trailing newline is ignored");
        store.clear().unwrap();
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn each_data_folder_has_its_own_account() {
        let a = account_for(std::path::Path::new("/tmp/member-1"));
        let b = account_for(std::path::Path::new("/tmp/member-2"));
        assert_ne!(a, b);
        assert!(a.starts_with("drive-") && a.len() == 22, "{a}");
    }
}
