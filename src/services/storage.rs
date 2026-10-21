//! On-disk state.
//!
//! `saved.json` and `history.json` are shared with the web and desktop builds,
//! so the same library is visible from every client. The feed and playback
//! sessions are TUI-specific (`tui-feed.json`, `tui-playback.json`) so the TUI
//! never clobbers a desktop session. Writes go through a temp file + rename so
//! a crash never leaves a half-written session behind.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::models::post::Post;
use crate::services::dawngasm::MAX_PAGE_INDEX;

pub const MAX_SESSION_PLAYLIST: usize = 100;
pub const MAX_HISTORY: usize = 50;
pub const LIBRARY_FORMAT: &str = "audiogasm-library-v1";
pub const MAX_IMPORT_SAVED: usize = 200;
pub const MAX_IMPORT_HISTORY: usize = 50;

#[derive(Debug, Default, Serialize, Deserialize)]
struct FeedSessionFile {
    #[serde(default)]
    sort: String,
    #[serde(default)]
    period: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    blocked_tags: String,
    #[serde(default)]
    query: String,
    #[serde(default)]
    current_page: i64,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    scroll_top: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PlaybackSessionFile {
    post: Post,
    #[serde(default)]
    playlist: Vec<Post>,
    #[serde(default)]
    position_ms: i64,
    #[serde(default)]
    was_playing: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Library {
    format: String,
    exported_at: i64,
    saved: Vec<Post>,
    history: Vec<Post>,
}

/// What `load_feed_session` found, after defaults have been applied.
#[derive(Debug, Clone)]
pub struct FeedSession {
    pub found: bool,
    pub sort: String,
    pub period: String,
    pub tags: String,
    pub blocked_tags: String,
    pub query: String,
    pub current_page: usize,
    pub has_more: bool,
    pub scroll_top: f64,
}

impl Default for FeedSession {
    fn default() -> Self {
        Self {
            found: false,
            sort: "top".into(),
            period: "day".into(),
            tags: String::new(),
            blocked_tags: String::new(),
            query: String::new(),
            current_page: 0,
            has_more: false,
            scroll_top: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlaybackSession {
    pub post: Post,
    pub playlist: Vec<Post>,
    pub position_ms: i64,
    pub was_playing: bool,
}

pub struct Storage {
    dir: PathBuf,
    saved_path: PathBuf,
    history_path: PathBuf,
    feed_path: PathBuf,
    playback_path: PathBuf,
    saved: Vec<Post>,
    history: Vec<Post>,
}

impl Storage {
    /// `$XDG_DATA_HOME/audiogasm`, `$HOME/.local/share/audiogasm` on Unix,
    /// or `%APPDATA%\audiogasm` on Windows.
    ///
    /// # Errors
    ///
    /// Returns an error if no data directory is configured or the storage
    /// directory cannot be created.
    pub fn new() -> io::Result<Self> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no data directory found (set XDG_DATA_HOME, HOME, or APPDATA)",
                )
            })?;
        Self::in_dir(&base.join("audiogasm"))
    }

    /// # Errors
    ///
    /// Returns an error if the storage directory cannot be created or its
    /// files cannot be read.
    pub fn in_dir(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let storage = Self {
            dir: dir.to_path_buf(),
            saved_path: dir.join("saved.json"),
            history_path: dir.join("history.json"),
            feed_path: dir.join("tui-feed.json"),
            playback_path: dir.join("tui-playback.json"),
            saved: Vec::new(),
            history: Vec::new(),
        };
        let mut storage = storage;
        storage.saved = Self::load_posts(&storage.saved_path.clone());
        storage.history = Self::load_posts(&storage.history_path.clone());
        Ok(storage)
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where [`export_library`](Self::export_library) writes and
    /// [`import_library_file`](Self::import_library_file) reads: a fixed path
    /// under the data directory, since a TUI has no file chooser.
    #[must_use]
    pub fn library_path(&self) -> PathBuf {
        self.dir.join("library.json")
    }

    // --- saved posts -------------------------------------------------------

    #[must_use]
    pub fn saved(&self) -> &[Post] {
        &self.saved
    }

    #[must_use]
    pub fn history(&self) -> &[Post] {
        &self.history
    }

    #[must_use]
    pub fn is_saved(&self, post: &Post) -> bool {
        self.saved.iter().any(|p| p.key() == post.key())
    }

    /// Adds when absent, removes when present. Returns the new state.
    pub fn toggle_saved(&mut self, post: &Post) -> bool {
        let now_saved = if let Some(index) = self.saved.iter().position(|p| p.key() == post.key()) {
            self.saved.remove(index);
            false
        } else {
            self.saved.insert(0, post.clone());
            true
        };
        Self::write_posts(&self.saved_path, &self.saved);
        now_saved
    }

    // --- history -----------------------------------------------------------

    /// Most recent first, capped at [`MAX_HISTORY`].
    pub fn add_to_history(&mut self, post: &Post) {
        if let Some(index) = self.history.iter().position(|p| p.key() == post.key()) {
            self.history.remove(index);
        }
        self.history.insert(0, post.clone());
        self.history.truncate(MAX_HISTORY);
        Self::write_posts(&self.history_path, &self.history);
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        Self::write_posts(&self.history_path, &self.history);
    }

    // --- feed session ------------------------------------------------------

    // The feed session is a flat record; grouping these into a struct would
    // just move the same fields one layer up.
    #[allow(clippy::too_many_arguments)]
    pub fn save_feed_session(
        &self,
        sort: &str,
        period: &str,
        tags: &str,
        blocked_tags: &str,
        query: &str,
        current_page: usize,
        has_more: bool,
        scroll_top: f64,
    ) {
        let session = FeedSessionFile {
            sort: if sort.is_empty() {
                "top".into()
            } else {
                sort.into()
            },
            period: if period.is_empty() {
                "day".into()
            } else {
                period.into()
            },
            tags: tags.into(),
            blocked_tags: blocked_tags.into(),
            query: query.into(),
            current_page: i64::try_from(current_page).unwrap_or(i64::MAX),
            has_more,
            scroll_top: if scroll_top.is_nan() || scroll_top < 0.0 {
                0.0
            } else {
                scroll_top
            },
        };
        Self::write_json(&self.feed_path, &session);
    }

    #[must_use]
    pub fn load_feed_session(&self) -> FeedSession {
        let mut session = FeedSession::default();
        // `after_tokens` from older builds is simply ignored: the API is
        // page-addressed now, and `current_page` survives as before.
        let stored: FeedSessionFile = match Self::read_json(&self.feed_path) {
            Some(stored) => stored,
            None => return session,
        };

        if !stored.sort.is_empty() {
            session.sort = stored.sort;
        }
        if !stored.period.is_empty() {
            session.period = stored.period;
        }
        session.tags = stored.tags;
        session.blocked_tags = stored.blocked_tags;
        session.query = stored.query;
        let max_page = i64::try_from(MAX_PAGE_INDEX).unwrap_or(i64::MAX);
        session.current_page = usize::try_from(stored.current_page.clamp(0, max_page)).unwrap_or(0);
        session.has_more = stored.has_more;
        session.scroll_top = if stored.scroll_top.is_nan() || stored.scroll_top < 0.0 {
            0.0
        } else {
            stored.scroll_top
        };
        session.found = true;
        session
    }

    // --- playback session --------------------------------------------------

    pub fn save_playback_session(
        &self,
        post: &Post,
        playlist: &[Post],
        position_ms: i64,
        was_playing: bool,
    ) {
        if post.key().is_empty() {
            self.clear_playback_session();
            return;
        }
        let session = PlaybackSessionFile {
            post: post.clone(),
            playlist: playlist
                .iter()
                .filter(|p| !p.key().is_empty())
                .take(MAX_SESSION_PLAYLIST)
                .cloned()
                .collect(),
            position_ms: position_ms.max(0),
            was_playing,
        };
        Self::write_json(&self.playback_path, &session);
    }

    #[must_use]
    pub fn load_playback_session(&self) -> Option<PlaybackSession> {
        let stored: PlaybackSessionFile = Self::read_json(&self.playback_path)?;
        if stored.post.key().is_empty() {
            return None;
        }
        Some(PlaybackSession {
            post: stored.post,
            playlist: stored
                .playlist
                .into_iter()
                .filter(|p| !p.key().is_empty())
                .take(MAX_SESSION_PLAYLIST)
                .collect(),
            position_ms: stored.position_ms.max(0),
            was_playing: stored.was_playing,
        })
    }

    pub fn clear_playback_session(&self) {
        if let Err(err) = fs::remove_file(&self.playback_path) {
            if err.kind() != io::ErrorKind::NotFound {
                eprintln!("failed to clear playback session file: {err}");
            }
        }
    }

    // --- library export / import ------------------------------------------

    /// `audiogasm-library-v1`, shared with the web app's Saved tab.
    #[must_use]
    pub fn export_library(&self) -> String {
        let library = Library {
            format: LIBRARY_FORMAT.into(),
            exported_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)),
            saved: self.saved.clone(),
            history: self.history.clone(),
        };
        serde_json::to_string_pretty(&library).unwrap_or_else(|_| "{}".into())
    }

