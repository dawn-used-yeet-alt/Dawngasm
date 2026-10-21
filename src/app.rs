//! The terminal event loop: state, keybindings, and the feed worker.
//!
//! Nothing here blocks the UI thread. API fetches run on one worker thread
//! and report back over a channel; playback resolution and decoding run on the
//! audio engine's own worker (see [`crate::services::audio`]). The loop draws,
//! waits briefly for a key, drains whatever finished, and repeats.
//!
//! The lists are virtualised: [`App::rows`] hands the renderer the slice for the
//! current view, and only the visible window is turned into spans, so a feed of
//! hundreds of posts costs the same to draw as a feed of ten.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::DefaultTerminal;

use crate::models::post::Post;
use crate::services::audio::{
    AudioEngine, AudioEvent, PlayerStatus, MAX_VOLUME, NUDGE_LARGE_MS, NUDGE_SMALL_MS,
};
use crate::services::dawngasm::{DawngasmClient, TagSuggestion, MAX_PAGE_INDEX, MIN_TAG_PREFIX};
use crate::services::mpris::{self, Mpris, PlaybackStatus};
use crate::services::storage::Storage;
use crate::ui;

/// The sorts the API offers. `relevance` needs a search to rank against and
/// otherwise falls back to `top` in the API.
pub const SORTS: [&str; 3] = ["top", "new", "relevance"];
/// The API's time windows, shortest first.
pub const PERIODS: [&str; 5] = ["day", "week", "month", "year", "all"];

/// The frame rate while idle; also how often playback position is refreshed.
const TICK: Duration = Duration::from_millis(200);
/// How long a toast stays on screen.
const TOAST_DURATION: Duration = Duration::from_secs(3);
/// How often the feed session is written while scrolling.
const SCROLL_SAVE_INTERVAL: Duration = Duration::from_secs(5);
/// Quiet time after a keystroke before asking the API for tag suggestions.
/// Matches the browser UI's 140ms debounce closely enough to feel instant.
const SUGGEST_DELAY: Duration = Duration::from_millis(150);

/// Which list the middle pane shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Feed,
    Saved,
    History,
}

impl View {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Feed => "Feed",
            Self::Saved => "Saved",
            Self::History => "History",
        }
    }
}

/// Which filter the footer prompt is editing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputMode {
    Search,
    Tags,
    BlockedTags,
}

impl InputMode {
    /// Both tag prompts use the same `/tags` autocomplete machinery.
    const fn is_tag_mode(self) -> bool {
        matches!(self, Self::Tags | Self::BlockedTags)
    }
}

/// Work for the feed worker.
enum Command {
    LoadFeed {
        sort: String,
        period: String,
        tags: String,
        exclude_tags: String,
        query: String,
        page: usize,
        generation: u64,
    },
    SuggestTags {
        prefix: String,
        generation: u64,
    },
}

/// Results from the feed worker.
enum Event {
    FeedLoaded {
        posts: Vec<Post>,
        has_more: bool,
        generation: u64,
    },
    FeedFailed {
        message: String,
        generation: u64,
    },
    TagsSuggested {
        tags: Vec<TagSuggestion>,
        generation: u64,
    },
}

pub struct App {
    // --- services ---
    /// `None` when no output device could be opened; browsing still works.
    pub(crate) engine: Option<AudioEngine>,
    storage: Storage,
    /// `None` when the MPRIS thread could not be started.
    mpris: Option<Mpris>,
    commands: Option<Sender<Command>>,
    events: Receiver<Event>,
    /// Joined on exit so the worker's channel is observed to close.
    worker: Option<JoinHandle<()>>,

    // --- feed ---
    pub(crate) feed_posts: Vec<Post>,
    /// Whether the API reported another page after the current one.
    has_more: bool,
    pub(crate) page: usize,
    load_generation: u64,
    pub(crate) loading: bool,
    pub(crate) status: String,
    pub(crate) status_is_error: bool,

    // --- view and filters ---
    pub(crate) view: View,
    sort: usize,
    period: usize,
    query: String,
    /// Comma-separated tag filter, `ANDed` with the text search by the API.
    pub(crate) tags: String,
    /// Comma-separated tags whose posts are hidden by the API.
    pub(crate) blocked_tags: String,
    /// `Some` while a filter prompt is open; holds the in-progress text.
    pub(crate) input: Option<String>,
    pub(crate) input_mode: InputMode,

