//! Audio-host detection.
//!
//! The patterns and their order are load-bearing: [`find_audio_url`] tries each
//! host in turn over both the post url and its selftext, so a post carrying a
//! soundgasm link in the body and a whyp.it link as its url resolves to
//! soundgasm.

use std::sync::OnceLock;

use regex::Regex;

use super::post::Post;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioHost {
    Soundgasm,
    Whyp,
    Audiochan,
    Hotaudio,
    Unknown,
}

impl AudioHost {
    /// The text shown in a post's host badge.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Soundgasm => "soundgasm",
            Self::Whyp => "whyp.it",
            Self::Audiochan => "audiochan",
            Self::Hotaudio => "hotaudio",
            Self::Unknown => "",
        }
    }
}

/// Reddit escapes slashes in some payloads and escapes HTML entities in the
/// rendered body; undo both before looking for links.
#[must_use]
pub fn normalize_text(text: &str) -> String {
    text.replace("\\u002F", "/")
        .replace("\\u002f", "/")
        .replace("\\/", "/")
        .replace("\\_", "_")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&#x2F;", "/")
        .replace("&#x2f;", "/")
        .replace("&#47;", "/")
}

/// Trailing punctuation picked up when a link is copied out of prose.
#[must_use]
pub fn clean_url(url: &str) -> String {
    let mut cleaned = url.trim();
    loop {
        match cleaned.chars().last() {
            Some(c) if matches!(c, ')' | ']' | '>' | '.' | ',' | ';' | '"' | '\'') => {
                cleaned = &cleaned[..cleaned.len() - c.len_utf8()];
            }
            _ => break,
        }
    }
    cleaned.to_string()
}

struct Patterns {
    soundgasm: Regex,
    whyp: Regex,
    audiochan: Regex,
    hotaudio: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        soundgasm: Regex::new(
            r"(?i)https?://(?:www\.)?soundgasm\.net/u/[a-zA-Z0-9_%~.-]+/[a-zA-Z0-9_%~.-]+",
        )
        .expect("soundgasm pattern"),
        whyp: Regex::new(
            r#"(?i)https?://(?:www\.)?whyp\.it/(?:tracks|t)/[a-zA-Z0-9_%~.-]+(?:/[a-zA-Z0-9_%~.-]+)?(?:\?[^"'\s<>\)\]]*)?"#,
        )
        .expect("whyp pattern"),
        audiochan: Regex::new(
            r"(?i)https?://(?:www\.)?audiochan\.com/(?:embed/)?a/([a-zA-Z0-9_-]+)",
        )
        .expect("audiochan pattern"),
        hotaudio: Regex::new(
            r#"(?i)https?://(?:www\.)?hotaudio\.net/u/[a-zA-Z0-9_%~.-]+/[a-zA-Z0-9_%~.-]+(?:\?[^"'\s<>\)\]]*)?"#,
        )
        .expect("hotaudio pattern"),
    })
}

/// The first audio link in the post, in host order, or `None`.
#[must_use]
pub fn find_audio_url(post: &Post) -> Option<String> {
    let patterns = patterns();
    let targets = [normalize_text(&post.url), normalize_text(&post.selftext)];
    let regexes = [
        &patterns.soundgasm,
        &patterns.whyp,
        &patterns.audiochan,
        &patterns.hotaudio,
    ];
    for regex in regexes {
        for target in &targets {
            if target.is_empty() {
                continue;
            }
            if let Some(found) = regex.find(target) {
                return Some(clean_url(found.as_str()));
            }
        }
    }
    None
}

#[must_use]
pub fn detect_host(post: &Post) -> AudioHost {
    let Some(url) = find_audio_url(post) else {
        return AudioHost::Unknown;
    };
    host_of_url(&url)
}

/// The host a resolved URL belongs to.
#[must_use]
pub fn host_of_url(url: &str) -> AudioHost {
    let lower = url.to_lowercase();
    if lower.contains("soundgasm.net/u/") {
        AudioHost::Soundgasm
    } else if lower.contains("whyp.it/tracks/") || lower.contains("whyp.it/t/") {
        AudioHost::Whyp
    } else if lower.contains("audiochan.com/a/") || lower.contains("audiochan.com/embed/a/") {
        AudioHost::Audiochan
    } else if lower.contains("hotaudio.net/u/") {
        AudioHost::Hotaudio
    } else {
        AudioHost::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(url: &str, selftext: &str) -> Post {
        Post {
            url: url.to_string(),
            selftext: selftext.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn detects_each_host_from_the_post_url() {
        assert_eq!(
            detect_host(&post("https://www.soundgasm.net/u/user/track", "")),
            AudioHost::Soundgasm
        );
        assert_eq!(
            detect_host(&post("https://whyp.it/tracks/abc-123", "")),
            AudioHost::Whyp
        );
        assert_eq!(
            detect_host(&post("https://whyp.it/t/abc-123", "")),
            AudioHost::Whyp
        );
        assert_eq!(
            detect_host(&post("https://audiochan.com/a/someid", "")),
            AudioHost::Audiochan
        );
        assert_eq!(
            detect_host(&post("https://audiochan.com/embed/a/someid", "")),
            AudioHost::Audiochan
        );
        assert_eq!(
            detect_host(&post("https://hotaudio.net/u/user/track", "")),
            AudioHost::Hotaudio
        );
        assert_eq!(
            detect_host(&post("https://example.com/audio.mp3", "")),
            AudioHost::Unknown
        );
        assert_eq!(detect_host(&post("", "")), AudioHost::Unknown);
    }

    #[test]
    fn finds_links_in_the_selftext() {
        let p = post(
            "https://example.com/x",
            "listen here https://hotaudio.net/u/someone/track-name",
        );
        assert_eq!(detect_host(&p), AudioHost::Hotaudio);
        assert_eq!(
            find_audio_url(&p).as_deref(),
            Some("https://hotaudio.net/u/someone/track-name")
        );
    }

    #[test]
    fn host_order_decides_when_a_post_carries_several_links() {
        let p = post(
            "https://whyp.it/tracks/abc",
            "also https://soundgasm.net/u/user/track",
        );
        assert_eq!(detect_host(&p), AudioHost::Soundgasm);
    }

    #[test]
    fn normalizes_reddit_escapes() {
        assert_eq!(
            normalize_text("https:\\/\\/soundgasm.net\\/u\\/a\\/b&amp;x=1"),
            "https://soundgasm.net/u/a/b&x=1"
        );
        let p = post(r"https:\/\/whyp.it\/tracks\/abc", "");
        assert_eq!(detect_host(&p), AudioHost::Whyp);
    }

    #[test]
    fn clean_url_strips_trailing_punctuation() {
        assert_eq!(clean_url("https://a/b."), "https://a/b");
        assert_eq!(clean_url("https://a/b),"), "https://a/b");
        assert_eq!(clean_url("  https://a/b  "), "https://a/b");
        assert_eq!(clean_url("https://a/b"), "https://a/b");
    }
}
