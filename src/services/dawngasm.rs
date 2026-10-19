//! Client for the dawngasm read-only API.
//!
//! The API indexes audio posts from every subreddit it follows, so the client
//! needs no credentials, no OAuth flow and no Reddit User-Agent: it asks the
//! Worker at [`DEFAULT_API_BASE`] for one page at a time.
//!
//! A search response carries the post metadata but, historically, not the two
//! fields this client needs to find a playable link (`url` and `selftext`).
//! When those columns are absent the page is completed with one `/query`
//! request for the same ids. Both paths are supported so the client keeps
//! working whichever revision of the API is deployed.
//!
//! Tag autocomplete comes from `GET /tags?prefix=...`, the same endpoint the
//! API's browser UI uses. Suggestions are cached for the life of the client so
//! typing back over a prefix costs no request.
//!
//! Blocking HTTP on a worker thread: the UI thread only ever sees the result,
//! so there is no async runtime to embed.

use std::collections::HashMap;

use serde_json::Value;

use crate::models::audio_host::find_audio_url;
use crate::models::post::Post;

/// The deployed Worker. Override with `DAWNGASM_API_URL` for a local
/// `wrangler dev` (for example `http://127.0.0.1:8787`).
///
/// So you've found the URL for the api this uses. I'm currently maintaining it
/// for free, I'm keeping it up until I see abuse from it. So if you want the
/// greater good, just don't abuse it, thx.
pub const DEFAULT_API_BASE: &str = "https://dawngasm.dawnusedyeet.workers.dev";
pub const DEFAULT_USER_AGENT: &str = concat!("audiogasm-tui/", env!("CARGO_PKG_VERSION"));

/// `/tags` requires at least this many characters in the prefix.
pub const MIN_TAG_PREFIX: usize = 2;
/// Suggestions shown for one prefix (the endpoint's default, made explicit).
const TAG_SUGGEST_LIMIT: usize = 10;
/// Prefixes kept for the session; a guard against unbounded typing.
const TAG_CACHE_PREFIXES: usize = 256;

/// One autocomplete recommendation from `GET /tags`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagSuggestion {
    pub tag: String,
    pub count: i64,
}

/// The API's maximum is 250; 100 is the page size the old Reddit feed used and
/// keeps a single details query under SQLite's bound-parameter comfort zone.
const PAGE_LIMIT: usize = 100;
/// The API caps `page` at 100 (1-based); this client stores it 0-based.
pub const MAX_PAGE_INDEX: usize = 99;

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

pub struct DawngasmClient {
    agent: ureq::Agent,
    base: String,
    /// Prefix to suggestions, so retyping a prefix is free. The API serves the
    /// same data with a day-long Cache-Control; this is the session's share.
    tag_cache: HashMap<String, Vec<TagSuggestion>>,
}

