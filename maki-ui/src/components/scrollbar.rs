use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

pub const SCROLLBAR_THUMB: &str = "\u{2590}";

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
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
    Some((thumb_start, thumb_start.saturating_add(thumb_len)))
}

pub fn position_for_thumb_row(
    content_len: u16,
    viewport_height: u16,
    thumb_row: u16,
) -> u16 {
    if content_len <= viewport_height || viewport_height == 0 {
        return 0;
    }
    let max_scroll = content_len.saturating_sub(viewport_height);
    let pos = (f64::from(thumb_row) * f64::from(content_len) / f64::from(viewport_height)).round()
        as u16;
    pos.min(max_scroll)
}
