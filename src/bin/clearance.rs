//! `dawngasm-plugin-clearance`: the webview-backed clearance plugin.
//!
//! The TUI invokes this as an extension. It prints a manifest for `--describe`,
//! or reads one JSON request from stdin and answers on stdout. All the webview
//! code lives here so the TUI itself never links `WebKitGTK`, `WKWebView` or
//! `WebView2`.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::run_return::EventLoopExtRunReturn as _;
use tao::window::WindowBuilder;
use wry::{WebView, WebViewBuilder};

const HOTAUDIO_URL: &str = "https://hotaudio.net/";
const AUDIOCHAN_URL: &str = "https://audiochan.com/";
const AUDIOCHAN_API_URL: &str = "https://api.audiochan.com/";

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

/// JavaScript injected into audiochan.com to render Cloudflare Turnstile
/// playback verification and post `verified` over the wry IPC bridge once completed.
const AUDIOCHAN_TURNSTILE_INJECTION: &str = r#"
(function() {
    if (window.__dawngasm_injected) return;
    window.__dawngasm_injected = true;

    function setupWidget() {
        if (!window.turnstile) {
            var s = document.createElement('script');
            s.src = 'https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit';
            s.async = true;
            s.onload = renderWidget;
            document.head.appendChild(s);
        } else {
            renderWidget();
        }
    }

    function renderWidget() {
        var container = document.getElementById('dawngasm-verify-box');
        if (!container) {
            container = document.createElement('div');
            container.id = 'dawngasm-verify-box';
            container.style.cssText = 'position:fixed;top:16px;left:50%;transform:translateX(-50%);z-index:999999;background:#181825;color:#cdd6f4;padding:20px;border-radius:16px;box-shadow:0 8px 32px rgba(0,0,0,0.55);text-align:center;max-width:92%;width:340px;font-family:system-ui,-apple-system,sans-serif;border:1px solid #45475a;';
            container.innerHTML = '<div style="font-weight:600;font-size:16px;margin-bottom:6px;">Playback Verification</div><div style="font-size:13px;color:#a6adc8;margin-bottom:14px;">Complete the check below to enable streaming on audiochan.</div><div id="dawngasm-turnstile-target" style="display:flex;justify-content:center;"></div><div id="dawngasm-verify-msg" style="margin-top:10px;font-size:12px;color:#89b4fa;"></div>';
            document.body.appendChild(container);
        }

        var target = document.getElementById('dawngasm-turnstile-target');
        if (target && window.turnstile) {
            try {
                window.turnstile.render(target, {
                    sitekey: '0x4AAAAAAAzFFy-NNEmM0ZDp',
                    action: 'audio_playback',
                    callback: function(token) {
                        var msg = document.getElementById('dawngasm-verify-msg');
                        if (msg) msg.innerText = 'Verifying with audiochan...';
                        fetch('https://api.audiochan.com/audios/playback-verification', {
                            method: 'POST',
                            headers: {
                                'Content-Type': 'application/json',
                                'Accept': 'application/json'
                            },
                            credentials: 'include',
                            body: JSON.stringify({ turnstile_token: token })
                        })
                        .then(function(r) { return r.json(); })
                        .then(function(data) {
                            if (data && data.success) {
                                if (msg) msg.innerText = 'Verified! Saving clearance...';
                                if (window.ipc) {
                                    window.ipc.postMessage('verified');
                                }
                            } else {
                                if (msg) msg.innerText = 'Verification failed, please retry.';
                            }
                        })
                        .catch(function(err) {
                            if (msg) msg.innerText = 'Network error: ' + err;
                        });
                    }
                });
            } catch(e) {}
        }
    }

    if (document.readyState === 'complete' || document.readyState === 'interactive') {
        setupWidget();
    } else {
        window.addEventListener('DOMContentLoaded', setupWidget);
    }
})();
"#;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Target {
    Hotaudio,
    Audiochan,
}

impl Target {
    fn from_str_opt(s: Option<&str>) -> Self {
        match s.map(str::to_ascii_lowercase).as_deref() {
            Some("audiochan") => Self::Audiochan,
            _ => Self::Hotaudio,
        }
    }

    const fn window_title(self) -> &'static str {
        match self {
            Self::Hotaudio => "Dawngasm — Cloudflare clearance",
            Self::Audiochan => "Dawngasm — audiochan playback verification",
        }
    }

    const fn url(self) -> &'static str {
        match self {
            Self::Hotaudio => HOTAUDIO_URL,
            Self::Audiochan => AUDIOCHAN_URL,
        }
    }
}

