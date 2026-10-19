//! Row windowing and a proportional scrollbar.
//!
//! Both are pure integer geometry with no terminal types, so they can be tested
//! directly. The scrollbar is deliberately not ratatui's `Scrollbar`: that one
//! sizes its thumb from the item count *and* a separate content-length, which
//! for a plain list is easy to get subtly wrong (an off-by-one that makes the
//! thumb never reach the ends). This one maps the item offset linearly onto the
//! track and always yields a thumb of at least one cell, so the position is
//! readable even when the viewport is a large fraction of the list.

/// The slice of rows to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: usize,
    pub count: usize,
}

/// The rows visible given a viewport height and a top row, clamped so the last
/// screenful ends at the last row rather than scrolling past it.
pub fn window(total: usize, visible: usize, offset: usize) -> Window {
    if total == 0 || visible == 0 {
        return Window { start: 0, count: 0 };
    }
    let count = visible.min(total);
    let max_start = total - count;
    Window {
        start: offset.min(max_start),
        count,
    }
}

/// A scrollbar thumb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thumb {
    /// The whole list fits; there is nothing to scrollbar.
    Hidden,
    /// `len` cells starting at `start` along a track of the height it was asked
    /// for.
    Visible { start: u16, len: u16 },
}

/// Maps the item offset onto a `track`-cell scrollbar.
///
/// The thumb grows with the visible fraction and shrinks to a single cell when
/// almost everything is off screen. `offset` is clamped to the scrollable range
/// first, which is what makes the thumb sit flush against both ends.
pub fn thumb(total: usize, visible: usize, offset: usize, track: u16) -> Thumb {
    if track == 0 || total == 0 || visible == 0 || total <= visible {
        return Thumb::Hidden;
    }
    let track = track as u64;
    let len = (track * visible as u64 / total as u64).clamp(1, track);
    let max_start = track - len;
    let scrollable = (total - visible) as u64;
    let offset = (offset as u64).min(scrollable);
    let start = (max_start * offset).checked_div(scrollable).unwrap_or(0);
    Thumb::Visible {
        start: start as u16,
        len: len as u16,
    }
}

/// Scrolls `offset` just far enough to include `selected`.
pub fn ensure_visible(offset: &mut usize, selected: usize, visible: usize, total: usize) {
    if total == 0 {
        *offset = 0;
        return;
    }
    // With no viewport measured yet there is nothing to keep in view; leave the
    // offset alone rather than discarding a restored position.
    if visible == 0 {
        return;
    }
    if selected < *offset {
        *offset = selected;
    } else if selected >= *offset + visible {
        *offset = selected + 1 - visible;
    }
    *offset = (*offset).min(total.saturating_sub(visible));
}

/// Moves `selected` by `delta` rows within `total`, clamping to the ends.
pub fn step(selected: usize, delta: isize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let last = total - 1;
    let next = selected as isize + delta;
    next.clamp(0, last as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_clamps_both_ends() {
        assert_eq!(
            window(100, 10, 0),
            Window {
                start: 0,
                count: 10
            }
        );
        assert_eq!(
            window(100, 10, 95),
            Window {
                start: 90,
                count: 10
            }
        );
        assert_eq!(window(4, 10, 0), Window { start: 0, count: 4 });
        assert_eq!(window(0, 10, 5), Window { start: 0, count: 0 });
    }

    #[test]
    fn thumb_is_hidden_when_everything_fits() {
        assert_eq!(thumb(10, 10, 0, 20), Thumb::Hidden);
        assert_eq!(thumb(3, 10, 0, 20), Thumb::Hidden);
        assert_eq!(thumb(0, 10, 0, 20), Thumb::Hidden);
        assert_eq!(thumb(100, 10, 0, 0), Thumb::Hidden);
    }

    #[test]
    fn thumb_is_proportional_and_reaches_both_ends() {
        // Half the list visible: the thumb is half the track.
        assert_eq!(thumb(100, 50, 0, 20), Thumb::Visible { start: 0, len: 10 });
        // At the bottom offset the thumb is flush with the end.
        assert_eq!(
            thumb(100, 50, 50, 20),
            Thumb::Visible { start: 10, len: 10 }
        );
        // A tiny viewport yields a one-cell thumb that still traverses fully.
        assert_eq!(thumb(1000, 1, 0, 20), Thumb::Visible { start: 0, len: 1 });
        assert_eq!(
            thumb(1000, 1, 999, 20),
            Thumb::Visible { start: 19, len: 1 }
        );
    }

    #[test]
    fn thumb_never_overruns_the_track() {
        for total in 1..200 {
            for visible in 1..total.max(1) {
                for offset in [0, 1, total / 2, total, total + 5] {
                    if let Thumb::Visible { start, len } = thumb(total, visible, offset, 17) {
                        assert!(len >= 1, "len {len}");
                        assert!(start + len <= 17, "start {start} len {len} track 17");
                    }
                }
            }
        }
    }

    #[test]
    fn ensure_visible_scrolls_only_when_needed() {
        let mut offset = 0;
        ensure_visible(&mut offset, 3, 10, 100);
        assert_eq!(offset, 0);
        ensure_visible(&mut offset, 15, 10, 100);
        assert_eq!(offset, 6);
        ensure_visible(&mut offset, 2, 10, 100);
        assert_eq!(offset, 2);
        // The last row cannot scroll past the end.
        ensure_visible(&mut offset, 99, 10, 100);
        assert_eq!(offset, 90);
    }

    #[test]
    fn ensure_visible_keeps_offset_without_a_viewport() {
        // The first feed load can land before the first draw has measured the
        // list; a restored position must survive that.
        let mut offset = 42;
        ensure_visible(&mut offset, 0, 0, 100);
        assert_eq!(offset, 42);
    }

    #[test]
    fn step_stays_in_range() {
        assert_eq!(step(0, -1, 10), 0);
        assert_eq!(step(9, 1, 10), 9);
        assert_eq!(step(5, 3, 10), 8);
        assert_eq!(step(0, 1, 0), 0);
    }
}
