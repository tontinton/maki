use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

pub const SCROLLBAR_THUMB: &str = "\u{2590}";

static ENABLED: AtomicBool = AtomicBool::new(true);

thread_local! {
    static PAINTED_RAILS: RefCell<Vec<Rect>> = const { RefCell::new(Vec::new()) };
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Lets a caller skip counting rows it would only hand to a scrollbar nobody
/// draws. Worth asking when the count is not already lying around.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Reset the painted-rail record. Called once at the top of each `App::view`
/// so the record only ever describes the frame currently being drawn.
pub fn begin_frame() {
    PAINTED_RAILS.with(|rails| rails.borrow_mut().clear());
}

/// Cells scrollbars painted over during this frame. Overlay zones register
/// content rects that can overlap a rail, so copy skips these cells no matter
/// where a widget decided to draw (#917).
pub fn painted_rails() -> Vec<Rect> {
    PAINTED_RAILS.with(|rails| rails.borrow().clone())
}

pub fn render_vertical_scrollbar(frame: &mut Frame, area: Rect, content_len: u32, position: u32) {
    let area = area.intersection(frame.area());
    if !is_enabled() || area.is_empty() {
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

    let rail = area.right() - 1;
    let before: Vec<String> = (area.y..area.bottom())
        .map(|row| frame.buffer_mut()[(rail, row)].symbol().to_string())
        .collect();
    frame.render_stateful_widget(scrollbar, area, &mut state);
    let painted = (area.y..area.bottom())
        .enumerate()
        .filter(|(i, row)| frame.buffer_mut()[(rail, *row)].symbol() != before[*i])
        .map(|(_, row)| Rect::new(rail, row, 1, 1));
    PAINTED_RAILS.with(|rails| rails.borrow_mut().extend(painted));
}
