use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

pub const SCROLLBAR_THUMB: &str = "\u{2590}";

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Lets a caller skip counting rows it would only hand to a scrollbar nobody
/// draws. Worth asking when the count is not already lying around.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn render_vertical_scrollbar(frame: &mut Frame, area: Rect, content_len: u32, position: u32) {
    if !is_enabled() {
        return;
    }
    let max_scroll = content_len.saturating_sub(u32::from(area.height));
    let mut state = ScrollbarState::default()
        .content_length(max_scroll as usize + 1)
        .position(position as usize);

    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol(SCROLLBAR_THUMB)
        // ListPicker renders highlighted rows over the scrollbar track; resetting
        // the thumb style keeps its color stable instead of inheriting row bg.
        .thumb_style(Style::new().fg(Color::Reset).bg(Color::Reset))
        .track_symbol(None)
        .begin_symbol(None)
        .end_symbol(None);

    frame.render_stateful_widget(scrollbar, area, &mut state);
}

/// Rail cells of `area` that still hold the thumb glyph in `buf`, one per row
/// of the thumb's span. Copy runs on the same buffer the user sees, so a rail
/// a modal later painted over stops excluding once its cells show text again
/// (#917).
pub fn live_rails(buf: &Buffer, area: Rect) -> Vec<Rect> {
    let area = area.intersection(*buf.area());
    let Some(rail) = area.width.checked_sub(1).map(|w| area.x + w) else {
        return Vec::new();
    };
    (area.y..area.bottom())
        .filter(|&row| buf[(rail, row)].symbol() == SCROLLBAR_THUMB)
        .map(|row| Rect::new(rail, row, 1, 1))
        .collect()
}