impl DawngasmClient {
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("DAWNGASM_API_URL").unwrap_or_else(|_| DEFAULT_API_BASE.to_string()),
        )
    }

    pub fn new(base: String) -> Self {
        let base = base.trim_end_matches('/').to_string();
        let agent = ureq::Agent::config_builder()
            .user_agent(DEFAULT_USER_AGENT)
            // Errors carry a JSON body with the reason (and, for a 429, a retry
            // hint), so read it instead of letting ureq turn the status into an
            // opaque error first.
            .http_status_as_error(false)
            .build()
            .new_agent();
        DawngasmClient {
            agent,
            base,
            tag_cache: HashMap::new(),
        }
    }

    /// One page of the feed. `page` is 0-based; the API's `page` is 1-based.
    ///
    /// `sort` is one of the API's sorts (`top`, `new`, `relevance`), `period`
    /// is this client's window name (`day`, `week`, `month`, `year`, `all`),
    /// and `tags` is the comma-separated tag filter (`f4m,ramblefap`). Tags
    /// and search text are ANDed by the API, and either may be empty. Posts
    /// whose title or body carries no supported audio link are dropped, exactly
    /// as the old Reddit listing parser did.
    pub fn fetch_posts(
        &mut self,
        sort: &str,
        period: &str,
        tags: &str,
        query: &str,
        page: usize,
    ) -> Result<(Vec<Post>, bool)> {
        let mut url = format!(
            "{}/search?limit={PAGE_LIMIT}&page={}&sort={}",
            self.base,
            page.min(MAX_PAGE_INDEX) + 1,
            api_sort(sort)
        );
        url.push_str("&window=");
        url.push_str(api_window(period));
        if !tags.is_empty() {
            url.push_str("&tags=");
            url.push_str(&percent_encode(tags));
        }
        if !query.is_empty() {
            url.push_str("&text=");
            url.push_str(&percent_encode(query));
        }

        let response = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| err(format!("dawngasm API request failed: {e}")))?;
        let status = response.status().as_u16();
        let body = read_body(response)?;
        if status != 200 {
            return Err(err(api_error(status, &body)));
        }

        let mut page = parse_search_response(&body)?;
        if page.needs_bodies {
            let ids: Vec<&str> = page.posts.iter().map(|post| post.id.as_str()).collect();
            let details = self.fetch_details(&ids)?;
            apply_details(&mut page.posts, &details);
        }
        page.posts.retain(is_audio_post);
        Ok((page.posts, page.has_more))
    }

    /// Autocomplete recommendations for one tag prefix, ordered by how many
    /// posts carry the tag. A prefix shorter than [`MIN_TAG_PREFIX`] is a
    /// local no-op, matching the endpoint's own rule.
    pub fn suggest_tags(&mut self, prefix: &str) -> Result<Vec<TagSuggestion>> {
        let prefix = prefix.trim();
        if prefix.chars().count() < MIN_TAG_PREFIX {
            return Ok(Vec::new());
        }
        if let Some(cached) = self.tag_cache.get(prefix) {
            return Ok(cached.clone());
        }

        let url = format!(
            "{}/tags?prefix={}&limit={TAG_SUGGEST_LIMIT}",
            self.base,
            percent_encode(prefix)
        );
        let response = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| err(format!("dawngasm tag request failed: {e}")))?;
        let status = response.status().as_u16();
        let body = read_body(response)?;
        if status != 200 {
            return Err(err(api_error(status, &body)));
        }
        let tags = parse_tags_response(&body)?;
        if self.tag_cache.len() >= TAG_CACHE_PREFIXES {
            self.tag_cache.clear();
        }
        self.tag_cache.insert(prefix.to_string(), tags.clone());
        Ok(tags)
    }

    /// Fetches `url` and `selftext` for a page of ids in one read-only query.
    fn fetch_details(&self, ids: &[&str]) -> Result<HashMap<String, PostBody>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT id, url, selftext FROM post WHERE id IN ({placeholders})");
        let request = serde_json::json!({ "sql": sql, "args": ids }).to_string();

        let response = self
            .agent
            .post(format!("{}/query", self.base))
            .header("content-type", "application/json")
            .send(request)
            .map_err(|e| err(format!("dawngasm API detail request failed: {e}")))?;
        let status = response.status().as_u16();
        let body = read_body(response)?;
        if status != 200 {
            return Err(err(api_error(status, &body)));
        }
        parse_details_response(&body)
    }
}

/// The two body fields that audio-host detection needs.
#[derive(Debug, Default, PartialEq, Eq)]
struct PostBody {
    url: String,
    selftext: String,
}

fn read_body(response: ureq::http::Response<ureq::Body>) -> Result<String> {
    response
        .into_body()
        .read_to_string()
        .map_err(|e| err(format!("failed to read dawngasm API response: {e}")))
}

/// Formats an API error body. The Worker answers with `{ "error": "..." }`
/// and, for rate limits, `retryAfterSeconds`.
fn api_error(status: u16, body: &str) -> String {
    let root: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let message = root
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unexpected response");
    if status == 429 {
        match root.get("retryAfterSeconds").and_then(Value::as_u64) {
            Some(seconds) => {
                format!("dawngasm API rate limit exceeded ({message}); retry in {seconds}s")
            }
            None => format!("dawngasm API rate limit exceeded ({message})"),
        }
    } else {
        format!("dawngasm API returned HTTP {status}: {message}")
    }
}