    // --- tag autocomplete ---
    suggestions: Vec<TagSuggestion>,
    suggestion_selected: Option<usize>,
    /// Set when Esc dismissed the dropdown without closing the prompt.
    suggestions_hidden: bool,
    /// Fires once the user has stopped typing for [`SUGGEST_DELAY`].
    suggestion_due: Option<Instant>,
    /// Invalidates responses that arrive after the buffer changed again.
    suggestion_generation: u64,

    // --- list geometry ---
    pub(crate) selected: usize,
    pub(crate) scroll: usize,
    /// Rows the list area showed on the last frame; drives page scrolling.
    pub(crate) viewport_rows: usize,

    // --- playback ---
    pub(crate) playlist: Vec<Post>,
    pub(crate) playing_key: String,
    /// The track being resolved, added to history once it actually starts.
    resolving: Option<Post>,
    pub(crate) volume: f32,
    muted_volume: Option<f32>,
    advance_playlist: bool,

    // --- transient UI ---
    pub(crate) toast: Option<String>,
    pub(crate) toast_error: bool,
    toast_at: Option<Instant>,
    pub(crate) show_help: bool,

    // --- session ---
    pending_scroll: Option<usize>,
    last_scroll_save: Instant,
    should_quit: bool,
}

impl App {
    /// Opens storage and the audio device, starts the feed worker, and restores
    /// the previous session. The terminal is set up by the caller.
    pub fn new() -> Result<Self> {
        let storage = Storage::new()?;

        let (engine, audio_error) = match AudioEngine::new() {
            Ok(engine) => (Some(engine), None),
            Err(error) => (None, Some(error)),
        };

        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("audiogasm-feed".into())
            .spawn(move || feed_worker(command_rx, event_tx))?;

        let mpris = Mpris::start();

        let feed = storage.load_feed_session();
        let playback = storage.load_playback_session();

        let mut app = Self {
            engine,
            storage,
            mpris,
            commands: Some(command_tx),
            events: event_rx,
            worker: Some(worker),
            feed_posts: Vec::new(),
            has_more: feed.has_more,
            page: 0,
            load_generation: 0,
            loading: true,
            status: String::new(),
            status_is_error: false,
            view: View::Feed,
            sort: sort_index(&feed.sort),
            period: period_index(&feed.period),
            query: feed.query,
            tags: feed.tags,
            blocked_tags: feed.blocked_tags,
            input: None,
            input_mode: InputMode::Search,
            suggestions: Vec::new(),
            suggestion_selected: None,
            suggestions_hidden: false,
            suggestion_due: None,
            suggestion_generation: 0,
            selected: 0,
            scroll: 0,
            viewport_rows: 0,
            playlist: Vec::new(),
            playing_key: String::new(),
            resolving: None,
            volume: 1.0,
            muted_volume: None,
            advance_playlist: false,
            toast: None,
            toast_error: false,
            toast_at: None,
            show_help: false,
            pending_scroll: Some(feed.scroll_top.round().max(0.0) as usize),
            last_scroll_save: Instant::now(),
            should_quit: false,
        };

        if let Some(error) = audio_error {
            app.set_toast(format!("Audio output unavailable: {error}"), true);
        }

        // Restore the page the session was saved on, then the last track.
        app.request_page(feed.current_page);
        if let Some(session) = playback {
            app.restore_playback(
                session.post,
                session.playlist,
                session.position_ms,
                session.was_playing,
            );
        }
        Ok(app)
    }

    /// Runs until the user quits, then saves the sessions.
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        use ratatui::crossterm::event::{self, Event as TermEvent, KeyEventKind};

        let mut last_tick = Instant::now();
        loop {
            terminal.draw(|frame| ui::draw(frame, self))?;

            let timeout = TICK.saturating_sub(last_tick.elapsed());
            if event::poll(timeout)? {
                match event::read()? {
                    TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                        self.on_key(key);
                    }
                    _ => {}
                }
            }

            if last_tick.elapsed() >= TICK {
                last_tick = Instant::now();
                self.on_tick();
            }

