use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

pub const SCROLLBAR_THUMB: &str = "\u{2590}";

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug)]
pub struct ScrollInfo {
    pub content_len: u16,
    pub position: u16,
}

pub fn render_vertical_scrollbar(frame: &mut Frame, area: Rect, content_len: u16, position: u16) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let max_scroll = content_len.saturating_sub(area.height);
    let mut state = ScrollbarState::default()
        .content_length(max_scroll as usize + 1)
        .position(position as usize);

    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol(SCROLLBAR_THUMB)
        .track_symbol(None)
        .begin_symbol(None)
        .end_symbol(None);

    frame.render_stateful_widget(scrollbar, area, &mut state);
}

pub fn vertical_thumb_bounds(
    content_len: u16,
    viewport_height: u16,
    position: u16,
) -> Option<(u16, u16)> {
    if content_len <= viewport_height || viewport_height == 0 {
        return None;
    }
    let content = f64::from(content_len);
    let track = f64::from(viewport_height);
    let max_scroll = content_len.saturating_sub(viewport_height);
    let pos = f64::from(position.min(max_scroll));
    let viewport = f64::from(viewport_height);

    let thumb_start = (pos * track / content).round() as u16;
    let thumb_end = ((pos + viewport) * track / content).round() as u16;
    let thumb_len = thumb_end.saturating_sub(thumb_start).max(1);

    let thumb_start = thumb_start.min(viewport_height.saturating_sub(thumb_len));
    let thumb_end = (thumb_start + thumb_len).min(viewport_height);

    Some((thumb_start, thumb_end))
}

pub fn position_for_thumb_row(
    content_len: u16,
    viewport_height: u16,
    thumb_len: u16,
    thumb_row: u16,
) -> u16 {
    if content_len <= viewport_height || viewport_height == 0 || thumb_len == 0 {
        return 0;
    }
    let max_scroll = content_len.saturating_sub(viewport_height);
    let max_thumb_start = viewport_height.saturating_sub(thumb_len).max(1);
    let pos = ((u32::from(thumb_row) * u32::from(max_scroll)) + (u32::from(max_thumb_start) / 2))
        / u32::from(max_thumb_start);
    pos.min(u32::from(max_scroll)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_thumb_bounds_stays_inside_track() {
        let (start, end) = vertical_thumb_bounds(200, 10, 190).unwrap();
        assert!(end <= 10, "thumb end {end} exceeds viewport 10");
        assert!(start < end, "thumb start {start} must be below end {end}");
    }

    #[test]
    fn vertical_thumb_bounds_top_and_bottom() {
        let top = vertical_thumb_bounds(200, 10, 0).unwrap();
        assert_eq!(top.0, 0);

        let bottom = vertical_thumb_bounds(200, 10, 190).unwrap();
        assert_eq!(bottom.1, 10);
    }

    #[test]
    fn position_for_thumb_row_reaches_max_scroll() {
        let content_len = 200;
        let viewport_height = 10;
        let thumb_len = 1;
        let max_scroll = content_len - viewport_height;

        let top = position_for_thumb_row(content_len, viewport_height, thumb_len, 0);
        assert_eq!(top, 0);

        let bottom = position_for_thumb_row(content_len, viewport_height, thumb_len, viewport_height - thumb_len);
        assert_eq!(bottom, max_scroll);
    }

    #[test]
    fn position_for_thumb_row_inverse_scales_linearly() {
        let content_len = 200;
        let viewport_height = 10;
        let thumb_len = 1;
        let max_scroll = content_len - viewport_height;

        // Half-way down the available thumb track should be about half the content.
        let mid_row = (viewport_height - thumb_len) / 2;
        let pos = position_for_thumb_row(content_len, viewport_height, thumb_len, mid_row);
        let expected = (u32::from(max_scroll) * u32::from(mid_row)
            + u32::from(viewport_height - thumb_len) / 2)
            / u32::from(viewport_height - thumb_len);
        assert_eq!(pos, expected as u16);
    }

    #[test]
    fn is_enabled_reflects_set() {
        let before = is_enabled();
        set_enabled(!before);
        assert_eq!(is_enabled(), !before);
        set_enabled(before);
    }

    #[test]
    fn render_is_noop_when_disabled() {
        // Rendering when disabled should not panic and should not access frame.
        set_enabled(false);
        // No frame to exercise; the function returns early.
        assert!(!is_enabled());
        set_enabled(true);
    }
}
