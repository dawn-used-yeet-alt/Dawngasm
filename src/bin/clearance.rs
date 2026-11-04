//! `dawngasm-plugin-clearance`: the webview-backed Cloudflare clearance plugin.
//!
//! The TUI invokes this as an extension. It prints a manifest for `--describe`,
//! or reads one JSON request from stdin and answers on stdout. All the webview
//! code lives here so the TUI itself never links `WebKitGTK`, `WKWebView` or
//! `WebView2`.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dawngasm_tui::services::hotaudio_auth::HotaudioClearance;
use serde_json::{json, Value};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::run_return::EventLoopExtRunReturn as _;
use tao::window::WindowBuilder;
use wry::{WebView, WebViewBuilder};

const HOTAUDIO_URL: &str = "https://hotaudio.net/";
const WINDOW_TITLE: &str = "Dawngasm — Cloudflare clearance";
/// How often the webview cookie jar is checked while the challenge is up.
const POLL_INTERVAL: Duration = Duration::from_millis(400);
/// Give up after this long; the clearance itself is short lived anyway.
const TIMEOUT: Duration = Duration::from_secs(300);
const CAPABILITY: &str = "clearance";
const METHOD: &str = "clearance.capture";
const PROTOCOL_VERSION: u32 = 2;

/// A desktop-browser user agent, set explicitly so the cookie is bound to a
/// value this process knows. The actual `navigator.userAgent` is read back when
/// the engine reports it, which covers WebView2 versions that ignore the
/// override.
#[cfg(target_os = "windows")]
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0";
#[cfg(target_os = "macos")]
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.1 Safari/605.1.15";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--describe") => describe(),
        Some("run") => run(),
        _ => {
            eprintln!("usage: dawngasm-plugin-clearance --describe | run");
            std::process::exit(2);
        }
    }
}

fn describe() {
    println!(
        "{}",
        json!({
            "protocol": PROTOCOL_VERSION,
            "id": "clearance",
            "name": "Cloudflare clearance webview",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Opens hotaudio in the system webview and returns the Cloudflare clearance cookie.",
            "capabilities": [{
                "name": CAPABILITY,
                "methods": [METHOD],
                "timeout_ms": 330_000,
            }],
        })
    );
}

fn run() {
    let request: Value = match serde_json::from_reader(io::stdin().lock()) {
        Ok(request) => request,
        Err(error) => {
            respond_error("", "protocol", &format!("invalid request: {error}"));
            return;
        }
    };
    let id = request
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method != METHOD {
        respond_error(&id, "unsupported", &format!("unknown method `{method}`"));
        return;
    }
    match capture_clearance() {
        Ok(clearance) => println!(
            "{}",
            json!({
                "protocol": PROTOCOL_VERSION,
                "id": id,
                "ok": true,
                "result": {
                    "cookie": clearance.cookie,
                    "user_agent": clearance.user_agent,
                },
            })
        ),
        Err(error) => respond_error(&id, "plugin", &error),
    }
}

fn respond_error(id: &str, code: &str, message: &str) {
    println!(
        "{}",
        json!({
            "protocol": PROTOCOL_VERSION,
            "id": id,
            "ok": false,
            "error": {"code": code, "message": message},
        })
    );
}

fn capture_clearance() -> Result<HotaudioClearance, String> {
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

    // Drop any stale clearance first: an old cookie would otherwise look like a
    // successful challenge the moment the window opens.
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
    captured.ok_or_else(|| "the clearance window closed without a cf_clearance cookie".to_string())
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