            if self.should_quit {
                break;
            }
        }
        self.shutdown();
        Ok(())
    }

    /// Persists the feed and playback sessions and stops the worker.
    fn shutdown(&mut self) {
        self.save_feed_session();
        self.save_playback_session();
        // Dropping the sender lets the worker's `recv` fail so it exits.
        self.commands = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    // --- derived data ------------------------------------------------------

    /// The posts the current view shows. Borrowed, never cloned, so a frame
    /// costs nothing for the rows that are off screen.
    pub(crate) fn rows(&self) -> &[Post] {
        match self.view {
            View::Feed => &self.feed_posts,
            View::Saved => self.storage.saved(),
            View::History => self.storage.history(),
        }
    }

    fn selected_post(&self) -> Option<Post> {
        self.rows().get(self.selected).cloned()
    }

    pub(crate) const fn sort_label(&self) -> &'static str {
        SORTS[self.sort]
    }

    pub(crate) const fn period_label(&self) -> &'static str {
        PERIODS[self.period]
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn tags(&self) -> &str {
        &self.tags
    }

    pub(crate) fn blocked_tags(&self) -> &str {
        &self.blocked_tags
    }

    pub(crate) fn suggestions(&self) -> &[TagSuggestion] {
        &self.suggestions
    }

    pub(crate) const fn suggestion_selected(&self) -> Option<usize> {
        self.suggestion_selected
    }

    /// Whether the autocomplete dropdown should be drawn. A prompt with no
    /// matches shows nothing rather than an empty box.
    pub(crate) fn suggestions_visible(&self) -> bool {
        self.input.is_some()
            && self.input_mode.is_tag_mode()
            && !self.suggestions_hidden
            && !self.suggestions.is_empty()
    }

    pub(crate) const fn can_prev_page(&self) -> bool {
        self.page > 0
    }

    pub(crate) const fn can_next_page(&self) -> bool {
        self.has_more
    }

    pub(crate) fn engine_status(&self) -> PlayerStatus {
        self.engine.as_ref().map_or(
            PlayerStatus::Idle,
            super::services::audio::AudioEngine::status,
        )
    }

    pub(crate) fn position_ms(&self) -> i64 {
        self.engine
            .as_ref()
            .map_or(0, super::services::audio::AudioEngine::position_ms)
    }

    pub(crate) fn duration_ms(&self) -> i64 {
        self.engine
            .as_ref()
            .map_or(0, super::services::audio::AudioEngine::duration_ms)
    }

    pub(crate) fn is_seekable(&self) -> bool {
        self.engine
            .as_ref()
            .is_some_and(super::services::audio::AudioEngine::is_seekable)
    }

    /// The post currently selected for playback, if any.
    pub(crate) fn playing_post(&self) -> Option<&Post> {
        self.playlist
            .iter()
            .find(|post| post.key() == self.playing_key)
    }

    /// The selected row, for the playing highlight.
    pub(crate) fn is_playing(&self, post: &Post) -> bool {
        !self.playing_key.is_empty() && post.key() == self.playing_key
    }

    pub(crate) fn is_saved(&self, post: &Post) -> bool {
        self.storage.is_saved(post)
    }

    // --- input -------------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        if self.input.is_some() {
            self.on_input_key(key);
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        use KeyCode::{Char, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Tab, Up};
        match key.code {
            Char('c') if ctrl => self.should_quit = true,
            Char('q') => self.should_quit = true,
            Char('?') => self.show_help = !self.show_help,
            Esc => self.show_help = false,
            Tab => self.cycle_view(if shift { -1 } else { 1 }),
            Char('f') => self.set_view(View::Feed),
            Char('v') => self.set_view(View::Saved),
            Char('y') => self.set_view(View::History),

            Char('j') | Down => self.move_selection(1),
            Char('k') | Up => self.move_selection(-1),
            Char('g') | Home => self.jump(0),
            Char('G') | End => self.jump(usize::MAX),
            PageDown => self.move_selection(self.page_step()),
            PageUp => self.move_selection(-self.page_step()),
            Char('d') if ctrl => self.move_selection(self.page_step() / 2),
            Char('u') if ctrl => self.move_selection(-self.page_step() / 2),

            Enter => self.play_selected(),
            Char(' ') => self.toggle_playback(),
            Char('s') => self.toggle_saved_selected(),
            Char('n') => self.step_track(1),
            Char('p') => self.step_track(-1),

            Left | Char('h') => self.nudge(if shift {
                -NUDGE_LARGE_MS
            } else {
                -NUDGE_SMALL_MS
            }),
            Right | Char('l') => self.nudge(if shift {
                NUDGE_LARGE_MS
            } else {
                NUDGE_SMALL_MS
            }),
            Char(',') => self.nudge(-NUDGE_SMALL_MS),
            Char('.') => self.nudge(NUDGE_SMALL_MS),

            Char('-' | '_') => self.change_volume(-0.05),
            Char('+' | '=') => self.change_volume(0.05),
            Char('0') => self.set_volume(1.0),
            Char('m') => self.toggle_mute(),

            Char('/') => self.begin_input(InputMode::Search),
            Char('T') => self.begin_input(InputMode::Tags),
            Char('B') => self.begin_input(InputMode::BlockedTags),
            Char('r') => self.request_page(self.page),
            Char(']') => self.next_page(),
            Char('[') => self.prev_page(),
            Char('t') => self.cycle_sort(),
            Char('e') => self.cycle_period(),
            Char('x') => self.export_library(),
            Char('i') => self.import_library(),
            _ => {}
        }
    }

    fn on_input_key(&mut self, key: KeyEvent) {
        use KeyCode::{Backspace, Char, Down, Enter, Esc, Tab, Up};
        match key.code {
            Esc => {
                // The first Esc dismisses the dropdown; a second closes the
                // prompt, matching the browser's tag field.
                if self.suggestions_visible() {
                    self.suggestions_hidden = true;
                    self.suggestion_selected = None;
                } else {
                    self.end_input();
                }
            }
            Enter => {
                // Enter accepts only a suggestion the user actively selected;
                // otherwise it submits the typed tag/text as-is.
                if !self.accept_suggestion(true) {
                    self.submit_input();
                }
            }
            Tab => {
                // Tab completes the highlighted entry, or the first one.
                self.accept_suggestion(false);
            }
            Up => self.move_suggestion(-1),
            Down => self.move_suggestion(1),
            Backspace => {
                if let Some(buffer) = self.input.as_mut() {
                    buffer.pop();
                }
                self.input_changed();
            }
            Char(c) => {
                if let Some(buffer) = self.input.as_mut() {
                    buffer.push(c);
                }
                self.input_changed();
            }
            _ => {}
        }
    }

    /// Opens a prompt with the current value, so a filter can be amended
    /// rather than retyped. Submitting an empty buffer clears it.
    fn begin_input(&mut self, mode: InputMode) {
        self.input_mode = mode;
        self.input = Some(match mode {
            InputMode::Search => self.query.clone(),
            InputMode::Tags => self.tags.clone(),
            InputMode::BlockedTags => self.blocked_tags.clone(),
        });
        self.input_changed();
    }

    fn end_input(&mut self) {
        self.input = None;
        self.clear_suggestions();
    }

    fn submit_input(&mut self) {
        let value = self.input.take().unwrap_or_default().trim().to_string();
        match self.input_mode {
            InputMode::Search => self.query = value,
            InputMode::Tags => self.tags = value,
            InputMode::BlockedTags => self.blocked_tags = value,
        }
        self.clear_suggestions();
        self.request_page(0);
        self.save_feed_session();
    }

    /// Invalidates in-flight suggestions and schedules a new request once the
    /// user pauses. Called for every buffer edit, including accepting a tag.
    fn input_changed(&mut self) {
        self.suggestion_generation = self.suggestion_generation.wrapping_add(1);
        self.clear_suggestions();
        if self.input.is_some() && self.input_mode.is_tag_mode() {
            let fragment = active_tag_fragment(self.input.as_deref().unwrap_or(""));
            if fragment.chars().count() >= MIN_TAG_PREFIX {
                self.suggestion_due = Some(Instant::now() + SUGGEST_DELAY);
            }
        }
    }

    fn clear_suggestions(&mut self) {
        self.suggestions.clear();
        self.suggestion_selected = None;
        self.suggestions_hidden = false;
        self.suggestion_due = None;
    }

    /// Sends the debounced request for the active comma-separated tag
    /// fragment, if there is one long enough for `/tags` to accept.
    fn poll_suggestions(&mut self) {
        let Some(due) = self.suggestion_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        self.suggestion_due = None;
        if self.input.is_none() || !self.input_mode.is_tag_mode() || self.suggestions_hidden {
            return;
        }
        let fragment = active_tag_fragment(self.input.as_deref().unwrap_or(""));
        if fragment.chars().count() < MIN_TAG_PREFIX {
            return;
        }
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::SuggestTags {
                prefix: fragment.to_string(),
                generation: self.suggestion_generation,
            });
        }
    }

    fn move_suggestion(&mut self, delta: isize) {
        if !self.suggestions_visible() {
            return;
        }
        let len = self.suggestions.len() as isize;
        self.suggestion_selected = Some(match self.suggestion_selected {
            Some(index) => (index as isize + delta).rem_euclid(len) as usize,
            None if delta > 0 => 0,
            None => (len - 1) as usize,
        });
    }

    /// Replaces the active fragment with a suggestion. With
    /// `selection_required`, only an arrow-key-highlighted entry is accepted,
    /// so Enter on plain typed text still submits.
    fn accept_suggestion(&mut self, selection_required: bool) -> bool {
        if !self.suggestions_visible() {
            return false;
        }
        let index = match self.suggestion_selected {
            Some(index) => index,
            None if !selection_required => 0,
            None => return false,
        };
        let Some(tag) = self
            .suggestions
            .get(index)
            .map(|suggestion| suggestion.tag.clone())
        else {
            return false;
        };
        if let Some(buffer) = self.input.as_mut() {
            choose_tag(buffer, &tag);
        }
        self.input_changed();
        true
    }

    /// How far a page key moves, at least one row.
    fn page_step(&self) -> isize {
        self.viewport_rows.max(1) as isize
    }

    // --- list navigation ---------------------------------------------------

    fn move_selection(&mut self, delta: isize) {
        let total = self.rows().len();
        self.selected = ui::list::step(self.selected, delta, total);
        ui::list::ensure_visible(&mut self.scroll, self.selected, self.viewport_rows, total);
    }

    fn jump(&mut self, target: usize) {
        let total = self.rows().len();
        self.selected = target.min(total.saturating_sub(1));
        ui::list::ensure_visible(&mut self.scroll, self.selected, self.viewport_rows, total);
    }

    /// Keeps `selected`/`scroll` inside the list after it changed underneath.
    pub(crate) fn clamp_selection(&mut self) {
        let total = self.rows().len();
        if total == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        self.selected = self.selected.min(total - 1);
        let visible = self.viewport_rows.max(1);
        self.scroll = self.scroll.min(total.saturating_sub(visible));
    }

    fn set_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.selected = 0;
        self.scroll = 0;
        self.status.clear();
        self.status_is_error = false;
    }

    fn cycle_view(&mut self, delta: isize) {
        let next = match (self.view, delta > 0) {
            (View::Feed, true) | (View::History, false) => View::Saved,
            (View::Saved, true) | (View::Feed, false) => View::History,
            (View::History, true) | (View::Saved, false) => View::Feed,
        };
        self.set_view(next);
    }

    // --- feed --------------------------------------------------------------

    fn request_page(&mut self, page: usize) {
        self.load_generation += 1;
        self.page = page.min(MAX_PAGE_INDEX);
        self.loading = true;
        self.status.clear();
        self.status_is_error = false;
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::LoadFeed {
                sort: SORTS[self.sort].to_string(),
                period: PERIODS[self.period].to_string(),
                tags: self.tags.clone(),
                exclude_tags: self.blocked_tags.clone(),
                query: self.query.clone(),
                page: self.page,
                generation: self.load_generation,
            });
        }
    }

    fn on_feed_loaded(&mut self, posts: Vec<Post>, has_more: bool, generation: u64) {
        if generation != self.load_generation {
            return;
        }
        self.has_more = has_more && self.page < MAX_PAGE_INDEX;
        self.feed_posts = posts;
        self.loading = false;
        let restored = self.pending_scroll.take().unwrap_or(0);
        if self.view == View::Feed {
            let total = self.feed_posts.len();
            // Selecting the restored row — not the first one — is what keeps
            // `ensure_visible` from scrolling back to the top on restore.
            let position = restored.min(total.saturating_sub(1));
            self.selected = position;
            self.scroll = position;
            ui::list::ensure_visible(&mut self.scroll, self.selected, self.viewport_rows, total);
            if self.feed_posts.is_empty() {
                self.status = if self.has_more {
                    "No playable posts on this page — try the next one.".into()
                } else {
                    "No playable posts found — try another sort or search terms.".into()
                };
            }
        }
    }

    fn on_feed_failed(&mut self, message: String, generation: u64) {
        if generation != self.load_generation {
            return;
        }
        self.loading = false;
        self.has_more = false;
        self.status_is_error = true;
        self.status = format!("Could not load posts: {message}");
    }

    fn next_page(&mut self) {
        if self.can_next_page() {
            self.request_page(self.page + 1);
            self.save_feed_session();
        } else {
            self.set_toast("No next page.", false);
        }
    }

    fn prev_page(&mut self) {
        if self.page > 0 {
            let page = self.page - 1;
            self.request_page(page);
            self.save_feed_session();
        } else {
            self.set_toast("Already on the first page.", false);
        }
    }

    fn cycle_sort(&mut self) {
        self.sort = (self.sort + 1) % SORTS.len();
        self.request_page(0);
        self.save_feed_session();
        let label = SORTS[self.sort];
        if label == "relevance" && self.query.is_empty() {
            self.set_toast("Sort: relevance (needs a search; using top)", false);
        } else {
            self.set_toast(format!("Sort: {label}"), false);
        }
    }

    fn cycle_period(&mut self) {
        self.period = (self.period + 1) % PERIODS.len();
        // The window applies to every sort, but keeping `top` here matches the
        // old feed's behaviour and avoids a no-op-looking key press.
        self.sort = SORTS.iter().position(|sort| *sort == "top").unwrap_or(0);
        self.request_page(0);
        self.save_feed_session();
        let label = PERIODS[self.period];
        self.set_toast(format!("Sort: top · period {label}"), false);
    }

    // --- playback ----------------------------------------------------------

    fn play_selected(&mut self) {
        let Some(post) = self.selected_post() else {
            return;
        };
        let playlist = self.rows().to_vec();
        self.play_post(post, playlist);
    }

    fn play_post(&mut self, post: Post, playlist: Vec<Post>) {
        let Some(engine) = self.engine.as_mut() else {
            self.set_toast("Audio output is unavailable.", true);
            return;
        };
        engine.play(post.clone());
        self.playlist = playlist;
        self.playing_key = post.key().to_string();
        self.advance_playlist = false;
        // History is recorded once the track actually starts (see `poll_audio`),
        // so a failed resolve does not pollute it.
        self.resolving = Some(post);
    }

    fn restore_playback(
        &mut self,
        post: Post,
        playlist: Vec<Post>,
        position_ms: i64,
        was_playing: bool,
    ) {
        self.play_post(post, playlist);
        if let Some(engine) = self.engine.as_mut() {
            engine.set_start_paused(!was_playing);
            if position_ms > 0 {
                // The engine holds the target until the worker has probed the
                // format, so this is safe before the source exists.
                engine.seek_ms(position_ms);
            }
        }
    }

    fn toggle_playback(&mut self) {
        if let Some(engine) = self.engine.as_mut() {
            engine.toggle_playback();
        }
    }

    fn pause(&mut self) {
        if let Some(engine) = self.engine.as_mut() {
            engine.pause();
        }
    }

    fn resume(&mut self) {
        if let Some(engine) = self.engine.as_mut() {
            engine.resume();
        }
    }

    fn step_track(&mut self, delta: isize) {
        let len = self.playlist.len();
        let found = if len == 0 {
            None
        } else {
            let index = match self
                .playlist
                .iter()
                .position(|post| post.key() == self.playing_key)
            {
                Some(current) => current as isize + delta,
                // No current track is not the end of the list: next starts at
                // the top and previous at the bottom.
                None if delta > 0 => 0,
                None => len as isize - 1,
            };
            if index < 0 || index as usize >= len {
                None
            } else {
                self.playlist.get(index as usize).cloned()
            }
        };
        match found {
            Some(post) => {
                let playlist = self.playlist.clone();
                self.play_post(post, playlist);
            }
            None => {
                if delta > 0 && !self.playlist.is_empty() {
                    self.set_toast("Reached end of playlist.", false);
                }
            }
        }
    }

    fn nudge(&mut self, delta_ms: i64) {
        if !self.is_seekable() {
            self.set_toast("This track cannot be seeked yet.", false);
            return;
        }
        self.seek_relative(delta_ms);
    }

    /// Seeks without the "cannot be seeked" toast, for media keys.
    fn seek_relative(&mut self, delta_ms: i64) {
        if !self.is_seekable() {
            return;
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.nudge(delta_ms);
        }
    }

    fn seek_absolute(&mut self, position_ms: i64) {
        if !self.is_seekable() {
            return;
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.seek_ms(position_ms);
        }
    }

    fn change_volume(&mut self, delta: f32) {
        self.set_volume(self.volume + delta);
    }

    /// Sets the volume without a toast, for MPRIS.
    fn apply_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, MAX_VOLUME);
        self.muted_volume = None;
        if let Some(engine) = self.engine.as_mut() {
            engine.set_volume(self.volume);
        }
    }

    fn set_volume(&mut self, volume: f32) {
        self.apply_volume(volume);
        let percent = (self.volume * 100.0).round() as i32;
        self.set_toast(format!("Volume {percent}%"), false);
    }

    fn toggle_mute(&mut self) {
        if let Some(previous) = self.muted_volume {
            self.apply_volume(previous);
            let percent = (previous * 100.0).round() as i32;
            self.set_toast(format!("Volume {percent}%"), false);
        } else {
            let previous = self.volume;
            self.apply_volume(0.0);
            // `apply_volume` clears this, so set it afterwards.
            self.muted_volume = Some(previous);
            self.set_toast("Muted.", false);
        }
    }

    fn poll_audio(&mut self) {
        let mut events = Vec::new();
        if let Some(engine) = self.engine.as_mut() {
            events = engine.poll();
        }
        let mut started = false;
        for event in events {
            match event {
                AudioEvent::Started => started = true,
                AudioEvent::Ended => self.advance_playlist = true,
                AudioEvent::Failed(message) => {
                    self.playing_key.clear();
                    self.resolving = None;
                    self.set_toast(format!("Audio error: {message}"), true);
                }
            }
        }
        // A track is only recorded once it actually began, so a failed resolve
        // does not leave a broken entry in history.
        if started {
            if let Some(post) = self.resolving.take() {
                self.storage.add_to_history(&post);
                if self.view == View::History {
                    self.clamp_selection();
                }
            }
        }
        if std::mem::take(&mut self.advance_playlist) {
            self.step_track(1);
        }
    }

    // --- library -----------------------------------------------------------

    fn export_library(&mut self) {
        match self.storage.export_library_file() {
            Ok(path) => self.set_toast(format!("Library exported to {}", path.display()), false),
            Err(error) => self.set_toast(format!("Export failed: {error}"), true),
        }
    }

    fn import_library(&mut self) {
        match self.storage.import_library_file() {
            Ok(true) => {
                self.clamp_selection();
                self.set_toast("Library imported.", false);
            }
            Ok(false) => self.set_toast("Import failed: not a valid library file.", true),
            Err(error) => self.set_toast(format!("Import failed: {error}"), true),
        }
    }

    fn toggle_saved_selected(&mut self) {
        let Some(post) = self.selected_post() else {
            return;
        };
        let now_saved = self.storage.toggle_saved(&post);
        if self.view == View::Saved {
            self.clamp_selection();
        }
        self.set_toast(
            if now_saved {
                "Saved.".to_string()
            } else {
                "Removed from saved.".to_string()
            },
            false,
        );
    }

    // --- session -----------------------------------------------------------

    fn save_feed_session(&self) {
        self.storage.save_feed_session(
            SORTS[self.sort],
            PERIODS[self.period],
            &self.tags,
            &self.blocked_tags,
            &self.query,
            self.page,
            self.has_more,
            self.scroll as f64,
        );
    }

    fn save_playback_session(&self) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        // The measured position, not a requested seek target.
        let was_playing = !matches!(engine.status(), PlayerStatus::Paused | PlayerStatus::Idle);
        match self
            .playlist
            .iter()
            .find(|post| post.key() == self.playing_key)
        {
            Some(post) => self.storage.save_playback_session(
                post,
                &self.playlist,
                engine.position_ms(),
                was_playing,
            ),
            None => self.storage.clear_playback_session(),
        }
    }

    // --- tick --------------------------------------------------------------

    fn on_tick(&mut self) {
        self.drain_mpris();
        self.drain_feed();
        self.poll_suggestions();
        self.poll_audio();
        self.publish_mpris();

        if let Some(at) = self.toast_at {
            if at.elapsed() >= TOAST_DURATION {
                self.toast = None;
                self.toast_at = None;
            }
        }

        if self.last_scroll_save.elapsed() >= SCROLL_SAVE_INTERVAL {
            self.last_scroll_save = Instant::now();
            self.save_feed_session();
        }
    }

    /// Applies media-key and panel requests. Everything lands on the UI thread
    /// so the engine is only ever touched from here.
    fn drain_mpris(&mut self) {
        let commands = self.mpris.as_ref().map(Mpris::poll).unwrap_or_default();
        for command in commands {
            self.apply_mpris(command);
        }
    }

    fn apply_mpris(&mut self, command: mpris::Command) {
        match command {
            mpris::Command::Play => self.resume(),
            mpris::Command::Pause => self.pause(),
            mpris::Command::Toggle => self.toggle_playback(),
            mpris::Command::Stop => {
                if let Some(engine) = self.engine.as_mut() {
                    engine.stop();
                }
                self.playing_key.clear();
                self.resolving = None;
            }
            mpris::Command::Next => self.step_track(1),
            mpris::Command::Previous => self.step_track(-1),
            mpris::Command::SeekRelative(delta_ms) => self.seek_relative(delta_ms),
            mpris::Command::SeekAbsolute(position_ms) => self.seek_absolute(position_ms),
            mpris::Command::SetVolume(volume) => self.apply_volume(volume),
            mpris::Command::Quit => self.should_quit = true,
        }
    }

    /// Refreshes the state MPRIS panels read.
    fn publish_mpris(&self) {
        let Some(mpris) = self.mpris.as_ref() else {
            return;
        };
        let status = match self.engine_status() {
            PlayerStatus::Idle => PlaybackStatus::Stopped,
            // A track still resolving reads as playing, matching the desktop's
            // treatment of buffering.
            PlayerStatus::Loading | PlayerStatus::Playing => PlaybackStatus::Playing,
            PlayerStatus::Paused => PlaybackStatus::Paused,
        };
        mpris.update(
            status,
            self.position_ms(),
            self.duration_ms(),
            self.volume,
            self.is_seekable(),
            self.playing_post(),
        );
    }

    fn drain_feed(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::FeedLoaded {
                    posts,
                    has_more,
                    generation,
                } => self.on_feed_loaded(posts, has_more, generation),
                Event::FeedFailed {
                    message,
                    generation,
                } => self.on_feed_failed(message, generation),
                Event::TagsSuggested { tags, generation } => {
                    // Ignore a response for a prefix the buffer has moved past,
                    // or one that arrived after the dropdown was dismissed.
                    if generation == self.suggestion_generation
                        && self.input.is_some()
                        && self.input_mode.is_tag_mode()
                        && !self.suggestions_hidden
                    {
                        self.suggestions = tags;
                        self.suggestion_selected = None;
                    }
                }
            }
        }
    }

    fn set_toast(&mut self, message: impl Into<String>, is_error: bool) {
        self.toast = Some(message.into());
        self.toast_error = is_error;
        self.toast_at = Some(Instant::now());
    }
}

