//! Per-host stream resolution into a seekable [`MediaSource`].
//!
//! Every host answers the same question — given a post, what bytes does the
//! decoder read? — and each is scraped differently:
//!
//! - **soundgasm / whyp.it** serve a plain ranged file, so the page is scraped
//!   once for the signed media URL and playback streams it directly.
//! - **audiochan** serves a cookie-gated URL: a play-intent call sets a cookie
//!   that the ranged requests must carry, so one cookie-jar agent is threaded
//!   through both.
//! - **hotaudio** serves nothing plain: its container is decrypted on demand
//!   (see [`hotaudio`](super::audio::hotaudio)), which is why it returns a
//!   different kind of source entirely.
//! - **unknown hosts** fall back to an `extract` extension: a separate process
//!   that returns a plain ranged media URL, so a new site can be added without
//!   rebuilding the TUI.
//!
//! Parsing lives in the pure `parse_*` functions so it can be unit tested
//! without a network.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use symphonia::core::io::MediaSource;

use crate::models::audio_host::{self, AudioHost};
use crate::models::post::Post;

use super::audio::hotaudio::HotaudioMediaSource;
use super::audio::http_source::HttpMediaSource;
use super::audiochan_auth::AudiochanClearance;
use super::extensions::{self, Extension, CAP_EXTRACT, METHOD_EXTRACT_RESOLVE};
use super::hotaudio_auth::HotaudioClearance;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err(message: impl Into<String>) -> Error {
    Error(message.into())
}

/// The desktop sites expect a browser UA; anything else gets blocked. Shared
/// with [`HttpMediaSource`] so its requests are indistinguishable from a page
/// load's.
pub const BROWSER_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0";

/// audiochan gates its stream on this session's cookie, set by the play-intent
/// call and checked on the ranged requests that follow.
const AUDIOCHAN_ORIGIN: &str = "https://audiochan.com";
const AUDIOCHAN_REFERER: &str = "https://audiochan.com/";
const AUDIOCHAN_API: &str = "https://api.audiochan.com";

/// A resolved track: the seekable byte source plus an optional duration the
/// extractor already knows.
///
/// The hint exists for containers whose length the demuxer cannot infer up
/// front (hotaudio's fragmented MP4, whose header omits a duration sample
/// count). For a plain file the probe finds the duration itself and the hint
/// stays `None`.
pub struct ResolvedMedia {
    pub media: Box<dyn MediaSource>,
    pub duration_hint_ms: Option<i64>,
}

/// The `extract.resolve` result an extension returns.
#[derive(serde::Deserialize)]
struct PluginMedia {
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    duration_ms: Option<i64>,
}

pub struct Extractors {
    /// One agent with a shared cookie jar: audiochan's play-intent cookie has
    /// to ride the ranged stream requests, and clones share the jar.
    agent: ureq::Agent,
}

