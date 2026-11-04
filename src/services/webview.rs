//! The hotaudio Cloudflare clearance extension.
//!
//! The webview itself ships as `dawngasm-plugin-clearance`, a separate
//! executable. Keeping it out of the TUI means the player never links
//! `WebKitGTK`, `WKWebView` or `WebView2`, so it starts on machines without a
//! webview runtime. Pressing `C` is the only action that needs the plugin.
//!
//! The plugin answers `clearance.capture` with the `cf_clearance` cookie and
//! the exact user agent that earned it; this module saves the pair.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;

use crate::services::extensions::{Extensions, CAP_CLEARANCE, METHOD_CLEARANCE_CAPTURE};
use crate::services::hotaudio_auth::HotaudioClearance;
use crate::services::storage::Storage;

#[derive(Deserialize)]
struct Captured {
    cookie: String,
    #[serde(default)]
    user_agent: String,
}

/// Opens the clearance plugin, waits for `cf_clearance`, and saves the pair.
///
/// # Errors
///
/// Returns an error when no clearance extension is installed, the plugin fails,
/// or the clearance cannot be saved.
pub fn capture_and_save() -> Result<PathBuf, String> {
    let extensions = Extensions::global();
    let plugin = extensions.find(CAP_CLEARANCE).ok_or_else(|| {
        extensions.failure(CAP_CLEARANCE).map_or_else(
            || {
                "no Cloudflare clearance extension is installed; put `dawngasm-plugin-clearance` in the plugins directory next to the app".to_string()
            },
            |failure| runtime_hint(&failure.error),
        )
    })?;
    let result = plugin.call(METHOD_CLEARANCE_CAPTURE, json!({}))?;
    let captured: Captured = serde_json::from_value(result)
        .map_err(|error| format!("the clearance extension returned an invalid result: {error}"))?;
    if captured.cookie.trim().is_empty() {
        return Err("the clearance extension returned an empty cookie".to_string());
    }
    let dir = Storage::default_dir()
        .map_err(|error| format!("could not find the data directory: {error}"))?;
    HotaudioClearance::new(captured.cookie, captured.user_agent)
        .save(&dir)
        .map_err(|error| format!("could not save the clearance: {error}"))
}

/// Runs the clearance extension and discards the saved path.
///
/// # Errors
///
/// See [`capture_and_save`].
pub fn run_clearance_process() -> Result<(), String> {
    capture_and_save()?;
    Ok(())
}

/// Turns a dynamic-loader error into an install hint. The plugin is present but
/// its webview runtime is not.
fn runtime_hint(message: &str) -> String {
    if !message.contains("libwebkit2gtk-4.1") && !message.contains("libjavascriptcoregtk-4.1") {
        return message.to_string();
    }
    #[cfg(target_os = "linux")]
    let hint = "Install the webview runtime: `sudo pacman -S webkit2gtk-4.1` on Arch, `sudo apt install libwebkit2gtk-4.1-0` on Debian/Ubuntu, or `sudo dnf install webkit2gtk4.1` on Fedora.";
    #[cfg(not(target_os = "linux"))]
    let hint = "The system webview runtime is missing.";
    format!("{message}\n{hint}")
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
    fn a_missing_webkit_library_gets_a_package_hint() {
        let message = runtime_hint(
            "./dawngasm-plugin-clearance: error while loading shared libraries: libwebkit2gtk-4.1.so.0: cannot open shared object file",
        );
        assert!(message.contains("webkit2gtk-4.1"));
    }

    #[test]
    fn unrelated_failures_are_passed_through() {
        assert_eq!(runtime_hint("the window closed"), "the window closed");
    }
}