/// The API's own sort names. Old saved sessions can still say `hot`, which has
/// no equivalent and reads as `top`.
fn api_sort(sort: &str) -> &'static str {
    match sort {
        "new" => "new",
        "relevance" => "relevance",
        _ => "top",
    }
}

/// This client's windows mapped to the API's `window` values. `hour` is kept
/// for old saved sessions; the API's shortest custom window is one day.
fn api_window(period: &str) -> &'static str {
    match period {
        "hour" | "day" => "1d",
        "week" => "7d",
        "month" => "month",
        "year" => "year",
        _ => "all",
    }
}

#[derive(Debug)]
struct SearchPage {
    posts: Vec<Post>,
    has_more: bool,
    /// The response had no `url`/`selftext` columns, so the caller has to ask
    /// for them separately.
    needs_bodies: bool,
}

/// Parses a `/search` response. Posts with a title the hosts cannot serve are
/// dropped here; the audio-link filter runs after the body fields are known.
fn parse_search_response(body: &str) -> Result<SearchPage> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| err(format!("invalid response from the dawngasm API: {e}")))?;
    if let Some(error) = root.get("error").and_then(Value::as_str) {
        return Err(err(format!("dawngasm API error: {error}")));
    }
    let columns = root
        .get("columns")
        .and_then(Value::as_array)
        .ok_or_else(|| err("dawngasm API response had no columns"))?;
    let names: Vec<&str> = columns.iter().filter_map(Value::as_str).collect();
    let column = |name: &str| names.iter().position(|candidate| *candidate == name);
    let id_col = column("id").ok_or_else(|| err("dawngasm API response had no id column"))?;
    let title_col =
        column("title").ok_or_else(|| err("dawngasm API response had no title column"))?;
    let author_col = column("author");
    let score_col = column("score");
    let created_col = column("created_unix");
    let url_col = column("url");
    let selftext_col = column("selftext");

    let rows = root
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| err("dawngasm API response had no rows"))?;

    let mut posts = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(row) = row.as_array() else {
            continue;
        };
        let id = string_cell(row, id_col);
        if id.is_empty() {
            continue;
        }
        let title = string_cell(row, title_col);
        if should_filter_title(&title) {
            continue;
        }
        posts.push(Post {
            id: id.clone(),
            name: format!("t3_{id}"),
            title,
            author: author_col
                .map(|index| string_cell(row, index))
                .unwrap_or_default(),
            url: url_col
                .map(|index| string_cell(row, index))
                .unwrap_or_default(),
            selftext: selftext_col
                .map(|index| string_cell(row, index))
                .unwrap_or_default(),
            score: int_cell(row, score_col),
            // The index does not store comment counts.
            num_comments: 0,
            created_utc: int_cell(row, created_col),
        });
    }

    Ok(SearchPage {
        posts,
        has_more: root
            .get("hasMore")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        needs_bodies: url_col.is_none() || selftext_col.is_none(),
    })
}

fn parse_details_response(body: &str) -> Result<HashMap<String, PostBody>> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| err(format!("invalid response from the dawngasm API: {e}")))?;
    if let Some(error) = root.get("error").and_then(Value::as_str) {
        return Err(err(format!("dawngasm API error: {error}")));
    }
    let columns = root
        .get("columns")
        .and_then(Value::as_array)
        .ok_or_else(|| err("dawngasm API detail response had no columns"))?;
    let names: Vec<&str> = columns.iter().filter_map(Value::as_str).collect();
    let column = |name: &str| {
        names
            .iter()
            .position(|candidate| *candidate == name)
            .ok_or_else(|| err(format!("dawngasm API detail response had no {name} column")))
    };
    let id_col = column("id")?;
    let url_col = column("url")?;
    let selftext_col = column("selftext")?;
    let rows = root
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| err("dawngasm API detail response had no rows"))?;

    let mut bodies = HashMap::with_capacity(rows.len());
    for row in rows {
        let Some(row) = row.as_array() else {
            continue;
        };
        let id = string_cell(row, id_col);
        if !id.is_empty() {
            bodies.insert(
                id,
                PostBody {
                    url: string_cell(row, url_col),
                    selftext: string_cell(row, selftext_col),
                },
            );
        }
    }
    Ok(bodies)
}