impl Extractors {
    #[must_use]
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .user_agent(BROWSER_UA)
            // Statuses are handled by the callers: audiochan's slug lookup has
            // to tell a 404 (try the older POST route) apart from a real error.
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self { agent }
    }

    /// Resolves `post` to a playable byte source, choosing the extractor by
    /// host. Unknown hosts fall back to `extract` extensions, and a plugin can
    /// opt in to running before a built-in extractor, so new sites and
    /// replacements can be added without rebuilding the TUI.
    ///
    /// This blocks on network I/O, so it runs on the decode worker.
    ///
    /// # Errors
    ///
    /// Returns an error if no extractor handles the post or resolving the
    /// stream fails.
    pub fn resolve_media(&self, post: &Post) -> Result<ResolvedMedia> {
        let host = audio_host::detect_host(post);
        let extensions = extensions::Extensions::global();

        let mut urls: Vec<&str> = Vec::new();
        if !post.url.is_empty() {
            urls.push(post.url.as_str());
        }
        urls.extend(extensions::urls_in_text(&post.selftext));
        let candidates = extensions.extractors_for_urls(&urls);

        if host != AudioHost::Unknown {
            let overrides: Vec<&Extension> = candidates
                .iter()
                .copied()
                .filter(|plugin| plugin.overrides_builtin(CAP_EXTRACT))
                .collect();
            // An override that crashes or times out must not hide the working
            // built-in extractor, so only a successful result short-circuits.
            if let Ok(Some(media)) = self.try_plugins(&overrides, post) {
                return Ok(media);
            }
            return self.resolve_host(post, host);
        }

        if let Some(media) = self.try_plugins(&candidates, post)? {
            return Ok(media);
        }
        Err(err(
            "the post carries no supported audio link (install an `extract` extension for this site)",
        ))
    }

    fn resolve_host(&self, post: &Post, host: AudioHost) -> Result<ResolvedMedia> {
        match host {
            AudioHost::Soundgasm => self.soundgasm(post),
            AudioHost::Whyp => self.whyp(post),
            AudioHost::Audiochan => self.audiochan(post),
            AudioHost::Hotaudio => Self::hotaudio(post),
            AudioHost::Unknown => Err(err(
                "the post carries no supported audio link (install an `extract` extension for this site)",
            )),
        }
    }

    /// Asks each `extract` extension in turn to turn the post into a plain
    /// HTTP media URL. A plugin can decline with `{"handled": false}` (or an
    /// `unsupported`/`not_found` error), which lets several plugins share the
    /// capability. A plugin that crashes or returns a malformed result is
    /// skipped so a later candidate still gets a chance; the first such error
    /// is reported only if nothing handles the post.
    ///
    /// Hotaudio stays built in because it needs a decrypting stream, not a URL.
    fn try_plugins(&self, plugins: &[&Extension], post: &Post) -> Result<Option<ResolvedMedia>> {
        let mut first_error: Option<String> = None;
        for plugin in plugins {
            let result = plugin.call(
                METHOD_EXTRACT_RESOLVE,
                serde_json::json!({
                    "id": post.id,
                    "name": post.name,
                    "url": post.url,
                    "selftext": post.selftext,
                    "title": post.title,
                    "author": post.author,
                    "score": post.score,
                    "num_comments": post.num_comments,
                    "created_utc": post.created_utc,
                }),
            );
            let value = match result {
                Ok(value) => value,
                Err(error) if error.is_unhandled() => continue,
                Err(error) => {
                    first_error.get_or_insert_with(|| error.to_string());
                    continue;
                }
            };
            if extensions::is_unhandled(&value) {
                continue;
            }
            let media: PluginMedia = match serde_json::from_value(value) {
                Ok(media) => media,
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        format!("{} returned an invalid result: {error}", plugin.manifest.id)
                    });
                    continue;
                }
            };
            if media.url.trim().is_empty() {
                first_error.get_or_insert_with(|| {
                    format!("{} returned an empty media URL", plugin.manifest.id)
                });
                continue;
            }
            match self.http(media.url, media.headers.into_iter().collect()) {
                Ok(mut resolved) => {
                    resolved.duration_hint_ms = media.duration_ms;
                    return Ok(Some(resolved));
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| error.to_string());
                }
            }
        }
        first_error.map_or_else(|| Ok(None), |error| Err(err(error)))
    }

    fn http(&self, url: String, headers: Vec<(String, String)>) -> Result<ResolvedMedia> {
        let media = HttpMediaSource::with_headers(self.agent.clone(), url, headers)
            .map_err(|e| err(e.to_string()))?;
        Ok(ResolvedMedia {
            media: Box::new(media),
            duration_hint_ms: None,
        })
    }

    fn get(&self, url: &str) -> Result<String> {
        let (status, body) = self.get_response(url)?;
        if status != 200 {
            return Err(err(format!("{url} returned HTTP {status}")));
        }
        Ok(body)
    }

    fn get_response(&self, url: &str) -> Result<(u16, String)> {
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|e| err(format!("request to {url} failed: {e}")))?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| err(format!("failed to read {url}: {e}")))?;
        Ok((status, body))
    }

    fn soundgasm(&self, post: &Post) -> Result<ResolvedMedia> {
        let page_url =
            find_soundgasm_url(post).ok_or_else(|| err("the post carries no soundgasm link"))?;
        let html = self.get(&page_url)?;
        let stream_url = parse_soundgasm_stream_url(&html)
            .ok_or_else(|| err("soundgasm stream url not found"))?;
        self.http(stream_url, Vec::new())
    }

    fn whyp(&self, post: &Post) -> Result<ResolvedMedia> {
        let page_url =
            audio_host::find_audio_url(post).ok_or_else(|| err("the post carries no whyp link"))?;
        let html = self.get(&page_url)?;
        let stream_url = parse_whyp_html(&html).ok_or_else(|| err("whyp stream url not found"))?;
        self.http(stream_url, Vec::new())
    }

    /// audiochan needs a short dance: resolve the slug to an id, ask for a play
    /// intent (which sets the playback cookie), exchange that for a signed
    /// stream URL, then stream it with the origin/referer the CDN checks.
    ///
    /// When Cloudflare Turnstile verification is required, a saved clearance
    /// cookie and user agent are attached to the API calls and stream request.
    ///
    /// The API moves, so both slug lookup methods are supported: the current
    /// deployment answers `GET /audios/slug/{slug}`, while older ones used
    /// `POST` for the same route. A 404/405 from GET is therefore retried as a
    /// POST before the track is reported missing.
    fn audiochan(&self, post: &Post) -> Result<ResolvedMedia> {
        let page_url = audio_host::find_audio_url(post)
            .ok_or_else(|| err("the post carries no audiochan link"))?;
        let slug =
            extract_audiochan_slug(&page_url).ok_or_else(|| err("audiochan slug not found"))?;

        let clearance = AudiochanClearance::load_default();
        let user_agent = clearance
            .as_ref()
            .map(|c| c.user_agent.trim())
            .filter(|ua| !ua.is_empty())
            .unwrap_or(BROWSER_UA);
        let clearance_cookie = clearance
            .as_ref()
            .map(|c| c.cookie.trim())
            .filter(|c| !c.is_empty());

        let metadata = self.audiochan_metadata(&slug, user_agent, clearance_cookie)?;

        if let Some(file_url) = parse_audiochan_file_url(&metadata) {
            let mut headers = vec![
                ("Origin".into(), AUDIOCHAN_ORIGIN.into()),
                ("Referer".into(), AUDIOCHAN_REFERER.into()),
                ("User-Agent".into(), user_agent.to_string()),
            ];
            if let Some(cookie) = clearance_cookie {
                headers.push(("Cookie".into(), cookie.to_string()));
            }
            return self.http(file_url, headers);
        }

        let audio_id = parse_audiochan_field(&metadata, "id")
            .ok_or_else(|| err("audiochan audio id not found"))?;
        let file_id = parse_audiochan_file_id(&metadata);

        let intent_body = file_id
            .as_ref()
            .map(|id| serde_json::json!({ "file_id": id }).to_string());
        let (intent_status, intent_body_resp) = self.audiochan_api_call(
            &format!("{AUDIOCHAN_API}/audios/{audio_id}/play-intent"),
            "POST",
            intent_body.as_deref(),
            user_agent,
            clearance_cookie,
        )?;
        let play_intent = match intent_status {
            200 => Some(
                parse_audiochan_field(&intent_body_resp, "play_intent")
                    .ok_or_else(|| err("audiochan play intent not found"))?,
            ),
            403 => return Err(audiochan_verification_error(clearance_cookie.is_some())),
            404 => None,
            status => {
                return Err(err(format!("audiochan play intent returned HTTP {status}")));
            }
        };

        let mut stream_payload_obj = serde_json::Map::new();
        if let Some(intent) = play_intent {
            stream_payload_obj.insert("play_intent".to_string(), Value::String(intent));
        }
        if let Some(fid) = file_id {
            stream_payload_obj.insert("file_id".to_string(), Value::String(fid));
        }
        let stream_body = Value::Object(stream_payload_obj).to_string();

        let (status, stream_json) = self.audiochan_api_call(
            &format!("{AUDIOCHAN_API}/audios/{audio_id}/stream-url"),
            "POST",
            Some(&stream_body),
            user_agent,
            clearance_cookie,
        )?;
        if status == 403 {
            return Err(audiochan_verification_error(clearance_cookie.is_some()));
        }
        if status != 200 {
            return Err(err(format!(
                "audiochan stream url returned HTTP {status}: {}",
                response_excerpt(&stream_json)
            )));
        }
        let stream_url = parse_audiochan_field(&stream_json, "url")
            .ok_or_else(|| err("audiochan stream url not found"))?;

        let mut headers = vec![
            ("Origin".into(), AUDIOCHAN_ORIGIN.into()),
            ("Referer".into(), AUDIOCHAN_REFERER.into()),
            ("User-Agent".into(), user_agent.to_string()),
        ];
        if let Some(cookie) = clearance_cookie {
            headers.push(("Cookie".into(), cookie.to_string()));
        }

        self.http(stream_url, headers)
    }

    /// Resolves an audiochan slug with the current GET route, falling back to
    /// the older POST route when the deployment does not expose GET.
    fn audiochan_metadata(
        &self,
        slug: &str,
        user_agent: &str,
        clearance_cookie: Option<&str>,
    ) -> Result<String> {
        let url = format!("{AUDIOCHAN_API}/audios/slug/{slug}");
        let (get_status, get_body) =
            self.audiochan_api_call(&url, "GET", None, user_agent, clearance_cookie)?;
        if get_status == 200 {
            return Ok(get_body);
        }
        if matches!(get_status, 404 | 405) {
            let (post_status, post_body) =
                self.audiochan_api_call(&url, "POST", None, user_agent, clearance_cookie)?;
            if post_status == 200 {
                return Ok(post_body);
            }
            if post_status == 403 {
                return Err(audiochan_verification_error(clearance_cookie.is_some()));
            }
            return Err(err(format!(
                "audiochan slug lookup returned HTTP {get_status} (GET) and {post_status} (POST)"
            )));
        }
        if get_status == 403 {
            return Err(audiochan_verification_error(clearance_cookie.is_some()));
        }
        Err(err(format!(
            "audiochan slug lookup returned HTTP {get_status}: {}",
            response_excerpt(&get_body)
        )))
    }

    /// hotaudio is never a plain URL: the container is decrypted on demand by
    /// [`HotaudioMediaSource`], which knows the track duration from its header.
    fn hotaudio(post: &Post) -> Result<ResolvedMedia> {
        let page_url = audio_host::find_audio_url(post)
            .ok_or_else(|| err("the post carries no hotaudio link"))?;
        // Read the pair on every resolve: the webview saves it from a child
        // process while this worker keeps running, so a cached copy would stay
        // stale until restart.
        let clearance = HotaudioClearance::load_default();
        let source = HotaudioMediaSource::open(&page_url, clearance.as_ref())
            .map_err(|error| hotaudio_error(&error))?;
        let duration = source.duration_ms();
        Ok(ResolvedMedia {
            media: Box::new(source),
            duration_hint_ms: (duration > 0).then_some(duration),
        })
    }

    /// Performs an audiochan API HTTP call with origin, referer, user-agent,
    /// and optional clearance cookie headers.
    fn audiochan_api_call(
        &self,
        url: &str,
        method: &str,
        body: Option<&str>,
        user_agent: &str,
        clearance_cookie: Option<&str>,
    ) -> Result<(u16, String)> {
        let response = if method == "POST" {
            let mut request = self
                .agent
                .post(url)
                .header("User-Agent", user_agent)
                .header("Origin", AUDIOCHAN_ORIGIN)
                .header("Referer", AUDIOCHAN_REFERER)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json");
            if let Some(cookie) = clearance_cookie {
                if !cookie.trim().is_empty() {
                    request = request.header("Cookie", cookie);
                }
            }
            request
                .send(body.unwrap_or_default())
                .map_err(|e| err(format!("{url} failed: {e}")))?
        } else {
            let mut request = self
                .agent
                .get(url)
                .header("User-Agent", user_agent)
                .header("Origin", AUDIOCHAN_ORIGIN)
                .header("Referer", AUDIOCHAN_REFERER)
                .header("Accept", "application/json");
            if let Some(cookie) = clearance_cookie {
                if !cookie.trim().is_empty() {
                    request = request.header("Cookie", cookie);
                }
            }
            request
                .call()
                .map_err(|e| err(format!("{url} failed: {e}")))?
        };
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .read_to_string()
            .map_err(|e| err(format!("failed to read {url}: {e}")))?;
        Ok((status, body))
    }
}

