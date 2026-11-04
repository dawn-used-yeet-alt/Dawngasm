//! The hotaudio Cloudflare clearance window.
//!
//! Cloudflare's `cf_clearance` cookie is HttpOnly, so a terminal app cannot read
//! it out of the user's browser. The one lightweight way to obtain it is to let
//! the platform's own webview solve the challenge, then read the cookie back
//! through the webview API. `wry` uses WebKitGTK on Linux, WKWebView on macOS
//! and WebView2 on Windows, so nothing is bundled.
//!
//! The webview runs in a short-lived child process (`--cloudflare-clearance`).
//! That keeps a GUI event loop out of the TUI's main thread, makes a second
//! capture a fresh process instead of a reused event loop, and lets the parent
//! capture the child's stderr so GTK warnings never scribble over the terminal.

use std::path::PathBuf;

use crate::services::hotaudio_auth::HotaudioClearance;
use crate::services::storage::Storage;

/// The hidden argument that runs the webview instead of the TUI.
pub const CLEARANCE_FLAG: &str = "--cloudflare-clearance";

/// Opens the webview, waits for `cf_clearance`, and saves the cookie/UA pair.
///
/// # Errors
///
/// Returns an error if the window cannot be opened, the user closes it without
/// clearing the challenge, or the clearance cannot be saved.
pub fn capture_and_save() -> Result<PathBuf, String> {
    let clearance = capture_clearance()?;
    let dir = Storage::default_dir()
        .map_err(|error| format!("could not find the data directory: {error}"))?;
    clearance
        .save(&dir)
        .map_err(|error| format!("could not save the clearance: {error}"))
}

/// Runs the clearance window in a child process and reports its result.
///
/// # Errors
///
/// Returns the child's stderr when it fails, or a message when the child could
/// not be started.
pub fn run_clearance_process() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not find the executable: {error}"))?;
    let output = std::process::Command::new(exe)
        .arg(CLEARANCE_FLAG)
        .output()
        .map_err(|error| format!("could not start the clearance window: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.trim();
    let status = output.status;
    Err(if message.is_empty() {
        format!("the clearance window exited with {status}")
    } else {
        message.to_string()
    })
}

#[cfg(feature = "webview")]
mod platform {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoop};
    use tao::platform::run_return::EventLoopExtRunReturn as _;
    use tao::window::WindowBuilder;
    use wry::{WebView, WebViewBuilder};

    use crate::services::hotaudio_auth::HotaudioClearance;

    /// Where the challenge is passed. Any hotaudio page sets the same
    /// domain-wide clearance; the home page keeps the window useful even when
    /// the user has not selected a track.
    const HOTAUDIO_URL: &str = "https://hotaudio.net/";
    const WINDOW_TITLE: &str = "Dawngasm — Cloudflare clearance";
    /// How often the webview cookie jar is checked while the challenge is up.
    const POLL_INTERVAL: Duration = Duration::from_millis(400);
    /// Give up after this long; the clearance itself is short lived anyway.
    const TIMEOUT: Duration = Duration::from_secs(300);

    /// A desktop-browser user agent, set explicitly so the cookie is bound to a
    /// value this process knows. The actual `navigator.userAgent` is read back
    /// when the engine reports it, which covers WebView2 versions that ignore
    /// the override.
    #[cfg(target_os = "windows")]
    const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0";
    #[cfg(target_os = "macos")]
    const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

    pub(super) fn capture_clearance() -> Result<HotaudioClearance, String> {
        let mut event_loop = EventLoop::new();
        let window = WindowBuilder::new()
            .with_title(WINDOW_TITLE)
            .with_inner_size(LogicalSize::new(960.0, 720.0))
            .build(&event_loop)
            .map_err(|error| format!("could not open the clearance window: {error}"))?;

        let builder = WebViewBuilder::new()
            .with_user_agent(USER_AGENT)
            .with_devtools(true);
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let webview = builder
            .build(&window)
            .map_err(|error| format!("could not create the clearance webview: {error}"))?;
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let webview = {
            use tao::platform::unix::WindowExtUnix as _;
            use wry::WebViewBuilderExtUnix as _;
            let vbox = window
                .default_vbox()
                .ok_or_else(|| "the clearance window has no GTK container".to_string())?;
            builder
                .build_gtk(vbox)
                .map_err(|error| format!("could not create the clearance webview: {error}"))?
        };

        // Drop any stale clearance first: an old cookie would otherwise look
        // like a successful challenge the moment the window opens.
        clear_hotaudio_cookies(&webview);

        let actual_user_agent = Arc::new(Mutex::new(None));
        capture_user_agent(&webview, Arc::clone(&actual_user_agent));
        webview
            .load_url(HOTAUDIO_URL)
            .map_err(|error| format!("could not load {HOTAUDIO_URL}: {error}"))?;

        let started = Instant::now();
        let mut captured = None;
        let exit_code = event_loop.run_return(|event, _, control_flow| {
            *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL);
            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => *control_flow = ControlFlow::Exit,
                Event::MainEventsCleared => {
                    if started.elapsed() >= TIMEOUT {
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    if let Some(clearance) = clearance_from(&webview, &actual_user_agent) {
                        captured = Some(clearance);
                        *control_flow = ControlFlow::Exit;
                    }
                }
                _ => {}
            }
        });
        if exit_code != 0 && captured.is_none() {
            return Err("the clearance window lost its display connection".to_string());
        }
        captured
            .ok_or_else(|| "the clearance window closed without a cf_clearance cookie".to_string())
    }

    fn clear_hotaudio_cookies(webview: &WebView) {
        let Ok(cookies) = webview.cookies_for_url(HOTAUDIO_URL) else {
            return;
        };
        for cookie in &cookies {
            if is_hotaudio_cookie(cookie.domain()) {
                let _ = webview.delete_cookie(cookie);
            }
        }
    }

    fn capture_user_agent(webview: &WebView, sink: Arc<Mutex<Option<String>>>) {
        let _ = webview.evaluate_script_with_callback("navigator.userAgent", move |value| {
            let user_agent = serde_json::from_str::<String>(&value).unwrap_or(value);
            if let Ok(mut slot) = sink.lock() {
                *slot = Some(user_agent);
            }
        });
    }

    fn clearance_from(
        webview: &WebView,
        user_agent: &Arc<Mutex<Option<String>>>,
    ) -> Option<HotaudioClearance> {
        let cookies = webview.cookies_for_url(HOTAUDIO_URL).ok()?;
        if !cookies.iter().any(|cookie| cookie.name() == "cf_clearance") {
            return None;
        }
        let cookie = cookies
            .iter()
            .filter(|cookie| is_hotaudio_cookie(cookie.domain()))
            .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
            .collect::<Vec<_>>()
            .join("; ");
        if cookie.is_empty() {
            return None;
        }
        let user_agent = user_agent
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_else(|| USER_AGENT.to_string());
        Some(HotaudioClearance::new(cookie, user_agent))
    }

    fn is_hotaudio_cookie(domain: Option<&str>) -> bool {
        domain.is_none_or(|domain| {
            let domain = domain.trim_start_matches('.').to_ascii_lowercase();
            domain == "hotaudio.net" || domain.ends_with(".hotaudio.net")
        })
    }
}

#[cfg(feature = "webview")]
use platform::capture_clearance;

#[cfg(not(feature = "webview"))]
fn capture_clearance() -> Result<HotaudioClearance, String> {
    Err("this build was compiled without the `webview` feature".to_string())
}
