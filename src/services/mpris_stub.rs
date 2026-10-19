//! Inert MPRIS on platforms without D-Bus.
//!
//! The desktop integration is Linux-only; this module keeps the same public
//! surface so [`crate::app`] does not need platform-specific branches.

use crate::models::post::Post;

/// A request from a media key or a panel. Never produced by this stub, but the
/// app applies the same variants on every platform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    SeekRelative(i64),
    SeekAbsolute(i64),
    SetVolume(f32),
    Quit,
}

/// Playback state as MPRIS reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackStatus {
    Stopped,
    Playing,
    Paused,
}

/// No D-Bus session on this platform: there is nothing to register.
pub struct Mpris;

impl Mpris {
    pub fn start() -> Option<Mpris> {
        None
    }

    pub fn update(
        &self,
        _status: PlaybackStatus,
        _position_ms: i64,
        _duration_ms: i64,
        _volume: f32,
        _can_seek: bool,
        _track: Option<&Post>,
    ) {
    }

    pub fn poll(&self) -> Vec<Command> {
        Vec::new()
    }
}
