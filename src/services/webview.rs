//! The Cloudflare clearance extension for hotaudio and audiochan.
//!
//! The webview itself ships as `dawngasm-plugin-clearance`, a separate
//! executable. Keeping it out of the TUI means the player never links
//! `WebKitGTK`, `WKWebView` or `WebView2`, so it starts on machines without a
//! webview runtime. Pressing `C` is the only action that needs the plugin.
//!
//! The plugin answers `clearance.capture` with the clearance cookie and
//! the exact user agent that earned it; this module saves the pair for either
//! hotaudio or audiochan depending on the requested target.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;

use crate::services::audiochan_auth::AudiochanClearance;
use crate::services::extensions::{Extensions, CAP_CLEARANCE, METHOD_CLEARANCE_CAPTURE};
use crate::services::hotaudio_auth::HotaudioClearance;
use crate::services::storage::Storage;

/// The host requiring verification clearance.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ClearanceTarget {
    #[default]
    Hotaudio,
    Audiochan,
}

impl ClearanceTarget {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hotaudio => "hotaudio",
            Self::Audiochan => "audiochan",
        }
    }
}

#[derive(Deserialize)]
struct Captured {
    cookie: String,
    #[serde(default)]
    user_agent: String,
}

/// Opens the clearance plugin for `target`, waits for verification, and saves the pair.
///
/// # Errors
///
/// Returns an error when no clearance extension is installed, the plugin fails,
/// or the clearance cannot be saved.
pub fn capture_and_save_target(target: ClearanceTarget) -> Result<PathBuf, String> {
    let extensions = Extensions::global();
    let plugin = extensions
        .find_for_method(CAP_CLEARANCE, METHOD_CLEARANCE_CAPTURE)
        .ok_or_else(|| {
            extensions
                .failure(CAP_CLEARANCE)
                .map_or_else(
                    || {
                        "no Cloudflare clearance extension is installed; put `dawngasm-plugin-clearance` in the plugins directory next to the app".to_string()
                    },
                    |failure| runtime_hint(&failure.error),
                )
        })?;
    let result = plugin
        .call(
            METHOD_CLEARANCE_CAPTURE,
            json!({ "target": target.as_str() }),
        )
        .map_err(|error| error.to_string())?;
    let captured: Captured = serde_json::from_value(result)
        .map_err(|error| format!("the clearance extension returned an invalid result: {error}"))?;
    if captured.cookie.trim().is_empty() {
        return Err("the clearance extension returned an empty cookie".to_string());
    }
    let dir = Storage::default_dir()
        .map_err(|error| format!("could not find the data directory: {error}"))?;
    match target {
        ClearanceTarget::Hotaudio => HotaudioClearance::new(captured.cookie, captured.user_agent)
            .save(&dir)
            .map_err(|error| format!("could not save hotaudio clearance: {error}")),
        ClearanceTarget::Audiochan => AudiochanClearance::new(captured.cookie, captured.user_agent)
            .save(&dir)
            .map_err(|error| format!("could not save audiochan clearance: {error}")),
    }
}

/// Opens the clearance plugin for hotaudio (backwards compatibility).
///
/// # Errors
///
/// See [`capture_and_save_target`].
pub fn capture_and_save() -> Result<PathBuf, String> {
    capture_and_save_target(ClearanceTarget::Hotaudio)
}

/// Runs the clearance extension for `target` and discards the saved path.
///
/// # Errors
///
/// See [`capture_and_save_target`].
pub fn run_clearance_process_target(target: ClearanceTarget) -> Result<(), String> {
    capture_and_save_target(target)?;
    Ok(())
}

/// Runs the clearance extension for hotaudio and discards the saved path.
///
/// # Errors
///
/// See [`capture_and_save_target`].
pub fn run_clearance_process() -> Result<(), String> {
    run_clearance_process_target(ClearanceTarget::Hotaudio)
}

/// Turns a dynamic-loader error into an install hint. The plugin is present but
/// its webview runtime is not.
///
/// The toast is a single line, so the package names come first; the loader
/// error is only useful when the hint does not apply.
fn runtime_hint(message: &str) -> String {
    if !message.contains("libwebkit2gtk-4.1") && !message.contains("libjavascriptcoregtk-4.1") {
        return message.to_string();
    }
    #[cfg(target_os = "linux")]
    let hint = "WebKitGTK runtime missing: install webkit2gtk-4.1 (Arch), libwebkit2gtk-4.1-0 (Debian), webkit2gtk4.1 (Fedora)";
    #[cfg(not(target_os = "linux"))]
    let hint = "The system webview runtime is missing.";
    hint.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_captured_pair() {
        let captured: Captured =
            serde_json::from_str(r#"{"cookie":"cf_clearance=x","user_agent":"Mozilla/5.0"}"#)
                .unwrap();
        assert_eq!(captured.cookie, "cf_clearance=x");
        assert_eq!(captured.user_agent, "Mozilla/5.0");
    }

    #[test]
    fn clearance_target_as_str() {
        assert_eq!(ClearanceTarget::Hotaudio.as_str(), "hotaudio");
        assert_eq!(ClearanceTarget::Audiochan.as_str(), "audiochan");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_webkit_library_gets_a_package_hint() {
        let message = runtime_hint(
            "./dawngasm-plugin-clearance: error while loading shared libraries: libwebkit2gtk-4.1.so.0: cannot open shared object file",
        );
        // The toast truncates, so the package hint must lead.
        assert!(message.starts_with("WebKitGTK runtime missing"));
        assert!(message.contains("webkit2gtk-4.1"));
    }

    #[test]
    fn unrelated_failures_are_passed_through() {
        assert_eq!(runtime_hint("the window closed"), "the window closed");
    }
}
