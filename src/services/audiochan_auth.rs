//! Playback clearance for audiochan.
//!
//! audiochan protects playback and stream URLs with Cloudflare Turnstile.
//! A browser (or the system webview) completes the verification check,
//! receiving the `__Secure-audiochan_playback` cookie. This module manages
//! the on-disk clearance pair (cookies and User-Agent) shared between the
//! webview capture and the audiochan extractor.
//!
//! `AUDIOCHAN_COOKIE` and `AUDIOCHAN_USER_AGENT` environment variables allow
//! overriding the saved values for a single run without altering the disk file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Raw `Cookie` header value, including `__Secure-audiochan_playback=...`.
pub const COOKIE_ENV: &str = "AUDIOCHAN_COOKIE";
/// The exact browser `User-Agent` used during clearance verification.
pub const USER_AGENT_ENV: &str = "AUDIOCHAN_USER_AGENT";

const FILE_NAME: &str = "audiochan-clearance.json";

/// An audiochan playback clearance cookie and the user agent it was issued to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudiochanClearance {
    #[serde(default)]
    pub cookie: String,
    #[serde(default)]
    pub user_agent: String,
}

impl AudiochanClearance {
    #[must_use]
    pub fn new(cookie: impl Into<String>, user_agent: impl Into<String>) -> Self {
        Self {
            cookie: cookie.into(),
            user_agent: user_agent.into(),
        }
    }

    /// Reads the saved pair and applies environment overrides if present.
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

    /// The saved clearance, or `None` when no cookie is set.
    #[must_use]
    pub fn load_default() -> Option<Self> {
        let dir = crate::services::storage::Storage::default_dir().ok()?;
        let clearance = Self::load(&dir);
        clearance.has_cookie().then_some(clearance)
    }

    /// Writes the pair atomically with owner-only permissions on Unix.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the file cannot
    /// be written.
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

    /// Removes the saved clearance file, if present.
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
        let dir = std::env::temp_dir().join(format!("dawngasm-audiochan-auth-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn clearance_round_trips_and_clears() {
        let dir = temp_dir("round-trip");
        let clearance =
            AudiochanClearance::new("__Secure-audiochan_playback=abc", "Mozilla/5.0 (X11)");
        let path = clearance.save(&dir).unwrap();
        assert_eq!(path.file_name().unwrap(), FILE_NAME);
        assert_eq!(AudiochanClearance::read(&dir), Some(clearance));
        AudiochanClearance::clear(&dir).unwrap();
        assert_eq!(AudiochanClearance::read(&dir), None);
        AudiochanClearance::clear(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_files_read_as_absent() {
        let dir = temp_dir("malformed");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(FILE_NAME), "not json").unwrap();
        assert_eq!(AudiochanClearance::read(&dir), None);
        assert_eq!(
            AudiochanClearance::load(&dir),
            AudiochanClearance::default()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_clearance_is_not_offered_to_the_extractor() {
        let empty = AudiochanClearance::default();
        assert!(!empty.has_cookie());
        assert!(empty.is_empty());
        let whitespace = AudiochanClearance::new("   ", "   ");
        assert!(!whitespace.has_cookie());
        assert!(whitespace.is_empty());
        let cookie_only = AudiochanClearance::new("__Secure-audiochan_playback=x", "");
        assert!(cookie_only.has_cookie());
        assert!(!cookie_only.is_empty());
    }
}
