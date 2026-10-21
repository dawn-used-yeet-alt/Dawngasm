//! Colours and styles for the terminal UI.
//!
//! Everything is a truecolour value so it does not depend on a terminal theme,
//! but nothing load-bearing is colour-only: the current tab, the playing row
//! and the volume are all also marked with text or weight.

use ratatui::style::{Color, Modifier, Style};

use crate::models::audio_host::AudioHost;

/// Selected-row and heading colour.
pub const ACCENT: Color = Color::Rgb(0x6c, 0xc6, 0xff);
/// Dimmed accent, used for the inactive half of a heading.
pub const ACCENT_DIM: Color = Color::Rgb(0x3f, 0x6f, 0x8f);
/// Secondary text.
pub const MUTED: Color = Color::Rgb(0x8a, 0x8a, 0x8a);
/// Non-content chrome (track, separators).
pub const FAINT: Color = Color::Rgb(0x4a, 0x4a, 0x4a);
/// Success and the playing marker.
pub const GOOD: Color = Color::Rgb(0x8f, 0xd1, 0x7a);
/// Saved marker and warnings.
pub const WARN: Color = Color::Rgb(0xe8, 0xc0, 0x6a);
/// Failures.
pub const BAD: Color = Color::Rgb(0xe0, 0x7a, 0x7a);
/// The selected row's background.
pub const SELECT_BG: Color = Color::Rgb(0x24, 0x31, 0x3b);

#[must_use]
pub fn title() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

#[must_use]
pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

#[must_use]
pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

#[must_use]
pub fn selected() -> Style {
    Style::default().bg(SELECT_BG)
}

#[must_use]
pub fn playing() -> Style {
    Style::default().fg(GOOD).add_modifier(Modifier::BOLD)
}

#[must_use]
pub fn saved() -> Style {
    Style::default().fg(WARN)
}

#[must_use]
pub fn error() -> Style {
    Style::default().fg(BAD)
}

#[must_use]
pub fn toast() -> Style {
    Style::default().fg(ACCENT)
}

/// An active tab is bold and accented; the rest recede.
#[must_use]
pub fn tab(active: bool) -> Style {
    if active {
        Style::default()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default().fg(MUTED)
    }
}

/// The badge colour and short label for a host. The label is padded to a fixed
/// width by the list so the titles line up.
#[must_use]
pub fn host(host: AudioHost) -> (Style, &'static str) {
    match host {
        AudioHost::Soundgasm => (Style::default().fg(Color::Rgb(0xd9, 0x8b, 0x5a)), "sndgsm"),
        AudioHost::Whyp => (Style::default().fg(Color::Rgb(0xc0, 0x92, 0xe0)), "whyp"),
        AudioHost::Audiochan => (Style::default().fg(Color::Rgb(0x7a, 0xc8, 0xb0)), "audchan"),
        AudioHost::Hotaudio => (
            Style::default().fg(Color::Rgb(0xe0, 0x8a, 0x9a)),
            "hotaudio",
        ),
        AudioHost::Unknown => (Style::default().fg(FAINT), "—"),
    }
}

/// The fixed width every host badge is drawn at.
pub const HOST_BADGE_WIDTH: usize = 9;
