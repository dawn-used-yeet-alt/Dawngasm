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

use std::collections::BTreeMap;
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
use crate::services::extensions::{
    self, CommandDecl, EnrichResult, Extension, Extensions, FeedPage, FilterResult,
    PostAnnotations, CAP_ENRICH, CAP_FEED, CAP_FILTER, METHOD_COMMAND_INVOKE, METHOD_ENRICH_APPLY,
    METHOD_FEED_FETCH, METHOD_FEED_TAGS, METHOD_FILTER_APPLY,
};
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
    Command,
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

/// One extension command shown in the `:` palette.
#[derive(Clone, Debug)]
pub(crate) struct CommandMatch {
    pub extension: Extension,
    pub command: CommandDecl,
}

/// Results from the feed worker.
enum Event {
    FeedLoaded {
        posts: Vec<Post>,
        has_more: bool,
        annotations: BTreeMap<String, PostAnnotations>,
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
    /// The feed worker finished discovering extensions. Commands are cached on
    /// the UI side so opening the palette never waits on a plugin scan.
    ExtensionsLoaded {
        commands: Vec<CommandMatch>,
    },
    /// A `command` extension finished on its worker thread.
    CommandFinished {
        message: String,
        is_error: bool,
    },
}

#[allow(clippy::struct_excessive_bools)] // Independent UI and session flags.
pub struct App {
    // --- services ---
    /// `None` when no output device could be opened; browsing still works.
    pub(crate) engine: Option<AudioEngine>,
    storage: Storage,
    /// `None` when the MPRIS thread could not be started.
    mpris: Option<Mpris>,
    commands: Option<Sender<Command>>,
    events: Receiver<Event>,
    /// A clone kept so extension commands can report back from their own
    /// thread instead of blocking the UI while the plugin runs.
    event_tx: Sender<Event>,
    /// Joined on exit so the worker's channel is observed to close.
    worker: Option<JoinHandle<()>>,

