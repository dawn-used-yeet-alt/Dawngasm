//! Dawngasm TUI: terminal client for browsing and playing audio posts from the
//! dawngasm index.
//!
//! The crate is split so the headless pieces (models, host detection, parsers,
//! storage, the audio pipeline) are testable without a terminal or an audio
//! device:
//!
//! - [`models`] — `Post` and audio-host detection.
//! - [`services`] — the dawngasm API, per-host extractors, storage, and the
//!   player.
//! - [`ui`] — rendering, including the virtualised list and its scrollbar.
//! - [`app`] — the event loop tying it together.

pub mod app;
pub mod models;
pub mod services;
pub mod ui;

pub use app::App;