    /// Unions the document into saved + history. Returns false, changing
    /// nothing, on invalid JSON or a format mismatch.
    pub fn import_library(&mut self, json_data: &str) -> bool {
        let raw: serde_json::Value = match serde_json::from_str(json_data) {
            Ok(value) => value,
            Err(_) => return false,
        };
        if raw.get("format").and_then(|v| v.as_str()) != Some(LIBRARY_FORMAT) {
            return false;
        }

        // Parse everything before touching state so a failure is a no-op.
        let imported_saved = parse_library_posts(raw.get("saved"), MAX_IMPORT_SAVED);
        let imported_history = parse_library_posts(raw.get("history"), MAX_IMPORT_HISTORY);

        let mut saved: Vec<Post> = Vec::new();
        for post in imported_saved.into_iter().chain(self.saved.iter().cloned()) {
            if !saved.iter().any(|p| p.key() == post.key()) {
                saved.push(post);
            }
        }
        let mut history: Vec<Post> = Vec::new();
        for post in imported_history
            .into_iter()
            .chain(self.history.iter().cloned())
        {
            if history.len() >= MAX_IMPORT_HISTORY {
                break;
            }
            if !history.iter().any(|p| p.key() == post.key()) {
                history.push(post);
            }
        }

        self.saved = saved;
        self.history = history;
        Self::write_posts(&self.saved_path, &self.saved);
        Self::write_posts(&self.history_path, &self.history);
        true
    }

