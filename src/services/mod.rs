//! Services: network (the dawngasm API + host extractors), on-disk state, and
//! playback.
//!
//! Everything here is designed to run off the UI thread. The terminal loop only
//! ever sees values that have already been produced; nothing blocks it.

pub mod audio;
pub mod dawngasm;
pub mod extensions;
pub mod extractors;
pub mod hotaudio_auth;
#[cfg(target_os = "linux")]
pub mod mpris;
#[cfg(not(target_os = "linux"))]
#[path = "mpris_stub.rs"]
pub mod mpris;
pub mod storage;
pub mod webview;