    // --- feed ---
    pub(crate) feed_posts: Vec<Post>,
    /// Badges and tags contributed by `post.enrich` extensions, keyed by post.
    pub(crate) annotations: BTreeMap<String, PostAnnotations>,
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
    /// Highlighted entry in the `:` command palette.
    command_selected: Option<usize>,
    /// Commands discovered by the feed worker, so the palette does not load
    /// plugins on the UI thread.
    extension_commands: Vec<CommandMatch>,

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
    ///
    /// # Errors
    ///
    /// Returns an error if storage cannot be opened or the feed worker cannot be
    /// spawned.
    pub fn new() -> Result<Self> {
        let storage = Storage::new()?;

        let (engine, audio_error) = match AudioEngine::new() {
            Ok(engine) => (Some(engine), None),
            Err(error) => (None, Some(error)),
        };

        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let worker_events = event_tx.clone();
        let worker = thread::Builder::new()
            .name("dawngasm-feed".into())
            .spawn(move || feed_worker(&command_rx, &worker_events))?;

        let mpris = Mpris::start();

        let feed = storage.load_feed_session();
        let playback = storage.load_playback_session();

        let mut app = Self {
            engine,
            storage,
            mpris,
            commands: Some(command_tx),
            events: event_rx,
            event_tx,
            worker: Some(worker),
            feed_posts: Vec::new(),
            annotations: BTreeMap::new(),
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
            command_selected: None,
            extension_commands: Vec::new(),
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
            pending_scroll: Some(saved_scroll_offset(feed.scroll_top)),
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
    ///
    /// # Errors
    ///
    /// Returns an error if drawing or reading terminal events fails.
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
    pub(crate) const fn suggestions_visible(&self) -> bool {
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
        use KeyCode::{Char, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Tab, Up};

        if self.input.is_some() {
            self.on_input_key(key);
            return;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
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
            Char(':') => self.begin_input(InputMode::Command),
            Char('T') => self.begin_input(InputMode::Tags),
            Char('B') => self.begin_input(InputMode::BlockedTags),
            Char('r') => self.request_page(self.page),
            Char(']') => self.next_page(),
            Char('[') => self.prev_page(),
            Char('t') => self.cycle_sort(),
            Char('e') => self.cycle_period(),
            Char('x') => self.export_library(),
            Char('i') => self.import_library(),
            Char('C') => self.capture_cloudflare_clearance(),
            _ => {}
        }
    }

    fn on_input_key(&mut self, key: KeyEvent) {
        use KeyCode::{Backspace, Char, Down, Enter, Esc, Tab, Up};
        if self.input_mode == InputMode::Command {
            self.on_command_key(key);
            return;
        }
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

    /// Handles keys while the `:` command palette is open.
    fn on_command_key(&mut self, key: KeyEvent) {
        use KeyCode::{Backspace, Char, Down, Enter, Esc, Tab, Up};
        match key.code {
            Esc => self.end_input(),
            Enter => {
                let typed = self.input.as_deref().unwrap_or_default().trim().to_string();
                let matches = self.command_matches();
                // An explicit arrow selection wins, so two plugins exposing
                // the same command id are still individually reachable.
                let command = self
                    .command_selected
                    .and_then(|index| matches.get(index))
                    .cloned()
                    .or_else(|| {
                        matches
                            .iter()
                            .find(|candidate| candidate.command.id == typed)
                            .cloned()
                    })
                    .or_else(|| matches.first().cloned());
                self.end_input();
                if let Some(command) = command {
                    self.run_command_match(command);
                } else if typed.trim().is_empty() {
                    self.set_toast("No extension commands are installed.", false);
                } else {
                    self.set_toast(
                        format!("No extension command matches “{}”.", typed.trim()),
                        true,
                    );
                }
            }
            Tab => {
                let id = self
                    .command_matches()
                    .get(self.command_selected.unwrap_or(0))
                    .map(|candidate| candidate.command.id.clone());
                if let (Some(id), Some(buffer)) = (id, self.input.as_mut()) {
                    buffer.clone_from(&id);
                }
            }
            Up => self.move_command(-1),
            Down => self.move_command(1),
            Backspace => {
                if let Some(buffer) = self.input.as_mut() {
                    buffer.pop();
                }
                self.command_selected = None;
            }
            Char(character) => {
                if let Some(buffer) = self.input.as_mut() {
                    buffer.push(character);
                }
                self.command_selected = None;
            }
            _ => {}
        }
    }

    fn move_command(&mut self, delta: isize) {
        let len = self.command_matches().len();
        if len == 0 {
            self.command_selected = None;
            return;
        }
        self.command_selected = Some(match self.command_selected {
            Some(index) => wrap_index(index, delta, len),
            None if delta > 0 => 0,
            None => len - 1,
        });
    }

    /// Commands whose id or title matches the current palette text.
    pub(crate) fn command_matches(&self) -> Vec<CommandMatch> {
        let query = self
            .input
            .as_deref()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        self.extension_commands
            .iter()
            .filter(|candidate| {
                query.is_empty()
                    || candidate.command.id.to_ascii_lowercase().contains(&query)
                    || candidate
                        .command
                        .display_title()
                        .to_ascii_lowercase()
                        .contains(&query)
                    || candidate
                        .extension
                        .manifest
                        .id
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .cloned()
            .collect()
    }

    pub(crate) const fn command_selected(&self) -> Option<usize> {
        self.command_selected
    }

    fn run_command(&mut self, value: &str) {
        let matches = self.command_matches();
        let selected = self.command_selected.unwrap_or(0);
        let command = matches
            .iter()
            .find(|candidate| candidate.command.id == value)
            .cloned()
            .or_else(|| matches.get(selected).cloned())
            .or_else(|| matches.first().cloned());
        let Some(command) = command else {
            self.set_toast("No extension commands are installed.", false);
            return;
        };
        self.run_command_match(command);
    }

    fn run_command_match(&mut self, command: CommandMatch) {
        let CommandMatch {
            extension,
            command: declaration,
        } = command;
        let context = serde_json::json!({
            "view": self.view.label(),
            "query": &self.query,
            "tags": &self.tags,
            "blocked_tags": &self.blocked_tags,
            "post": self.selected_post(),
        });
        // Read the title before moving the id into the request.
        let title = declaration.display_title().to_string();
        let params = serde_json::json!({"id": declaration.id, "context": context});
        let event_tx = self.event_tx.clone();
        let thread_title = title.clone();
        // A plugin can take its whole timeout; running it here would freeze
        // the terminal, so the result arrives as an event like feed work does.
        let spawned = thread::Builder::new()
            .name("dawngasm-command".into())
            .spawn(move || {
                let event = match extension.call(METHOD_COMMAND_INVOKE, params) {
                    Ok(value) => Event::CommandFinished {
                        message: value
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .map_or_else(
                                || format!("{thread_title} finished."),
                                ToString::to_string,
                            ),
                        is_error: value
                            .get("error")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    },
                    Err(error) => Event::CommandFinished {
                        message: format!("{thread_title}: {error}"),
                        is_error: true,
                    },
                };
                let _ = event_tx.send(event);
            });
        if let Err(error) = spawned {
            self.set_toast(format!("Could not start {title}: {error}"), true);
        } else {
            self.set_toast(format!("Running {title}…"), false);
        }
    }

    /// Opens a prompt with the current value, so a filter can be amended
    /// rather than retyped. Submitting an empty buffer clears it.
    fn begin_input(&mut self, mode: InputMode) {
        self.input_mode = mode;
        self.command_selected = None;
        self.input = Some(match mode {
            InputMode::Search => self.query.clone(),
            InputMode::Tags => self.tags.clone(),
            InputMode::BlockedTags => self.blocked_tags.clone(),
            InputMode::Command => String::new(),
        });
        self.input_changed();
    }

    fn end_input(&mut self) {
        self.input = None;
        self.command_selected = None;
        self.clear_suggestions();
    }

    fn submit_input(&mut self) {
        let value = self.input.take().unwrap_or_default().trim().to_string();
        if self.input_mode == InputMode::Command {
            // Close the palette before running, so the next key press is not
            // routed to the command handler.
            self.end_input();
            self.run_command(&value);
            return;
        }
        match self.input_mode {
            InputMode::Search => self.query = value,
            InputMode::Tags => self.tags = value,
            InputMode::BlockedTags => self.blocked_tags = value,
            InputMode::Command => {}
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

    const fn move_suggestion(&mut self, delta: isize) {
        if !self.suggestions_visible() {
            return;
        }
        let len = self.suggestions.len();
        self.suggestion_selected = Some(match self.suggestion_selected {
            Some(index) => wrap_index(index, delta, len),
            None if delta > 0 => 0,
            None => len - 1,
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
        isize::try_from(self.viewport_rows.max(1)).unwrap_or(isize::MAX)
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

    fn on_feed_loaded(
        &mut self,
        posts: Vec<Post>,
        has_more: bool,
        annotations: BTreeMap<String, PostAnnotations>,
        generation: u64,
    ) {
        if generation != self.load_generation {
            return;
        }
        self.has_more = has_more && self.page < MAX_PAGE_INDEX;
        self.feed_posts = posts;
        self.annotations = annotations;
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

    fn on_feed_failed(&mut self, message: &str, generation: u64) {
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
                Some(current) => current.wrapping_add_signed(delta),
                // No current track is not the end of the list: next starts at
                // the top and previous at the bottom.
                None if delta > 0 => 0,
                None => len - 1,
            };
            self.playlist.get(index).cloned()
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
        let percent = (self.volume * 100.0).round();
        self.set_toast(format!("Volume {percent:.0}%"), false);
    }

    fn toggle_mute(&mut self) {
        if let Some(previous) = self.muted_volume {
            self.apply_volume(previous);
            let percent = (previous * 100.0).round();
            self.set_toast(format!("Volume {percent:.0}%"), false);
        } else {
            let previous = self.volume;
            self.apply_volume(0.0);
            // `apply_volume` clears this, so set it afterwards.
            self.muted_volume = Some(previous);
            self.set_toast("Muted.", false);
        }
    }

    fn poll_audio(&mut self) {
        let events = self
            .engine
            .as_mut()
            .map_or_else(Vec::new, super::services::audio::AudioEngine::poll);
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

    // --- hotaudio clearance ------------------------------------------------

    /// Opens the system webview in a child process, waits for the Cloudflare
    /// challenge, and saves the cookie/UA pair for the next hotaudio resolve.
    fn capture_cloudflare_clearance(&mut self) {
        match crate::services::webview::run_clearance_process() {
            Ok(()) => self.set_toast(
                "Cloudflare clearance saved for hotaudio. Press Enter to retry.",
                false,
            ),
            Err(error) => self.set_toast(format!("Cloudflare clearance: {error}"), true),
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
            f64::from(u32::try_from(self.scroll).unwrap_or(u32::MAX)),
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
                    annotations,
                    generation,
                } => self.on_feed_loaded(posts, has_more, annotations, generation),
                Event::FeedFailed {
                    message,
                    generation,
                } => self.on_feed_failed(&message, generation),
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
                Event::ExtensionsLoaded { commands } => {
                    self.extension_commands = commands;
                }
                Event::CommandFinished { message, is_error } => {
                    self.set_toast(message, is_error);
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

/// Moves a highlighted index by `delta`, wrapping around `len` entries.
/// `len` must be non-zero; callers check that before stepping.
const fn wrap_index(current: usize, delta: isize, len: usize) -> usize {
    let step = delta.unsigned_abs() % len;
    if delta < 0 {
        (current + len - step) % len
    } else {
        (current + step) % len
    }
}

/// Converts a persisted scroll offset, saturating malformed values.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn saved_scroll_offset(value: f64) -> usize {
    // Session data is non-negative screen rows; saturating casts keep a
    // corrupt file from wrapping the offset.
    value.round().max(0.0) as usize
}

/// Fetches pages for the feed, one request at a time.
fn feed_worker(commands: &Receiver<Command>, events: &Sender<Event>) {
    let mut client = DawngasmClient::from_env();
    let extensions = Extensions::global();
    let discovered = extensions
        .commands()
        .into_iter()
        .map(|candidate| CommandMatch {
            extension: candidate.plugin.clone(),
            command: candidate.command.clone(),
        })
        .collect();
    if events
        .send(Event::ExtensionsLoaded {
            commands: discovered,
        })
        .is_err()
    {
        return;
    }
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
            } => match load_feed(
                &mut client,
                &extensions,
                &sort,
                &period,
                &tags,
                &exclude_tags,
                &query,
                page,
            ) {
                Ok((posts, has_more, annotations)) => Event::FeedLoaded {
                    posts,
                    has_more,
                    annotations,
                    generation,
                },
                Err(message) => Event::FeedFailed {
                    message,
                    generation,
                },
            },
            Command::SuggestTags { prefix, generation } => {
                let tags = suggest_tags(&mut client, &extensions, &prefix);
                Event::TagsSuggested { tags, generation }
            }
        };
        if events.send(event).is_err() {
            break;
        }
    }
}

/// A loaded page plus the annotations contributed by `post.enrich` plugins.
type LoadedFeed = (Vec<Post>, bool, BTreeMap<String, PostAnnotations>);

/// Loads one page through a `feed` extension when present, then applies the
/// filter and enrich hooks. Hooks are best-effort: a broken plugin leaves the
/// page usable rather than turning a feed load into an error.
#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)] // One feed pipeline.
fn load_feed(
    client: &mut DawngasmClient,
    extensions: &Extensions,
    sort: &str,
    period: &str,
    tags: &str,
    exclude_tags: &str,
    query: &str,
    page: usize,
) -> Result<LoadedFeed, String> {
    let plugin_feed = extensions.find(CAP_FEED).and_then(|plugin| {
        plugin
            .call(
                METHOD_FEED_FETCH,
                serde_json::json!({
                    "sort": sort,
                    "period": period,
                    "tags": tags,
                    "exclude_tags": exclude_tags,
                    "query": query,
                    "page": page,
                }),
            )
            .ok()
    });
    let (mut posts, has_more) = match plugin_feed.filter(|value| !extensions::is_unhandled(value)) {
        Some(value) => {
            let feed: FeedPage = serde_json::from_value(value)
                .map_err(|error| format!("feed extension returned an invalid page: {error}"))?;
            (feed.posts, feed.has_more)
        }
        None => client
            .fetch_posts(sort, period, tags, exclude_tags, query, page)
            .map_err(|error| error.to_string())?,
    };

    // Filters and enrichers chain in priority order. Each filter sees the
    // posts the previous one kept, and annotations from every enricher merge.
    for plugin in extensions.find_all(CAP_FILTER) {
        let Ok(value) = plugin.call(
            METHOD_FILTER_APPLY,
            serde_json::json!({
                "posts": &posts,
                "filters": {
                    "tags": tags,
                    "exclude_tags": exclude_tags,
                    "query": query,
                },
            }),
        ) else {
            continue;
        };
        if extensions::is_unhandled(&value) {
            continue;
        }
        let Ok(result) = serde_json::from_value::<FilterResult>(value) else {
            continue;
        };
        if let Some(keep) = result.keep {
            let keep: std::collections::HashSet<&str> = keep.iter().map(String::as_str).collect();
            posts.retain(|post| keep.contains(post.key()));
        }
    }

    let mut annotations: BTreeMap<String, PostAnnotations> = BTreeMap::new();
    for plugin in extensions.find_all(CAP_ENRICH) {
        let Ok(value) = plugin.call(METHOD_ENRICH_APPLY, serde_json::json!({"posts": &posts}))
        else {
            continue;
        };
        if extensions::is_unhandled(&value) {
            continue;
        }
        let Ok(result) = serde_json::from_value::<EnrichResult>(value) else {
            continue;
        };
        for (key, extra) in result.annotations {
            let entry = annotations.entry(key).or_default();
            for badge in extra.badges {
                if !entry.badges.contains(&badge) {
                    entry.badges.push(badge);
                }
            }
            for tag in extra.tags {
                if !entry.tags.contains(&tag) {
                    entry.tags.push(tag);
                }
            }
        }
    }

    Ok((posts, has_more, annotations))
}

/// Asks a `feed` extension for tag suggestions, falling back to the API.
fn suggest_tags(
    client: &mut DawngasmClient,
    extensions: &Extensions,
    prefix: &str,
) -> Vec<TagSuggestion> {
    extensions
        .find(CAP_FEED)
        .and_then(|plugin| {
            plugin
                .call(METHOD_FEED_TAGS, serde_json::json!({"prefix": prefix}))
                .ok()
        })
        .filter(|value| !extensions::is_unhandled(value))
        .and_then(|value| {
            let tags = value.get("tags").cloned().unwrap_or(value);
            serde_json::from_value::<Vec<TagSuggestion>>(tags).ok()
        })
        .unwrap_or_else(|| client.suggest_tags(prefix).unwrap_or_default())
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
    fn highlight_navigation_wraps_in_both_directions() {
        assert_eq!(wrap_index(0, -1, 3), 2);
        assert_eq!(wrap_index(0, 1, 3), 1);
        assert_eq!(wrap_index(2, 1, 3), 0);
        assert_eq!(wrap_index(1, -5, 3), 2);
        assert_eq!(wrap_index(1, 5, 3), 0);
    }

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
