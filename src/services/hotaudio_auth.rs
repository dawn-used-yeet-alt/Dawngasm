//! Cloudflare clearance for hotaudio.
//!
//! hotaudio sits behind Cloudflare. A normal HTTP client cannot answer an
//! interactive challenge, but a browser (or the system webview) can. Cloudflare
//! then issues a `cf_clearance` cookie, and the cookie only works when the
//! request also carries the exact `User-Agent` that earned it. This module is
//! the on-disk pair, shared by the webview capture and the extractor.
//!
//! The environment variables mirror the upstream CLI (`HOTAUDIO_COOKIE` and
//! `HOTAUDIO_USER_AGENT`) so a shell setup can override the saved values
//! without rewriting the file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Raw `Cookie` header value, normally `cf_clearance=...`.
pub const COOKIE_ENV: &str = "HOTAUDIO_COOKIE";
/// The exact browser `User-Agent` that earned the clearance.
pub const USER_AGENT_ENV: &str = "HOTAUDIO_USER_AGENT";

const FILE_NAME: &str = "hotaudio-clearance.json";

/// A Cloudflare clearance cookie and the user agent it is bound to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotaudioClearance {
    #[serde(default)]
    pub cookie: String,
    #[serde(default)]
    pub user_agent: String,
}

impl HotaudioClearance {
    #[must_use]
    pub fn new(cookie: impl Into<String>, user_agent: impl Into<String>) -> Self {
        Self {
            cookie: cookie.into(),
            user_agent: user_agent.into(),
        }
    }

    /// Reads the saved pair and applies the environment overrides.
    #[must_use]
    pub fn load(dir: &Path) -> Self {
        let mut clearance = Self::read(dir).unwrap_or_default();
        if let Some(cookie) = non_empty_env(COOKIE_ENV) {
            clearance.cookie = cookie;
        }
        if let Some(user_agent) = non_empty_env(USER_AGENT_ENV) {
            clearance.user_agent = user_agent;
        }
        clearance
    }

    /// The saved clearance, or `None` when no cookie is set. Used by the
    /// extractor, which must work without a clearance on a clean network.
    #[must_use]
    pub fn load_default() -> Option<Self> {
        let dir = crate::services::storage::Storage::default_dir().ok()?;
        let clearance = Self::load(&dir);
        clearance.has_cookie().then_some(clearance)
    }

    /// Writes the pair atomically, with owner-only permissions on Unix because
    /// the cookie is a credential.
    ///
    /// # Errors
    ///
    /// Returns an error if the data directory cannot be created or the file
    /// cannot be written.
    pub fn save(&self, dir: &Path) -> io::Result<PathBuf> {
        fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        fs::write(&tmp, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Removes the saved pair, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists and cannot be removed.
    pub fn clear(dir: &Path) -> io::Result<()> {
        match fs::remove_file(dir.join(FILE_NAME)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cookie.trim().is_empty() && self.user_agent.trim().is_empty()
    }

    /// The cookie is the load-bearing half; the user agent only has to match
    /// it. A file with a cookie and no user agent is still usable with the
    /// upstream default.
    #[must_use]
    pub fn has_cookie(&self) -> bool {
        !self.cookie.trim().is_empty()
    }

    fn read(dir: &Path) -> Option<Self> {
        let data = fs::read_to_string(dir.join(FILE_NAME)).ok()?;
        serde_json::from_str(&data).ok()
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dawngasm-hotaudio-auth-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn clearance_round_trips_and_clears() {
        let dir = temp_dir("round-trip");
        let clearance = HotaudioClearance::new("cf_clearance=abc", "Mozilla/5.0 (X11)");
        let path = clearance.save(&dir).unwrap();
        assert_eq!(path.file_name().unwrap(), FILE_NAME);
        assert_eq!(HotaudioClearance::read(&dir), Some(clearance));
        HotaudioClearance::clear(&dir).unwrap();
        assert_eq!(HotaudioClearance::read(&dir), None);
        // Clearing twice is fine.
        HotaudioClearance::clear(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_clearance_is_not_offered_to_the_extractor() {
        assert!(HotaudioClearance::default().is_empty());
        assert!(!HotaudioClearance::default().has_cookie());
        assert!(!HotaudioClearance::new("  ", "  ").has_cookie());
        assert!(HotaudioClearance::new("cf_clearance=x", "").has_cookie());
    }

    #[test]
    fn malformed_files_read_as_absent() {
        let dir = temp_dir("malformed");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE_NAME), "not json").unwrap();
        assert_eq!(HotaudioClearance::read(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