struct CapturedClearance {
    cookie: String,
    user_agent: String,
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--describe") => describe(),
        Some("run") => run(),
        Some("hotaudio") => run_standalone(Target::Hotaudio),
        Some("audiochan") => run_standalone(Target::Audiochan),
        _ => {
            eprintln!("usage: dawngasm-plugin-clearance --describe | run | hotaudio | audiochan");
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
            "description": "Opens hotaudio or audiochan in the system webview and returns the clearance cookie.",
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

    let target_str = request
        .get("params")
        .and_then(|p| p.get("target"))
        .or_else(|| request.get("target"))
        .and_then(Value::as_str);
    let target = Target::from_str_opt(target_str);

    match capture_clearance(target) {
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

fn run_standalone(target: Target) {
    match capture_clearance(target) {
        Ok(clearance) => {
            let Ok(dir) = dawngasm_tui::services::storage::Storage::default_dir() else {
                eprintln!("could not find the data directory");
                std::process::exit(1);
            };
            let result = match target {
                Target::Hotaudio => dawngasm_tui::services::hotaudio_auth::HotaudioClearance::new(
                    clearance.cookie,
                    clearance.user_agent,
                )
                .save(&dir),
                Target::Audiochan => {
                    dawngasm_tui::services::audiochan_auth::AudiochanClearance::new(
                        clearance.cookie,
                        clearance.user_agent,
                    )
                    .save(&dir)
                }
            };
            match result {
                Ok(path) => println!("clearance saved to {}", path.display()),
                Err(error) => {
                    eprintln!("failed to save clearance: {error}");
                    std::process::exit(1);
                }
            }
        }
        Err(error) => {
            eprintln!("clearance failed: {error}");
            std::process::exit(1);
        }
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

#[allow(clippy::too_many_lines, unreachable_code, unused_mut)]
fn capture_clearance(target: Target) -> Result<CapturedClearance, String> {
    let mut event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(target.window_title())
        .with_inner_size(LogicalSize::new(960.0, 720.0))
        .build(&event_loop)
        .map_err(|error| format!("could not open the clearance window: {error}"))?;

    let verified = Arc::new(AtomicBool::new(false));
    let verified_clone = Arc::clone(&verified);

    let mut builder = WebViewBuilder::new()
        .with_user_agent(USER_AGENT)
        .with_devtools(true);

    if target == Target::Audiochan {
        builder = builder
            .with_initialization_script(AUDIOCHAN_TURNSTILE_INJECTION)
            .with_ipc_handler(move |req: wry::http::Request<String>| {
                if req.body() == "verified" {
                    verified_clone.store(true, Ordering::SeqCst);
                }
            });
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let webview = builder
        .build(&window)
        .map_err(|error| format!("could not create the clearance webview: {error}"))?;
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
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
    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    let webview = {
        let _ = &builder;
        let _ = &window;
        return Err("the clearance webview is not supported on this platform".to_string());
    };

    // Drop stale clearance first so an old cookie cannot masquerade as an
    // immediate challenge success.
    match target {
        Target::Hotaudio => clear_hotaudio_cookies(&webview),
        Target::Audiochan => clear_audiochan_cookies(&webview),
    }

    let actual_user_agent = Arc::new(Mutex::new(None));
    capture_user_agent(&webview, Arc::clone(&actual_user_agent));
    webview
        .load_url(target.url())
        .map_err(|error| format!("could not load {}: {error}", target.url()))?;

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
                let clearance = match target {
                    Target::Hotaudio => clearance_from_hotaudio(&webview, &actual_user_agent),
                    Target::Audiochan => clearance_from_audiochan(
                        &webview,
                        &actual_user_agent,
                        verified.load(Ordering::SeqCst),
                    ),
                };
                if let Some(clearance) = clearance {
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
    captured.ok_or_else(|| match target {
        Target::Hotaudio => "the clearance window closed without a cf_clearance cookie".to_string(),
        Target::Audiochan => {
            "the clearance window closed without audiochan playback verification".to_string()
        }
    })
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

fn clear_audiochan_cookies(webview: &WebView) {
    for url in [AUDIOCHAN_URL, AUDIOCHAN_API_URL] {
        if let Ok(cookies) = webview.cookies_for_url(url) {
            for cookie in &cookies {
                if is_audiochan_cookie(cookie.domain()) {
                    let _ = webview.delete_cookie(cookie);
                }
            }
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

fn clearance_from_hotaudio(
    webview: &WebView,
    user_agent: &Arc<Mutex<Option<String>>>,
) -> Option<CapturedClearance> {
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
    Some(CapturedClearance { cookie, user_agent })
}

fn clearance_from_audiochan(
    webview: &WebView,
    user_agent: &Arc<Mutex<Option<String>>>,
    verified: bool,
) -> Option<CapturedClearance> {
    let mut all_cookies = Vec::new();
    if let Ok(cookies) = webview.cookies_for_url(AUDIOCHAN_URL) {
        all_cookies.extend(cookies);
    }
    if let Ok(cookies) = webview.cookies_for_url(AUDIOCHAN_API_URL) {
        all_cookies.extend(cookies);
    }
    let has_clearance = all_cookies.iter().any(|cookie| {
        cookie
            .name()
            .eq_ignore_ascii_case("__Secure-audiochan_playback")
    });
    if !has_clearance && !verified {
        return None;
    }
    let mut cookie_map = BTreeMap::new();
    for cookie in all_cookies {
        if is_audiochan_cookie(cookie.domain()) {
            cookie_map.insert(cookie.name().to_string(), cookie.value().to_string());
        }
    }
    if cookie_map.is_empty() {
        return None;
    }
    let cookie = cookie_map
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    let user_agent = user_agent
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_else(|| USER_AGENT.to_string());
    Some(CapturedClearance { cookie, user_agent })
}

fn is_hotaudio_cookie(domain: Option<&str>) -> bool {
    domain.is_none_or(|domain| {
        let domain = domain.trim_start_matches('.').to_ascii_lowercase();
        domain == "hotaudio.net" || domain.ends_with(".hotaudio.net")
    })
}

fn is_audiochan_cookie(domain: Option<&str>) -> bool {
    domain.is_none_or(|domain| {
        let domain = domain.trim_start_matches('.').to_ascii_lowercase();
        domain == "audiochan.com" || domain.ends_with(".audiochan.com")
    })
}
