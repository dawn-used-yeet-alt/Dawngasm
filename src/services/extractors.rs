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
//!
//! Parsing lives in the pure `parse_*` functions so it can be unit tested
//! without a network.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use symphonia::core::io::MediaSource;

use crate::models::audio_host::{self, AudioHost};
use crate::models::post::Post;

use super::audio::hotaudio::HotaudioMediaSource;
use super::audio::http_source::HttpMediaSource;

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

pub struct Extractors {
    /// One agent with a shared cookie jar: audiochan's play-intent cookie has
    /// to ride the ranged stream requests, and clones share the jar.
    agent: ureq::Agent,
}

impl Extractors {
    pub fn new() -> Self {
        let agent = ureq::Agent::config_builder()
            .user_agent(BROWSER_UA)
            .build()
            .new_agent();
        Extractors { agent }
    }

    /// Resolves `post` to a playable byte source, choosing the extractor by
    /// host. This blocks on network I/O, so it runs on the decode worker.
    pub fn resolve_media(&self, post: &Post) -> Result<ResolvedMedia> {
        match audio_host::detect_host(post) {
            AudioHost::Soundgasm => self.soundgasm(post),
            AudioHost::Whyp => self.whyp(post),
            AudioHost::Audiochan => self.audiochan(post),
            AudioHost::Hotaudio => self.hotaudio(post),
            AudioHost::Unknown => Err(err("the post carries no supported audio link")),
        }
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
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|e| err(format!("request to {url} failed: {e}")))?;
        if response.status() != 200 {
            return Err(err(format!("{url} returned HTTP {}", response.status())));
        }
        response
            .into_body()
            .read_to_string()
            .map_err(|e| err(format!("failed to read {url}: {e}")))
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
    fn audiochan(&self, post: &Post) -> Result<ResolvedMedia> {
        let page_url = audio_host::find_audio_url(post)
            .ok_or_else(|| err("the post carries no audiochan link"))?;
        let slug =
            extract_audiochan_slug(&page_url).ok_or_else(|| err("audiochan slug not found"))?;

        let metadata = self.post_json(
            &format!("https://api.audiochan.com/audios/slug/{slug}"),
            None,
        )?;
        let audio_id = parse_audiochan_field(&metadata, "id")
            .ok_or_else(|| err("audiochan audio id not found"))?;

        // A play intent sets the __Secure-audiochan_playback cookie the stream
        // call below needs. 404 means the track predates intents and streams
        // without one.
        let play_intent = match self.post_json(
            &format!("https://api.audiochan.com/audios/{audio_id}/play-intent"),
            None,
        ) {
            Ok(body) => Some(
                parse_audiochan_field(&body, "play_intent")
                    .ok_or_else(|| err("audiochan play intent not found"))?,
            ),
            Err(error) if error.0.contains("404") => None,
            Err(error) => return Err(error),
        };

        let stream_body = play_intent
            .as_ref()
            .map(|intent| serde_json::json!({ "play_intent": intent }).to_string());
        let stream_json = self.post_json(
            &format!("https://api.audiochan.com/audios/{audio_id}/stream-url"),
            stream_body.as_deref(),
        )?;
        let stream_url = parse_audiochan_field(&stream_json, "url")
            .ok_or_else(|| err("audiochan stream url not found"))?;

        self.http(
            stream_url,
            vec![
                ("Origin".into(), AUDIOCHAN_ORIGIN.into()),
                ("Referer".into(), AUDIOCHAN_REFERER.into()),
            ],
        )
    }

    /// hotaudio is never a plain URL: the container is decrypted on demand by
    /// [`HotaudioMediaSource`], which knows the track duration from its header.
    fn hotaudio(&self, post: &Post) -> Result<ResolvedMedia> {
        let page_url = audio_host::find_audio_url(post)
            .ok_or_else(|| err("the post carries no hotaudio link"))?;
        let source =
            HotaudioMediaSource::open(&page_url).map_err(|e| err(format!("hotaudio: {e}")))?;
        let duration = source.duration_ms();
        Ok(ResolvedMedia {
            media: Box::new(source),
            duration_hint_ms: (duration > 0).then_some(duration),
        })
    }

    /// Posts JSON and returns the response body, with the origin/referer
    /// audiochan's API checks for.
    fn post_json(&self, url: &str, body: Option<&str>) -> Result<String> {
        let response = self
            .agent
            .post(url)
            .header("Origin", AUDIOCHAN_ORIGIN)
            .header("Referer", AUDIOCHAN_REFERER)
            .header("Content-Type", "application/json")
            // ureq only offers a body-carrying POST builder, so the body-less
            // case sends an empty payload.
            .send(body.unwrap_or_default())
            .map_err(|e| match e {
                ureq::Error::StatusCode(code) => err(format!("{url} returned HTTP {code}")),
                other => err(format!("{url} failed: {other}")),
            })?;
        response
            .into_body()
            .read_to_string()
            .map_err(|e| err(format!("failed to read {url}: {e}")))
    }
}

impl Default for Extractors {
    fn default() -> Self {
        Self::new()
    }
}

// --- pure parsers ----------------------------------------------------------

pub fn find_soundgasm_url(post: &Post) -> Option<String> {
    audio_host::find_audio_url(post).filter(|url| url.to_lowercase().contains("soundgasm.net/u/"))
}

/// soundgasm embeds the stream as `m4a: "https://…"` in its page payload.
pub fn parse_soundgasm_stream_url(html: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let regex = PATTERN.get_or_init(|| Regex::new(r#"m4a:\s*["']([^"']+)["']"#).expect("m4a"));
    regex
        .captures(html)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// whyp.it ships an escaped JSON blob in `__NUXT_DATA__`. MP3 wins over FLAC.
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

pub fn extract_audiochan_slug(text: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let regex = PATTERN
        .get_or_init(|| Regex::new(r"audiochan\.com/a/([a-zA-Z0-9_-]+)").expect("audiochan slug"));
    regex
        .captures(text)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// Reads a top-level string field out of an audiochan JSON response.
pub fn parse_audiochan_field(json_data: &str, field: &str) -> Option<String> {
    let root: Value = serde_json::from_str(json_data).ok()?;
    root.get(field)?.as_str().map(|s| s.to_string())
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
        assert_eq!(parse_audiochan_field("not json", "id"), None);
        assert_eq!(parse_audiochan_field(r#"{"other":1}"#, "id"), None);
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
        match extractors.resolve_media(&post) {
            Ok(_) => panic!("an unknown host should not resolve"),
            Err(error) => {
                assert!(error.0.contains("no supported audio link"), "{}", error.0)
            }
        }
    }
}