/// Fetches pages for the feed, one request at a time.
fn feed_worker(commands: Receiver<Command>, events: Sender<Event>) {
    let mut client = DawngasmClient::from_env();
    while let Ok(command) = commands.recv() {
        let event = match command {
            Command::LoadFeed {
                sort,
                period,
                tags,
                exclude_tags,
                query,
                page,
                generation,
            } => match client.fetch_posts(&sort, &period, &tags, &exclude_tags, &query, page) {
                Ok((posts, has_more)) => Event::FeedLoaded {
                    posts,
                    has_more,
                    generation,
                },
                Err(error) => Event::FeedFailed {
                    message: error.to_string(),
                    generation,
                },
            },
            Command::SuggestTags { prefix, generation } => {
                // Autocomplete is best-effort: a rate limit or transient
                // failure just leaves the dropdown empty.
                let tags = client.suggest_tags(&prefix).unwrap_or_default();
                Event::TagsSuggested { tags, generation }
            }
        };
        if events.send(event).is_err() {
            break;
        }
    }
}

fn sort_index(sort: &str) -> usize {
    SORTS
        .iter()
        .position(|candidate| *candidate == sort)
        // An old session may say `hot`, which the API no longer has.
        .unwrap_or(0)
}

fn period_index(period: &str) -> usize {
    PERIODS
        .iter()
        .position(|candidate| *candidate == period)
        // Old sessions can say `hour`; start at the shortest window we have.
        .unwrap_or(0)
}