/// Parses `GET /tags`: `{ "tags": [{ "tag": "...", "count": n }, ...] }`.
fn parse_tags_response(body: &str) -> Result<Vec<TagSuggestion>> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| err(format!("invalid response from the dawngasm API: {e}")))?;
    if let Some(error) = root.get("error").and_then(Value::as_str) {
        return Err(err(format!("dawngasm API error: {error}")));
    }
    let tags = root
        .get("tags")
        .and_then(Value::as_array)
        .ok_or_else(|| err("dawngasm API tag response had no tags"))?;
    let mut suggestions = Vec::with_capacity(tags.len());
    for tag in tags {
        let Some(name) = tag.get("tag").and_then(Value::as_str) else {
            continue;
        };
        suggestions.push(TagSuggestion {
            tag: name.to_string(),
            count: tag.get("count").and_then(Value::as_i64).unwrap_or(0),
        });
    }
    Ok(suggestions)
}

fn apply_details(posts: &mut [Post], bodies: &HashMap<String, PostBody>) {
    for post in posts {
        if let Some(body) = bodies.get(&post.id) {
            post.url.clone_from(&body.url);
            post.selftext.clone_from(&body.selftext);
        }
    }
}

fn string_cell(row: &[Value], index: usize) -> String {
    row.get(index)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn int_cell(row: &[Value], index: Option<usize>) -> i64 {
    index
        .and_then(|index| row.get(index))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// Drops the titles the audio hosts cannot serve, matching the old feed.
pub fn should_filter_title(title: &str) -> bool {
    let upper = title.to_uppercase();
    upper.contains("M4A") || upper.contains("M4F")
}

/// Whether a post resolves to something the app can actually play.
///
/// The index is full of posts that only *point at* audio elsewhere — script
/// offers, requests, discussion and mod announcements — and showing them next
/// to real audios just gets in the way, so the feed keeps only posts with a
/// recognisable audio link.
pub fn is_audio_post(post: &Post) -> bool {
    find_audio_url(post).is_some()
}

/// The percent-encoding GLib's `Uri.escape_string(url, "", false)` applied:
/// everything except unreserved characters.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = r#"{
        "columns": ["id","title","subreddit","author","score","created","created_unix"],
        "rows": [
            ["abc","[FF4M] One","gonewildaudio","alice",12,"2026-10-12 08:00:00",1791938952],
            ["def","Take my M4A file","gonewildaudio","bob",1,"2026-10-12 08:01:00",1791938962],
            ["ghi","[FF4M] Two","GWASapphic","carol",99,"2026-10-12 08:02:00",1791938972]
        ],
        "hasMore": true,
        "mode": "walk",
        "page": 1
    }"#;

    #[test]
    fn parses_a_search_page_and_filters_bad_titles() {
        let page = parse_search_response(SEARCH).unwrap();
        assert!(page.has_more);
        assert!(page.needs_bodies);
        let names: Vec<&str> = page.posts.iter().map(|post| post.name.as_str()).collect();
        assert_eq!(names, ["t3_abc", "t3_ghi"]);
        assert_eq!(page.posts[0].id, "abc");
        assert_eq!(page.posts[0].author, "alice");
        assert_eq!(page.posts[0].score, 12);
        assert_eq!(page.posts[0].created_utc, 1791938952);
        assert_eq!(page.posts[0].num_comments, 0);
    }

    #[test]
    fn a_response_with_body_columns_needs_no_detail_query() {
        let body = r#"{
            "columns": ["id","title","author","score","created_unix","url","selftext"],
            "rows": [
                ["abc","[F4M] One","alice",3,1,"https://example.com","listen https://soundgasm.net/u/alice/one"],
                ["def","[F4M] Two","bob",2,2,"https://whyp.it/tracks/two",""]
            ],
            "hasMore": false
        }"#;
        let mut page = parse_search_response(body).unwrap();
        assert!(!page.needs_bodies);
        assert!(!page.has_more);
        page.posts.retain(is_audio_post);
        assert_eq!(page.posts.len(), 2, "both links should be playable");
        assert_eq!(
            find_audio_url(&page.posts[0]).as_deref(),
            Some("https://soundgasm.net/u/alice/one")
        );
    }

    #[test]
    fn details_are_merged_by_id_and_audio_links_survive_the_filter() {
        let mut page = parse_search_response(SEARCH).unwrap();
        let mut bodies = HashMap::new();
        bodies.insert(
            "abc".to_string(),
            PostBody {
                url: String::new(),
                selftext: "listen: https://hotaudio.net/u/alice/one".into(),
            },
        );
        bodies.insert(
            "ghi".to_string(),
            PostBody {
                url: "https://whyp.it/tracks/two".into(),
                selftext: String::new(),
            },
        );
        apply_details(&mut page.posts, &bodies);
        assert_eq!(
            find_audio_url(&page.posts[0]).as_deref(),
            Some("https://hotaudio.net/u/alice/one")
        );
        page.posts.retain(is_audio_post);
        let names: Vec<&str> = page.posts.iter().map(|post| post.name.as_str()).collect();
        assert_eq!(names, ["t3_abc", "t3_ghi"]);
    }

    #[test]
    fn script_offers_and_linkless_posts_are_dropped() {
        let mut page = parse_search_response(SEARCH).unwrap();
        let mut bodies = HashMap::new();
        bodies.insert(
            "abc".to_string(),
            PostBody {
                selftext: "[script](https://scriptbin.works/s/1)".into(),
                ..Default::default()
            },
        );
        bodies.insert(
            "ghi".to_string(),
            PostBody {
                selftext: "just a discussion".into(),
                ..Default::default()
            },
        );
        apply_details(&mut page.posts, &bodies);
        page.posts.retain(is_audio_post);
        assert!(page.posts.is_empty(), "only playable posts should survive");
    }

    #[test]
    fn an_error_body_is_reported_instead_of_an_empty_page() {
        let error = parse_search_response(r#"{"error":"too many tags (max 20)"}"#).unwrap_err();
        assert!(error.0.contains("too many tags"));
    }

    #[test]
    fn malformed_json_is_an_error() {
        let error = parse_search_response("not json at all").unwrap_err();
        assert!(error.0.contains("invalid response"));
    }

    #[test]
    fn filters_m4a_and_m4f_case_insensitively() {
        assert!(should_filter_title("Listen to my M4A"));
        assert!(should_filter_title("listen to my m4a"));
        assert!(should_filter_title("An m4F audio"));
        assert!(!should_filter_title("[FF4M] A normal title"));
    }

    #[test]
    fn maps_client_sorts_and_windows_to_api_values() {
        assert_eq!(api_sort("top"), "top");
        assert_eq!(api_sort("new"), "new");
        assert_eq!(api_sort("relevance"), "relevance");
        // An old saved session.
        assert_eq!(api_sort("hot"), "top");

        assert_eq!(api_window("day"), "1d");
        assert_eq!(api_window("hour"), "1d");
        assert_eq!(api_window("week"), "7d");
        assert_eq!(api_window("month"), "month");
        assert_eq!(api_window("year"), "year");
        assert_eq!(api_window("all"), "all");
    }

    #[test]
    fn percent_encodes_reserved_characters() {
        assert_eq!(percent_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(percent_encode("t3_abc-123.xyz~"), "t3_abc-123.xyz~");
    }

    #[test]
    fn parses_tag_autocomplete_recommendations() {
        let tags = parse_tags_response(
            r#"{
                "tags": [
                    {"tag": "femdom", "count": 2990},
                    {"tag": "femboy", "count": 601},
                    {"tag": "feminization", "count": 563}
                ],
                "rowCount": 3
            }"#,
        )
        .unwrap();
        assert_eq!(tags.len(), 3);
        assert_eq!(tags[0].tag, "femdom");
        assert_eq!(tags[0].count, 2990);
        assert_eq!(tags[2].tag, "feminization");
    }

    #[test]
    fn an_empty_tag_response_is_an_empty_list() {
        assert!(parse_tags_response(r#"{"tags":[],"rowCount":0}"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_rate_limit_error_carries_its_retry_hint() {
        let message = api_error(
            429,
            r#"{"error":"rate limit exceeded — slow down","retryAfterSeconds":4}"#,
        );
        assert!(message.contains("retry in 4s"), "{message}");
    }
}
