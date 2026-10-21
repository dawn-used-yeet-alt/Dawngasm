//! Terminal rendering.
//!
//! The list is drawn by materialising only the rows that fit on screen: the
//! view description for the current view plus the window computed by
//! [`list::window`] decide the slice, so the cost of a frame does not grow with
//! the number of posts loaded. The scrollbar is drawn cell by cell from the
//! geometry in [`list::thumb`].

pub mod list;
pub mod theme;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, InputMode, View};
use crate::models::audio_host::{self, AudioHost};
use crate::services::audio::{format_time, PlayerStatus};

/// The always-visible key hints, shown when nothing more urgent is in the
/// footer.
const HINTS: &str = " ? help  / search  T tags  B block  Enter play  Space pause  n/p track  ←/→ seek  -/+ vol  s save  t sort  e window  Tab view  q quit";

pub fn draw(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(1),    // list
        Constraint::Length(2), // player bar
        Constraint::Length(1), // footer
    ])
    .split(frame.area());

    draw_header(frame, app, chunks[0]);

    let body = chunks[1];
    let total = app.rows().len();
    let visible = body.height as usize;
    app.viewport_rows = visible;
    if total == 0 {
        app.selected = 0;
        app.scroll = 0;
    } else {
        app.selected = app.selected.min(total - 1);
        app.scroll = app.scroll.min(total.saturating_sub(visible.max(1)));
    }

    let thumb = list::thumb(total, visible, app.scroll, body.height);
    match thumb {
        list::Thumb::Hidden => draw_list(frame, app, body),
        list::Thumb::Visible { start, len } => {
            let cols = Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).split(body);
            draw_list(frame, app, cols[0]);
            draw_scrollbar(frame, cols[1], start, len);
        }
    }

    draw_player(frame, app, chunks[2]);
    draw_footer(frame, app, chunks[3]);
    draw_suggestions(frame, app, frame.area());

    if app.show_help {
        draw_help(frame, frame.area());
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    for view in [View::Feed, View::Saved, View::History] {
        spans.push(Span::styled(
            format!(" {} ", view.label()),
            theme::tab(app.view == view),
        ));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled("sort ", theme::faint()));
    spans.push(Span::styled(app.sort_label(), theme::muted()));
    spans.push(Span::styled("(t)", theme::faint()));
    spans.push(Span::styled("  window ", theme::faint()));
    spans.push(Span::styled(app.period_label(), theme::muted()));
    spans.push(Span::styled("(e)", theme::faint()));
    spans.push(Span::styled(
        format!("  page {}", app.page + 1),
        theme::faint(),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        "[ prev ]",
        if app.can_prev_page() {
            theme::muted()
        } else {
            theme::faint()
        },
    ));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        "[ next ]",
        if app.can_next_page() {
            theme::muted()
        } else {
            theme::faint()
        },
    ));
    if !app.tags().is_empty() {
        spans.push(Span::styled(
            format!("  tags “{}”", app.tags()),
            theme::muted(),
        ));
    }
    if !app.blocked_tags().is_empty() {
        spans.push(Span::styled(
            format!("  blocked “{}”", app.blocked_tags()),
            theme::muted(),
        ));
    }
    if !app.query().is_empty() {
        spans.push(Span::styled(format!("  “{}”", app.query()), theme::muted()));
    }
    if app.loading {
        spans.push(Span::styled("  loading…", theme::title()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Renders the visible slice of the current view's rows.
fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let total = app.rows().len();
    let win = list::window(total, area.height as usize, app.scroll);
    let rows = app.rows();
    let mut lines: Vec<Line> = Vec::with_capacity(win.count);
    for index in win.start..win.start + win.count {
        let Some(post) = rows.get(index) else {
            break;
        };
        let selected = index == app.selected;
        lines.push(row_line(
            post,
            area.width as usize,
            app.is_playing(post),
            app.is_saved(post),
            selected,
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// A score that fits the fixed four-column slot, so rows stay aligned even when
/// a post has thousands of upvotes.
fn compact_score(score: i64) -> String {
    let sign = if score < 0 { "-" } else { "" };
    let magnitude = score.unsigned_abs();
    let body = match magnitude {
        0..=999 => magnitude.to_string(),
        1_000..=9_999 => format!("{:.1}k", magnitude as f64 / 1_000.0),
        10_000..=999_999 => format!("{}k", magnitude / 1_000),
        _ => "999k".to_string(),
    };
    // ASCII only, so truncating by bytes is safe and keeps the slot fixed.
    let mut out = format!("{sign}{body}");
    out.truncate(out.len().min(4));
    out
}

/// One post row, laid out so the author stays on the right even for a long
/// title, and truncated when there genuinely is not room. The score leads the
/// row, so the upvotes are the first thing the eye lands on.
fn row_line(
    post: &crate::models::post::Post,
    width: usize,
    playing: bool,
    saved: bool,
    selected: bool,
) -> Line<'static> {
    let host = audio_host::detect_host(post);
    let (host_style, label) = theme::host(host);
    let badge = format!("{label:<badge$}", badge = theme::HOST_BADGE_WIDTH);
    // score(6) + marker(1) + space(1) + saved(1) + space(1) + badge(9) + space(1)
    let prefix_cols = 20;
    let score = format!("▲{:<4} ", compact_score(post.score));
    let marker = if playing { "▶" } else { " " };
    let saved_mark = if saved { "♥" } else { " " };

    let meta = format!("u/{}", post.author);
    let meta_cols = meta.chars().count();
    let show_meta = width >= prefix_cols + meta_cols + 2;
    let title_cols = if show_meta {
        width - prefix_cols - meta_cols - 1
    } else {
        width.saturating_sub(prefix_cols)
    };
    let title = truncate(&post.title, title_cols.max(1));

    let title_style = if playing {
        theme::playing()
    } else {
        Style::default()
    };

    let mut spans = vec![
        Span::styled(score, theme::muted()),
        Span::styled(
            format!("{marker} "),
            if playing {
                theme::playing()
            } else {
                theme::faint()
            },
        ),
        Span::styled(
            format!("{saved_mark} "),
            if saved {
                theme::saved()
            } else {
                theme::faint()
            },
        ),
        Span::styled(format!("{badge} "), host_style),
    ];
    if show_meta {
        let pad = " ".repeat(title_cols.saturating_sub(title.chars().count()));
        spans.push(Span::styled(title, title_style));
        spans.push(Span::styled(format!("{pad}{meta} "), theme::muted()));
    } else {
        spans.push(Span::styled(title, title_style));
    }

    let style = if selected {
        theme::selected()
    } else {
        Style::default()
    };
    Line::from(spans).style(style)
}

fn draw_scrollbar(frame: &mut Frame, area: Rect, start: u16, len: u16) {
    let buffer = frame.buffer_mut();
    for y in 0..area.height {
        let in_thumb = y >= start && y < start.saturating_add(len);
        let (symbol, style) = if in_thumb {
            ("█", theme::title())
        } else {
            ("│", theme::faint())
        };
        buffer.set_string(area.x, area.y + y, symbol, style);
    }
}

fn draw_player(frame: &mut Frame, app: &App, area: Rect) {
    let status = app.engine_status();
    let icon = match status {
        PlayerStatus::Idle => "■",
        PlayerStatus::Loading => "◌",
        PlayerStatus::Playing => "▶",
        PlayerStatus::Paused => "❚",
    };
    let icon_style = match status {
        PlayerStatus::Playing => theme::playing(),
        PlayerStatus::Paused => Style::default()
            .fg(theme::WARN)
            .add_modifier(Modifier::BOLD),
        PlayerStatus::Loading => theme::muted(),
        PlayerStatus::Idle => theme::faint(),
    };
    let current = app.playing_post();

    let title_line = match current {
        Some(post) => Line::from(vec![
            Span::styled(format!("{icon} "), icon_style),
            Span::styled(post.title.clone(), theme::title()),
            Span::styled(format!("  — u/{}", post.author), theme::muted()),
        ]),
        None => Line::from(vec![
            Span::styled(format!("{icon} "), icon_style),
            Span::styled("Nothing playing", theme::muted()),
        ]),
    };

    let progress_line = if matches!(status, PlayerStatus::Idle) {
        Line::from(Span::styled(
            "   select a post and press Enter to play",
            theme::faint(),
        ))
    } else {
        progress_line(
            app,
            area.width as usize,
            current.map(audio_host::detect_host),
        )
    };

    frame.render_widget(Paragraph::new(vec![title_line, progress_line]), area);
}

fn progress_line(app: &App, width: usize, host: Option<AudioHost>) -> Line<'static> {
    let position = format_time(app.position_ms());
    let duration = if app.duration_ms() > 0 {
        format_time(app.duration_ms())
    } else {
        "--:--".to_string()
    };
    let volume = format!("{:>3}%", (app.volume * 100.0).round() as i32);
    let host_label = host.map_or("—", |host| theme::host(host).1).to_string();

    let fixed = position.chars().count()
        + duration.chars().count()
        + volume.chars().count()
        + host_label.chars().count()
        + 7; // separators
    let bar_width = width.saturating_sub(fixed);
    let bar = progress_bar(
        bar_width,
        app.position_ms(),
        app.duration_ms(),
        app.is_seekable(),
    );

    Line::from(vec![
        Span::styled(format!("  {position} "), theme::muted()),
        Span::styled(bar, theme::title()),
        Span::styled(format!(" {duration}   "), theme::muted()),
        Span::styled(
            volume,
            if app.volume == 0.0 {
                theme::faint()
            } else {
                theme::muted()
            },
        ),
        Span::styled(format!("   {host_label}"), theme::faint()),
    ])
}

/// A progress bar `width` cells wide with a rounded playhead. A non-seekable
/// track shows an empty track rather than a false position.
fn progress_bar(width: usize, position_ms: i64, duration_ms: i64, seekable: bool) -> String {
    if width == 0 {
        return String::new();
    }
    if !seekable || duration_ms <= 0 {
        return "─".repeat(width);
    }
    let fraction = (position_ms.max(0) as f64 / duration_ms as f64).clamp(0.0, 1.0);
    let filled = ((width as f64) * fraction).round() as usize;
    let filled = filled.min(width);
    let mut bar = String::with_capacity(width);
    for cell in 0..width {
        if filled == 0 {
            bar.push('─');
        } else if cell + 1 < filled {
            bar.push('█');
        } else if cell + 1 == filled {
            bar.push('●');
        } else {
            bar.push('─');
        }
    }
    bar
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = if let Some(buffer) = &app.input {
        let label = match app.input_mode {
            InputMode::Search => "Search: ",
            InputMode::Tags => "Tags: ",
            InputMode::BlockedTags => "Blocked tags: ",
        };
        let hint = if app.suggestions_visible() {
            "   ↑/↓ select · Tab complete · Enter submit · Esc hide"
        } else {
            "   Enter submit · Esc cancel"
        };
        Line::from(vec![
            Span::styled(label, theme::title()),
            Span::raw(buffer.clone()),
            Span::styled("▏", theme::title()),
            Span::styled(hint, theme::faint()),
        ])
    } else if let Some(toast) = &app.toast {
        let style = if app.toast_error {
            theme::error()
        } else {
            theme::toast()
        };
        Line::from(Span::styled(format!(" {toast}"), style))
    } else if !app.status.is_empty() {
        let style = if app.status_is_error {
            theme::error()
        } else {
            theme::muted()
        };
        Line::from(Span::styled(format!(" {}", app.status), style))
    } else {
        Line::from(Span::styled(HINTS, theme::faint()))
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Draws the tag autocomplete dropdown just above the footer. It is transient
/// chrome, so it overlays the player bar rather than reflowing the list.
fn draw_suggestions(frame: &mut Frame, app: &App, area: Rect) {
    if !app.suggestions_visible() {
        return;
    }
    let suggestions = app.suggestions();
    let height = (suggestions.len() as u16 + 2).min(area.height);
    let width = area.width.min(52);
    let popup = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(1 + height),
        width,
        height,
    };
    let inner_width = width.saturating_sub(2) as usize;
    let lines: Vec<Line> = suggestions
        .iter()
        .enumerate()
        .map(|(index, suggestion)| {
            let count = suggestion.count.to_string();
            let tag_width = inner_width.saturating_sub(count.chars().count() + 2);
            let tag = truncate(&suggestion.tag, tag_width);
            let padding = inner_width.saturating_sub(tag.chars().count() + count.chars().count());
            let style = if app.suggestion_selected() == Some(index) {
                theme::selected().add_modifier(Modifier::BOLD)
            } else {
                theme::muted()
            };
            Line::from(Span::styled(
                format!("{tag}{}{count}", " ".repeat(padding)),
                style,
            ))
        })
        .collect();
    let block = ratatui::widgets::Block::bordered()
        .title(if app.input_mode == InputMode::BlockedTags {
            " Blocked tags "
        } else {
            " Tags "
        })
        .border_style(theme::title());
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let keys: [(&str, &str); 23] = [
        ("j / k, ↑ / ↓", "move the selection"),
        ("g / G, Home / End", "jump to the first or last post"),
        ("PgDn / PgUp, Ctrl-d / Ctrl-u", "move a page at a time"),
        ("Enter", "play the selected post"),
        ("Space", "pause or resume"),
        ("n / p", "next or previous track in the list"),
        ("← / →, h / l", "seek back or forward 5s (Shift: 10s)"),
        (", / .", "seek back or forward 5s"),
        ("- / +", "volume down or up 5%"),
        ("0", "reset the volume to 100%"),
        ("m", "mute or unmute"),
        ("s", "save or unsave the selected post"),
        ("Tab, Shift-Tab", "cycle Feed → Saved → History"),
        ("f / v / y", "go straight to Feed / Saved / History"),
        ("/", "search every indexed subreddit"),
        ("T", "edit tag filters with /tags autocomplete"),
        ("B", "block tags with /tags autocomplete"),
        ("[ / ]", "previous or next page"),
        ("t", "cycle the sort (top → new → relevance)"),
        ("e", "cycle the time window (switches sort to top)"),
        ("r", "reload the current page"),
        ("x / i", "export / import the library file"),
        ("q, Ctrl-c", "quit"),
    ];
    // +2 for the border, +2 so a wrapped row never pushes the last key out.
    let popup = centered_rect(64, keys.len() as u16 + 4, area);
    frame.render_widget(Clear, popup);
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, action)| {
            Line::from(vec![
                Span::styled(format!("  {key:<30}"), theme::title()),
                Span::styled(*action, theme::muted()),
            ])
        })
        .collect();
    let block = ratatui::widgets::Block::bordered()
        .title(" Keys ")
        .border_style(theme::title());
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        popup,
    );
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = (area.width.saturating_mul(percent_x) / 100).min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// Clips `text` to `max` characters, adding an ellipsis when it does not fit.
fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_adds_an_ellipsis_only_when_needed() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn compact_score_never_exceeds_the_slot() {
        assert_eq!(compact_score(0), "0");
        assert_eq!(compact_score(999), "999");
        assert_eq!(compact_score(1_200), "1.2k");
        assert_eq!(compact_score(12_400), "12k");
        assert_eq!(compact_score(999_999), "999k");
        assert_eq!(compact_score(5_000_000), "999k");
        assert_eq!(compact_score(5_000_000).chars().count(), 4);
        // Downvoted posts must not overrun the slot either.
        assert_eq!(compact_score(-12), "-12");
        assert_eq!(compact_score(-1_500), "-1.5");
        assert_eq!(compact_score(-5_000_000), "-999");
    }

    #[test]
    fn progress_bar_places_a_single_playhead() {
        let bar = progress_bar(10, 0, 1000, true);
        assert_eq!(bar.chars().count(), 10);
        assert_eq!(
            bar.matches('●').count(),
            0,
            "start has no playhead cell: {bar}"
        );
        let bar = progress_bar(10, 500, 1000, true);
        assert_eq!(bar.matches('●').count(), 1);
        assert_eq!(bar.chars().count(), 10);
        let bar = progress_bar(10, 1000, 1000, true);
        assert_eq!(bar.matches('●').count(), 1);
        assert_eq!(bar.chars().count(), 10);
    }

    #[test]
    fn progress_bar_is_flat_when_not_seekable() {
        assert_eq!(progress_bar(5, 3, 0, false), "─────");
        assert_eq!(progress_bar(0, 3, 100, true), "");
    }

    #[test]
    fn centered_rect_stays_inside_the_area() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let rect = centered_rect(64, 22, area);
        assert_eq!(rect.width, 64);
        assert_eq!(rect.height, 22);
        assert_eq!(rect.x, 18);
        assert_eq!(rect.y, 9);
        // A tiny terminal still yields a rect inside itself.
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 3,
        };
        let rect = centered_rect(64, 22, tiny);
        assert!(rect.width <= tiny.width && rect.height <= tiny.height);
        assert!(rect.x + rect.width <= tiny.width && rect.y + rect.height <= tiny.height);
    }
}