/// The comma-separated fragment currently being typed in the tag field.
fn active_tag_fragment(value: &str) -> &str {
    let start = value.rfind(',').map_or(0, |index| index + 1);
    value[start..].trim()
}

/// Replaces the active fragment with `tag`, leaving earlier tags untouched and
/// leaving the field ready for the next one.
fn choose_tag(value: &mut String, tag: &str) {
    let start = value.rfind(',').map_or(0, |index| index + 1);
    let before = value[..start].to_string();
    let separator = if !before.is_empty() && !before.ends_with(", ") {
        " "
    } else {
        ""
    };
    *value = format!("{before}{separator}{tag}, ");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_active_tag_is_the_part_after_the_last_comma() {
        assert_eq!(active_tag_fragment(""), "");
        assert_eq!(active_tag_fragment("fem"), "fem");
        assert_eq!(active_tag_fragment("f4m, fem"), "fem");
        assert_eq!(active_tag_fragment("f4m,fem"), "fem");
        assert_eq!(active_tag_fragment("f4m, "), "");
    }

    #[test]
    fn choosing_a_tag_replaces_only_the_active_fragment() {
        let mut value = String::new();
        choose_tag(&mut value, "femdom");
        assert_eq!(value, "femdom, ");

        let mut value = "f4m, fem".to_string();
        choose_tag(&mut value, "femdom");
        assert_eq!(value, "f4m, femdom, ");

        let mut value = "f4m,fem".to_string();
        choose_tag(&mut value, "femdom");
        assert_eq!(value, "f4m, femdom, ");

        let mut value = "f4m, ".to_string();
        choose_tag(&mut value, "femdom");
        assert_eq!(value, "f4m, femdom, ");
    }
}