/// Adds the one actionable hint when hotaudio is gated by Cloudflare.
fn hotaudio_error(error: &std::io::Error) -> Error {
    let message = format!("hotaudio: {error}");
    if message.contains("site challenge") {
        err(format!(
            "{message} — press C to open the Cloudflare clearance window"
        ))
    } else {
        err(message)
    }
}

fn audiochan_verification_error(has_clearance: bool) -> Error {
    if has_clearance {
        err("audiochan verification expired — press C to open the clearance window")
    } else {
        err("audiochan needs browser verification — press C to open the clearance window")
    }
}

/// A short, single-line response body for an error message.
fn response_excerpt(body: &str) -> String {
    let excerpt: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if excerpt.chars().count() > 200 {
        excerpt.chars().take(200).collect::<String>() + "…"
    } else {
        excerpt
    }
}

impl Default for Extractors {
    fn default() -> Self {
        Self::new()
    }
}

// --- pure parsers ----------------------------------------------------------

#[must_use]
pub fn find_soundgasm_url(post: &Post) -> Option<String> {
    audio_host::find_audio_url(post).filter(|url| url.to_lowercase().contains("soundgasm.net/u/"))
}

/// soundgasm embeds the stream as `m4a: "https://…"` in its page payload.
///
/// # Panics
///
/// Panics if the built-in regular expression is invalid.
pub fn parse_soundgasm_stream_url(html: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let regex = PATTERN.get_or_init(|| Regex::new(r#"m4a:\s*["']([^"']+)["']"#).expect("m4a"));
    regex
        .captures(html)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// whyp.it ships an escaped JSON blob in `__NUXT_DATA__`. MP3 wins over FLAC.
///
/// # Panics
///
/// Panics if a built-in regular expression is invalid.
pub fn parse_whyp_html(html: &str) -> Option<String> {
    static MP3: OnceLock<Regex> = OnceLock::new();
    static FLAC: OnceLock<Regex> = OnceLock::new();
    let unescaped = html.replace("\\u002F", "/");
    let mp3 = MP3.get_or_init(|| {
        Regex::new(r#"https://cdn\.whyp\.it/[0-9a-fA-F\-]+\.mp3\?token=[^"&,]+\&expires=\d+"#)
            .expect("whyp mp3")
    });
    if let Some(found) = mp3.find(&unescaped) {
        return Some(found.as_str().to_string());
    }
    let flac = FLAC.get_or_init(|| {
        Regex::new(r#"https://cdn\.whyp\.it/[0-9a-fA-F\-]+\.flac\?token=[^"&,]+\&expires=\d+"#)
            .expect("whyp flac")
    });
    flac.find(&unescaped)
        .map(|found| found.as_str().to_string())
}

/// Extracts the slug from an audiochan post URL. Both the normal `/a/{slug}`
/// route and the embed route are accepted.
///
/// # Panics
///
/// Panics if the built-in regular expression is invalid.
pub fn extract_audiochan_slug(text: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let regex = PATTERN.get_or_init(|| {
        Regex::new(r"(?i)audiochan\.com/(?:embed/)?a/([a-zA-Z0-9_-]+)").expect("audiochan slug")
    });
    regex
        .captures(text)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// Reads a string field out of an audiochan JSON response. The API has served
/// fields both at the top level and under a `data` envelope; accept either.
#[must_use]
pub fn parse_audiochan_field(json_data: &str, field: &str) -> Option<String> {
    let root: Value = serde_json::from_str(json_data).ok()?;
    root.get(field)
        .or_else(|| root.get("data").and_then(|data| data.get(field)))?
        .as_str()
        .map(std::string::ToString::to_string)
}

/// Reads `audioFile.id` from the audiochan slug response when present.
#[must_use]
pub fn parse_audiochan_file_id(json_data: &str) -> Option<String> {
    let root: Value = serde_json::from_str(json_data).ok()?;
    let from_obj = |obj: &Value| -> Option<String> {
        let file = obj.get("audioFile").or_else(|| obj.get("audio_file"))?;
        let id = file.get("id")?.as_str()?;
        if id.is_empty() || id == "null" {
            None
        } else {
            Some(id.to_string())
        }
    };
    from_obj(&root).or_else(|| root.get("data").and_then(from_obj))
}

/// Reads `audioFile.url` from the audiochan slug response when present.
#[must_use]
pub fn parse_audiochan_file_url(json_data: &str) -> Option<String> {
    let root: Value = serde_json::from_str(json_data).ok()?;
    let from_obj = |obj: &Value| -> Option<String> {
        let file = obj.get("audioFile").or_else(|| obj.get("audio_file"))?;
        let url = file.get("url")?.as_str()?;
        if url.is_empty() || url == "null" {
            None
        } else {
            Some(url.to_string())
        }
    };
    from_obj(&root).or_else(|| root.get("data").and_then(from_obj))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_soundgasm_m4a_field() {
        let html = r#"<script>var tracks = [{ m4a: "https://media.soundgasm.net/sounds/abc.m4a", ogg: "x" }];</script>"#;
        assert_eq!(
            parse_soundgasm_stream_url(html).as_deref(),
            Some("https://media.soundgasm.net/sounds/abc.m4a")
        );
        assert_eq!(parse_soundgasm_stream_url("<html>nothing</html>"), None);
    }

    #[test]
    fn parses_whyp_escaped_payloads_and_prefers_mp3() {
        let html = r#"<script>__NUXT_DATA__=[...,"https:\u002F\u002Fcdn.whyp.it\u002F12345678-abcd-1234-abcd-1234567890ab.flac?token=f&expires=1","https:\u002F\u002Fcdn.whyp.it\u002F12345678-abcd-1234-abcd-1234567890ab.mp3?token=pub&expires=1787330000"]</script>"#;
        assert_eq!(
            parse_whyp_html(html).as_deref(),
            Some("https://cdn.whyp.it/12345678-abcd-1234-abcd-1234567890ab.mp3?token=pub&expires=1787330000")
        );
    }

    #[test]
    fn falls_back_to_flac_when_there_is_no_mp3() {
        let html = r#"<script>__NUXT_DATA__=[...,"https:\u002F\u002Fcdn.whyp.it\u002F98765432-feed-9876-feed-9876543210fe.flac?token=flac_token&expires=1787330000"]</script>"#;
        assert_eq!(
            parse_whyp_html(html).as_deref(),
            Some("https://cdn.whyp.it/98765432-feed-9876-feed-9876543210fe.flac?token=flac_token&expires=1787330000")
        );
        assert_eq!(parse_whyp_html("<html></html>"), None);
    }

    #[test]
    fn finds_the_audiochan_slug() {
        assert_eq!(
            extract_audiochan_slug("https://audiochan.com/a/some-track_1").as_deref(),
            Some("some-track_1")
        );
        assert_eq!(
            extract_audiochan_slug("https://audiochan.com/embed/a/some-track_1").as_deref(),
            Some("some-track_1")
        );
        assert_eq!(extract_audiochan_slug("https://example.com/a/x"), None);
    }

    #[test]
    fn reads_audiochan_json_fields() {
        assert_eq!(
            parse_audiochan_field(r#"{"id":"abc"}"#, "id").as_deref(),
            Some("abc")
        );
        assert_eq!(
            parse_audiochan_field(r#"{"url":"https://cdn/x.m4a"}"#, "url").as_deref(),
            Some("https://cdn/x.m4a")
        );
        assert_eq!(
            parse_audiochan_field(r#"{"data":{"id":"nested"}}"#, "id").as_deref(),
            Some("nested")
        );
        assert_eq!(parse_audiochan_field("not json", "id"), None);
        assert_eq!(parse_audiochan_field(r#"{"other":1}"#, "id"), None);
    }

    #[test]
    fn reads_audiochan_file_id() {
        let json = r#"{"audioFile":{"id":"file_123","url":null}}"#;
        assert_eq!(parse_audiochan_file_id(json).as_deref(), Some("file_123"));
        let snake_case = r#"{"audio_file":{"id":"file_456"}}"#;
        assert_eq!(
            parse_audiochan_file_id(snake_case).as_deref(),
            Some("file_456")
        );
        let nested = r#"{"data":{"audioFile":{"id":"file_789"}}}"#;
        assert_eq!(parse_audiochan_file_id(nested).as_deref(), Some("file_789"));
        assert_eq!(
            parse_audiochan_file_id(r#"{"audioFile":{"id":null}}"#),
            None
        );
        assert_eq!(parse_audiochan_file_id(r#"{"audioFile":{"id":""}}"#), None);
        assert_eq!(parse_audiochan_file_id("{}"), None);
    }

    #[test]
    fn reads_audiochan_file_url_when_public() {
        let json = r#"{"audioFile":{"id":"x","url":"https://cdn.audiochan.com/file.m4a"}}"#;
        assert_eq!(
            parse_audiochan_file_url(json).as_deref(),
            Some("https://cdn.audiochan.com/file.m4a")
        );
        let nested =
            r#"{"data":{"audio_file":{"id":"x","url":"https://cdn.audiochan.com/nested.m4a"}}}"#;
        assert_eq!(
            parse_audiochan_file_url(nested).as_deref(),
            Some("https://cdn.audiochan.com/nested.m4a")
        );
        assert_eq!(
            parse_audiochan_file_url(r#"{"audioFile":{"url":null}}"#),
            None
        );
        assert_eq!(parse_audiochan_file_url("{}"), None);
    }

    #[test]
    fn only_soundgasm_posts_resolve_to_a_soundgasm_page() {
        let mut post = Post {
            url: "https://soundgasm.net/u/someone/track".into(),
            ..Default::default()
        };
        assert_eq!(
            find_soundgasm_url(&post).as_deref(),
            Some("https://soundgasm.net/u/someone/track")
        );
        post.url = "https://whyp.it/tracks/abc".into();
        assert_eq!(find_soundgasm_url(&post), None);
    }

    #[test]
    fn a_post_without_a_known_link_fails_before_any_request() {
        let extractors = Extractors::new();
        let post = Post {
            url: "https://example.com/not-audio".into(),
            ..Default::default()
        };
        match extractors.resolve_host(&post, AudioHost::Unknown) {
            Ok(_) => panic!("an unknown host should not resolve"),
            Err(error) => {
                assert!(error.0.contains("no supported audio link"), "{}", error.0);
            }
        }
    }
}
