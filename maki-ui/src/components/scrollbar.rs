use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::Frame;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget};

pub const SCROLLBAR_THUMB: &str = "\u{2590}";
/// ListPicker paints highlighted rows across the scrollbar column. Resetting the
/// style keeps the thumb color steady instead of picking up the row background.
const THUMB_STYLE: Style = Style::new().fg(Color::Reset).bg(Color::Reset);

static ENABLED: AtomicBool = AtomicBool::new(true);

thread_local! {
    /// Scrollbars are drawn deep inside components that have no way to hand
    /// anything back, so they report here. Outside `Thumbs::record` it stays
    /// empty and nothing is kept.
    static RECORDING: RefCell<Option<Thumbs>> = const { RefCell::new(None) };
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Lets a caller skip counting rows it would only hand to a scrollbar nobody
/// draws. Worth asking when the count is not already lying around.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// A thumb is painted on the last column of its content, often right on top
/// of text. Selection copies text straight off the frame, so it would grab the
/// thumb glyph and lose the letter under it. Each painted thumb keeps the cell
/// it hid, so the copy can get that letter back.
#[derive(Debug, Default)]
pub struct Thumbs(HashMap<Position, Cell>);

impl Thumbs {
    pub fn record<R>(draw: impl FnOnce() -> R) -> (R, Self) {
        RECORDING.set(Some(Self::default()));
        let drawn = draw();
        (drawn, RECORDING.take().unwrap_or_default())
    }

    pub fn covers(&self, buf: &Buffer, pos: Position) -> bool {
        buf.cell(pos).is_some_and(|cell| self.shows(cell, pos))
    }

    /// The frame as if no thumb was drawn, which is what a copy should read.
    pub fn uncovered(&self, buf: &Buffer) -> Buffer {
        let mut out = buf.clone();
        for (&pos, beneath) in &self.0 {
            if self.covers(buf, pos) {
                out[pos] = beneath.clone();
            }
        }
        out
    }

    /// Something drawn later, like a popup, can paint over a thumb. Then its
    /// text is what the user sees, and checking the glyph catches that.
    fn shows(&self, cell: &Cell, pos: Position) -> bool {
        cell.symbol() == SCROLLBAR_THUMB && self.0.contains_key(&pos)
    }

    /// Call before painting the thumb. When two scrollbars hit the same cell,
    /// the first one already saved the real content, so we keep that.
    fn cover(&mut self, cell: &Cell, pos: Position) {
        if !self.shows(cell, pos) {
            self.0.insert(pos, cell.clone());
        }
    }
}

pub fn render_vertical_scrollbar(frame: &mut Frame, area: Rect, content_len: u32, position: u32) {
    if !is_enabled() {
        return;
    }
    let Some(column) = area.columns().next_back() else {
        return;
    };
    let max_scroll = content_len.saturating_sub(u32::from(area.height));
    let mut state = ScrollbarState::default()
        .content_length(max_scroll as usize + 1)
        .position(position as usize);

    // ratatui works out where the thumb goes on a blank scratch column. Then
    // we paint it ourselves, so we can save each cell before it gets covered.
    let mut probe = Buffer::empty(column);
    Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol(SCROLLBAR_THUMB)
        .track_symbol(None)
        .begin_symbol(None)
        .end_symbol(None)
        .render(column, &mut probe, &mut state);

    let thumb = column
        .positions()
        .filter(|&pos| probe[pos].symbol() == SCROLLBAR_THUMB);
    let buf = frame.buffer_mut();
    RECORDING.with_borrow_mut(|recording| {
        for pos in thumb {
            let Some(cell) = buf.cell_mut(pos) else {
                continue;
            };
            if let Some(thumbs) = recording {
                thumbs.cover(cell, pos);
            }
            cell.set_symbol(SCROLLBAR_THUMB).set_style(THUMB_STYLE);
        }
    });
}

#[cfg(test)]
pub(crate) mod test_support {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Position;
    use ratatui::{Frame, Terminal};

    use super::{Thumbs, render_vertical_scrollbar};

    const CONTENT_SCREENS: u32 = 4;

    pub(crate) fn render_scrollbar(frame: &mut Frame) {
        let area = frame.area();
        render_vertical_scrollbar(frame, area, u32::from(area.height) * CONTENT_SCREENS, 0);
    }

    pub(crate) fn thumb_cells(buf: &Buffer, thumbs: &Thumbs) -> Vec<Position> {
        buf.area
            .positions()
            .filter(|&pos| thumbs.covers(buf, pos))
            .collect()
    }

    /// Panics if no thumb got painted, so a test can't pass just because there
    /// was nothing to check.
    pub(crate) fn draw_recorded(content: Buffer, scrollbars: usize) -> (Buffer, Thumbs) {
        let area = content.area;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let (_, thumbs) = Thumbs::record(|| {
            terminal
                .draw(|frame| {
                    *frame.buffer_mut() = content;
                    for _ in 0..scrollbars {
                        render_scrollbar(frame);
                    }
                })
                .unwrap()
        });
        let buf = terminal.backend().buffer().clone();
        assert!(!thumb_cells(&buf, &thumbs).is_empty(), "no thumb painted");
        (buf, thumbs)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{draw_recorded, thumb_cells};
    use super::*;

    const WIDTH: u16 = 4;
    const HEIGHT: u16 = 10;
    const FILL: &str = "x";

    fn filled(symbol: &'static str) -> Buffer {
        Buffer::filled(Rect::new(0, 0, WIDTH, HEIGHT), Cell::new(symbol))
    }

    /// Here the content itself is full of thumb glyphs, so only the recording
    /// can tell which ones the scrollbar painted. The plain run shows where
    /// ratatui puts the thumb.
    #[test]
    fn only_painted_thumbs_count() {
        let (plain, _) = draw_recorded(filled(FILL), 1);
        let painted: Vec<Position> = plain
            .area
            .positions()
            .filter(|&pos| plain[pos].symbol() == SCROLLBAR_THUMB)
            .collect();
        let (buf, thumbs) = draw_recorded(filled(SCROLLBAR_THUMB), 1);
        assert_eq!(thumb_cells(&buf, &thumbs), painted);
    }

    #[test]
    fn text_drawn_over_a_thumb_wins() {
        let (mut buf, thumbs) = draw_recorded(filled(SCROLLBAR_THUMB), 1);
        let pos = thumb_cells(&buf, &thumbs)[0];
        buf[pos].set_symbol(FILL);
        assert!(!thumbs.covers(&buf, pos));
        assert_eq!(thumbs.uncovered(&buf)[pos].symbol(), FILL);
    }

    #[test]
    fn stacked_thumbs_uncover_to_the_content_beneath() {
        let (buf, thumbs) = draw_recorded(filled(FILL), 2);
        assert_eq!(thumbs.uncovered(&buf), filled(FILL));
    }
}