    /// Writes the exported library to [`library_path`](Self::library_path).
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn export_library_file(&self) -> io::Result<PathBuf> {
        let path = self.library_path();
        fs::write(&path, self.export_library())?;
        Ok(path)
    }

    /// Reads and imports the library at [`library_path`](Self::library_path).
    /// `Ok(false)` means the file existed but was not a valid library.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read.
    pub fn import_library_file(&mut self) -> io::Result<bool> {
        let data = fs::read_to_string(self.library_path())?;
        Ok(self.import_library(&data))
    }

    // --- file helpers ------------------------------------------------------

    fn write_json<T: Serialize>(path: &Path, value: &T) {
        let data = match serde_json::to_string_pretty(value) {
            Ok(data) => data,
            Err(err) => {
                eprintln!("failed to serialize {}: {err}", path.display());
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(err) = fs::write(&tmp, data) {
            eprintln!("failed to save {}: {err}", path.display());
            return;
        }
        if let Err(err) = fs::rename(&tmp, path) {
            eprintln!("failed to replace {}: {err}", path.display());
            let _ = fs::remove_file(&tmp);
        }
    }

    fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
        let data = fs::read_to_string(path).ok()?;
        match serde_json::from_str(&data) {
            Ok(value) => Some(value),
            Err(err) => {
                eprintln!("failed to parse {}: {err}", path.display());
                None
            }
        }
    }

    fn write_posts(path: &Path, posts: &[Post]) {
        Self::write_json(path, &posts);
    }

    fn load_posts(path: &Path) -> Vec<Post> {
        Self::read_json::<Vec<Post>>(path).unwrap_or_default()
    }
}

fn parse_library_posts(value: Option<&serde_json::Value>, cap: usize) -> Vec<Post> {
    let Some(array) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut posts: Vec<Post> = Vec::new();
    for entry in array {
        if posts.len() >= cap {
            break;
        }
        if let Some(post) = parse_library_post(entry) {
            if !posts.iter().any(|p| p.key() == post.key()) {
                posts.push(post);
            }
        }
    }
    posts
}

/// Accepts the union of the desktop and web field shapes. `None` when the
/// entry lacks a non-empty name, title or url.
fn parse_library_post(value: &serde_json::Value) -> Option<Post> {
    let object = value.as_object()?;
    let string_field = |key: &str| -> String {
        object
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let int_field = |key: &str| -> i64 {
        object
            .get(key)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };

    let name = string_field("name");
    let title = string_field("title");
    let url = string_field("url");
    if name.is_empty() || title.is_empty() || url.is_empty() {
        return None;
    }
    // The web app writes numComments; the desktop wrote num_comments.
    let num_comments = object.get("num_comments").map_or_else(
        || {
            object
                .get("numComments")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
        },
        |value| value.as_i64().unwrap_or(0),
    );
    Some(Post {
        id: string_field("id"),
        name,
        title,
        author: string_field("author"),
        url,
        selftext: string_field("selftext"),
        score: int_field("score"),
        num_comments,
        created_utc: int_field("created_utc"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("audiogasm-storage-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn post(name: &str) -> Post {
        Post {
            id: name.trim_start_matches("t3_").to_string(),
            name: name.to_string(),
            title: format!("title for {name}"),
            author: "someone".into(),
            url: "https://example.com".into(),
            ..Default::default()
        }
    }

    #[test]
    fn saved_round_trips_through_disk() {
        let dir = temp_dir("saved");
        {
            let mut storage = Storage::in_dir(&dir).unwrap();
            assert!(storage.toggle_saved(&post("t3_a")));
            assert!(storage.is_saved(&post("t3_a")));
            assert!(storage.toggle_saved(&post("t3_b")));
        }
        let mut storage = Storage::in_dir(&dir).unwrap();
        assert_eq!(storage.saved().len(), 2);
        assert!(storage.is_saved(&post("t3_a")));
        // Toggling an already-saved post removes it.
        assert!(!storage.toggle_saved(&post("t3_a")));
        assert!(!storage.is_saved(&post("t3_a")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn toggle_saved_reports_the_new_state() {
        let dir = temp_dir("toggle");
        let mut storage = Storage::in_dir(&dir).unwrap();
        assert!(storage.toggle_saved(&post("t3_a")));
        assert!(!storage.toggle_saved(&post("t3_a")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_is_most_recent_first_and_capped() {
        let dir = temp_dir("history");
        let mut storage = Storage::in_dir(&dir).unwrap();
        for i in 0..MAX_HISTORY + 10 {
            storage.add_to_history(&post(&format!("t3_{i}")));
        }
        assert_eq!(storage.history().len(), MAX_HISTORY);
        assert_eq!(
            storage.history()[0].key(),
            format!("t3_{}", MAX_HISTORY + 9)
        );
        // Replaying a track moves it back to the front rather than duplicating.
        storage.add_to_history(&post("t3_5"));
        assert_eq!(storage.history()[0].key(), "t3_5");
        assert_eq!(
            storage
                .history()
                .iter()
                .filter(|p| p.key() == "t3_5")
                .count(),
            1
        );
        storage.clear_history();
        assert_eq!(storage.history(), []);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn feed_session_round_trips() {
        let dir = temp_dir("feed");
        let storage = Storage::in_dir(&dir).unwrap();
        storage.save_feed_session(
            "top",
            "week",
            "f4m,ramblefap",
            "m4f,m4a",
            "gentle dom",
            2,
            true,
            240.5,
        );
        let loaded = storage.load_feed_session();
        assert!(loaded.found);
        assert_eq!(loaded.sort, "top");
        assert_eq!(loaded.period, "week");
        assert_eq!(loaded.tags, "f4m,ramblefap");
        assert_eq!(loaded.blocked_tags, "m4f,m4a");
        assert_eq!(loaded.query, "gentle dom");
        assert_eq!(loaded.current_page, 2);
        assert!(loaded.has_more);
        assert_eq!(loaded.scroll_top, 240.5);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn feed_session_defaults_when_absent() {
        let dir = temp_dir("feed-missing");
        let storage = Storage::in_dir(&dir).unwrap();
        let loaded = storage.load_feed_session();
        assert!(!loaded.found);
        assert_eq!(loaded.sort, "top");
        assert_eq!(loaded.period, "day");
        assert_eq!(loaded.tags, "");
        assert_eq!(loaded.blocked_tags, "");
        assert!(!loaded.has_more);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_legacy_feed_session_still_loads_its_page() {
        let dir = temp_dir("feed-legacy");
        fs::create_dir_all(&dir).unwrap();
        // The old format carried Reddit `after` tokens. Serde ignores them and
        // the page number survives.
        fs::write(
            dir.join("tui-feed.json"),
            r#"{"sort":"hot","period":"hour","query":"","current_page":2,
                "after_tokens":[null,"t3_x","t3_y"],"scroll_top":5.0}"#,
        )
        .unwrap();
        let loaded = Storage::in_dir(&dir).unwrap().load_feed_session();
        assert!(loaded.found);
        assert_eq!(loaded.current_page, 2);
        assert_eq!(loaded.sort, "hot");
        assert_eq!(loaded.period, "hour");
        assert!(loaded.tags.is_empty(), "a legacy file has no tag filter");
        assert!(
            loaded.blocked_tags.is_empty(),
            "a legacy file has no blocklist"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn feed_session_clamps_a_page_past_the_api_limit() {
        let dir = temp_dir("feed-clamp");
        let storage = Storage::in_dir(&dir).unwrap();
        storage.save_feed_session("top", "day", "", "", "", 900, false, 0.0);
        assert_eq!(storage.load_feed_session().current_page, 99);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn playback_session_round_trips() {
        let dir = temp_dir("playback");
        let storage = Storage::in_dir(&dir).unwrap();
        let playlist = vec![post("t3_a"), post("t3_b")];
        storage.save_playback_session(&post("t3_b"), &playlist, 90_000, true);
        let loaded = storage.load_playback_session().expect("session");
        assert_eq!(loaded.post.key(), "t3_b");
        assert_eq!(loaded.playlist.len(), 2);
        assert_eq!(loaded.position_ms, 90_000);
        assert!(loaded.was_playing);
        storage.clear_playback_session();
        assert!(storage.load_playback_session().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_track_clears_the_playback_session() {
        let dir = temp_dir("playback-empty");
        let storage = Storage::in_dir(&dir).unwrap();
        storage.save_playback_session(&post("t3_a"), &[], 0, false);
        storage.save_playback_session(&Post::default(), &[], 0, false);
        assert!(storage.load_playback_session().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn library_export_import_round_trips() {
        let dir = temp_dir("library");
        let mut storage = Storage::in_dir(&dir).unwrap();
        storage.toggle_saved(&post("t3_a"));
        storage.add_to_history(&post("t3_b"));
        let json = storage.export_library();

        let other_dir = temp_dir("library-import");
        let mut other = Storage::in_dir(&other_dir).unwrap();
        assert!(other.import_library(&json));
        assert!(other.is_saved(&post("t3_a")));
        assert_eq!(other.history().len(), 1);
        // Importing again is idempotent.
        assert!(other.import_library(&json));
        assert_eq!(other.saved().len(), 1);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&other_dir);
    }

    #[test]
    fn import_rejects_bad_documents_without_touching_state() {
        let dir = temp_dir("library-bad");
        let mut storage = Storage::in_dir(&dir).unwrap();
        storage.toggle_saved(&post("t3_a"));
        assert!(!storage.import_library("not json"));
        assert!(!storage.import_library("{}"));
        assert!(!storage.import_library(r#"{"format":"other","saved":[]}"#));
        assert_eq!(storage.saved().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_accepts_the_web_apps_camel_case_comments() {
        let dir = temp_dir("library-web");
        let mut storage = Storage::in_dir(&dir).unwrap();
        let json = r#"{
            "format": "audiogasm-library-v1",
            "exported_at": 1,
            "saved": [{"id":"a","name":"t3_a","title":"t","url":"https://x",
                       "author":"me","selftext":"","score":3,"numComments":7,
                       "created_utc":100}],
            "history": []
        }"#;
        assert!(storage.import_library(json));
        assert_eq!(storage.saved()[0].num_comments, 7);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_skips_entries_missing_required_fields() {
        let dir = temp_dir("library-partial");
        let mut storage = Storage::in_dir(&dir).unwrap();
        let json = r#"{
            "format": "audiogasm-library-v1",
            "saved": [{"name":"t3_a","title":"t"},
                      {"name":"","title":"t","url":"https://x"},
                      {"name":"t3_c","title":"t","url":"https://y"}],
            "history": []
        }"#;
        assert!(storage.import_library(json));
        assert_eq!(storage.saved().len(), 1);
        assert_eq!(storage.saved()[0].key(), "t3_c");
        let _ = fs::remove_dir_all(&dir);
    }
}
