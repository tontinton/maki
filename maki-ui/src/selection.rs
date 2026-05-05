//! Mouse selection + clipboard copy.
//!
//! Enabling mouse capture (for scroll) kills the terminal's native text
//! selection, so we reimplement it here. A few things that are easy to get
//! wrong:
//!
//! Positions are stored in doc space (`DocPos`), not screen space, because
//! screen coords go stale the moment the user scrolls.
//!
//! Copy runs inside `view()`, not on mouse-up. The terminal buffer only
//! holds valid cell data during rendering, so that is the only moment we
//! can scrape the text. Code bar prefixes (`│ `) are stripped during
//! scraping so you get clean source.
//!
//! While a selection is active, `has_selection` freezes auto-scroll in the
//! messages panel so the viewport stays put while the user drags.
//!
//! The rightmost column of each area is reserved for the scrollbar, so
//! `highlight_area` / `msg_area()` are 1 column narrower than the full
//! area and all column math uses `width - 1`.

use std::cmp::Ordering;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::markdown::{CODE_BAR, CODE_BAR_WRAP};
use crate::theme;

/// Position in doc space (full logical document, not just visible window).
/// Stored as (row, col) where col is a screen x coordinate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocPos {
    pub row: u32,
    pub col: u16,
}

impl DocPos {
    fn new(row: u32, col: u16) -> Self {
        Self { row, col }
    }
}

impl PartialOrd for DocPos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DocPos {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.row, self.col).cmp(&(other.row, other.col))
    }
}

/// Selection is locked to one zone for its entire lifetime.
///
/// Variant order matters: higher index = higher z-order priority in `zone_at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionZone {
    Messages,
    Input,
    StatusBar,
    Overlay,
}

impl SelectionZone {
    pub const COUNT: usize = 4;

    pub const fn idx(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SelectableZone {
    pub area: Rect,
    pub highlight_area: Rect,
    pub zone: SelectionZone,
}

pub type ZoneRegistry = [Option<SelectableZone>; SelectionZone::COUNT];

/// Returns the zone at `(row, col)`, preferring higher-index (higher z-order) zones.
pub fn zone_at(zones: &ZoneRegistry, row: u16, col: u16) -> Option<SelectableZone> {
    let pos = ratatui::layout::Position::new(col, row);
    zones
        .iter()
        .rev()
        .flatten()
        .find(|z| z.area.contains(pos))
        .copied()
}

/// Anchor + cursor in doc space. `area` and `zone` are captured at mouse-down
/// and stay fixed so layout changes mid-drag don't break the selection.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    anchor: DocPos,
    cursor: DocPos,
    pub area: Rect,
    pub zone: SelectionZone,
}

fn screen_to_doc(screen_row: u16, area: Rect, scroll_offset: u32) -> u32 {
    let clamped = screen_row.clamp(area.y, area.y + area.height.saturating_sub(1));
    scroll_offset + (clamped - area.y) as u32
}

fn clamp_col(col: u16, area: Rect) -> u16 {
    col.clamp(area.x, area.x + area.width.saturating_sub(1))
}

impl Selection {
    pub fn start(row: u16, col: u16, area: Rect, zone: SelectionZone, scroll_offset: u32) -> Self {
        let doc_row = screen_to_doc(row, area, scroll_offset);
        let doc_col = clamp_col(col, area);
        let pos = DocPos::new(doc_row, doc_col);
        Self {
            anchor: pos,
            cursor: pos,
            area,
            zone,
        }
    }

    pub fn update(&mut self, row: u16, col: u16, scroll_offset: u32) {
        self.cursor = DocPos::new(
            screen_to_doc(row, self.area, scroll_offset),
            clamp_col(col, self.area),
        );
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    pub fn normalized(&self) -> (DocPos, DocPos) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    pub fn to_screen(self, scroll_offset: u32) -> Option<ScreenSelection> {
        let (start, end) = self.normalized();
        if start == end {
            return None;
        }

        let view_top = scroll_offset;
        let view_bottom = scroll_offset + self.area.height as u32;

        if end.row < view_top || start.row >= view_bottom {
            return None;
        }

        let project_row = |doc_row: u32| -> u16 {
            if doc_row < view_top {
                self.area.y
            } else if doc_row >= view_bottom {
                self.area.y + self.area.height.saturating_sub(1)
            } else {
                self.area.y + (doc_row - view_top) as u16
            }
        };

        let start_row = project_row(start.row);
        let start_col = if start.row < view_top {
            self.area.x
        } else {
            start.col
        };
        let end_row = project_row(end.row);
        let end_col = if end.row >= view_bottom {
            self.area.x + self.area.width.saturating_sub(1)
        } else {
            end.col
        };

        Some(ScreenSelection {
            start_row,
            start_col,
            end_row,
            end_col,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenSelection {
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
}

pub struct EdgeScroll {
    pub dir: i32,
    pub last_tick: Instant,
}

/// Two-phase lifecycle: `Dragging` while the mouse is held, then
/// `PendingCopy` on release so the next `view()` can scrape the buffer.
pub enum SelectionState {
    Dragging {
        sel: Selection,
        edge_scroll: Option<EdgeScroll>,
        last_drag_col: u16,
    },
    PendingCopy {
        sel: Selection,
    },
}

impl SelectionState {
    pub fn sel(&self) -> &Selection {
        match self {
            Self::Dragging { sel, .. } | Self::PendingCopy { sel } => sel,
        }
    }

    pub fn is_pending_copy(&self) -> bool {
        matches!(self, Self::PendingCopy { .. })
    }

    pub fn is_edge_scrolling(&self) -> bool {
        matches!(
            self,
            Self::Dragging {
                edge_scroll: Some(_),
                ..
            }
        )
    }
}

#[derive(Clone, Debug, Default)]
pub enum LineBreaks {
    #[default]
    EveryRow,
    Bitmap(Vec<u64>),
}

impl LineBreaks {
    pub fn from_heights(heights: impl Iterator<Item = u16>) -> Self {
        let mut bits = Vec::new();
        let mut row: u16 = 0;
        for h in heights {
            if h == 0 {
                continue;
            }
            let idx = (row / 64) as usize;
            if idx >= bits.len() {
                bits.resize(idx + 1, 0u64);
            }
            bits[idx] |= 1 << (row % 64);
            row = row.saturating_add(h);
        }
        Self::Bitmap(bits)
    }

    pub fn from_lines(lines: &[Line<'_>], width: u16) -> Self {
        if width == 0 {
            return Self::EveryRow;
        }
        let mut heights = Vec::with_capacity(lines.len());
        for line in lines {
            if is_code_wrap_continuation(line)
                && let Some(last) = heights.last_mut()
            {
                *last += 1;
                continue;
            }
            let h = Paragraph::new(vec![line.clone()])
                .wrap(Wrap { trim: false })
                .line_count(width) as u16;
            heights.push(h);
        }
        Self::from_heights(heights.into_iter())
    }

    pub fn is_line_start(&self, row: u16) -> bool {
        match self {
            Self::EveryRow => true,
            Self::Bitmap(bits) => bits
                .get((row / 64) as usize)
                .is_some_and(|word| word & (1 << (row % 64)) != 0),
        }
    }
}

fn is_code_wrap_continuation(line: &Line<'_>) -> bool {
    line.spans
        .first()
        .is_some_and(|s| s.content.as_ref() == CODE_BAR_WRAP)
}

/// When `raw_text` is set and the region is fully selected we use the
/// source text verbatim instead of scraping cells.
#[derive(Default)]
pub struct ContentRegion<'a> {
    pub area: Rect,
    pub raw_text: &'a str,
    pub line_breaks: LineBreaks,
}

pub fn inset_border(area: Rect) -> Rect {
    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

pub(crate) fn range_covers(
    sel_start: DocPos,
    sel_end: DocPos,
    rect_top: u32,
    rect_bottom_incl: u32,
    rect_left: u16,
    rect_right_incl: u16,
) -> bool {
    rect_top >= sel_start.row
        && rect_bottom_incl <= sel_end.row
        && (rect_top != sel_start.row || sel_start.col <= rect_left)
        && (rect_bottom_incl != sel_end.row || sel_end.col >= rect_right_incl)
}

impl ScreenSelection {
    pub fn covers_rect(&self, area: Rect) -> bool {
        if area.width == 0 || area.height == 0 {
            return false;
        }
        range_covers(
            DocPos::new(self.start_row as u32, self.start_col),
            DocPos::new(self.end_row as u32, self.end_col),
            area.y as u32,
            area.bottom().saturating_sub(1) as u32,
            area.x,
            area.x + area.width.saturating_sub(1),
        )
    }
}

#[inline]
pub(crate) fn col_range(ss: &ScreenSelection, left: u16, right: u16, row: u16) -> (u16, u16) {
    let col_start = if row == ss.start_row {
        ss.start_col.max(left)
    } else {
        left
    };
    let col_end = if row == ss.end_row {
        ss.end_col.min(right)
    } else {
        right
    };
    (col_start, col_end)
}

/// Last column is the scrollbar, so we skip it.
pub fn apply_highlight(buf: &mut Buffer, area: Rect, ss: &ScreenSelection) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let row_start = ss.start_row.max(area.y);
    let row_end = ss.end_row.min(area.bottom().saturating_sub(1));
    let right = area.x + area.width.saturating_sub(1);
    for row in row_start..=row_end {
        let (col_start, col_end) = col_range(ss, area.x, right, row);
        for col in col_start..=col_end {
            if col >= buf.area().right() || row >= buf.area().bottom() {
                continue;
            }
            let cell = &mut buf[(col, row)];
            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
        }
    }
}

pub(crate) fn strip_code_bar_prefix(
    cell: &ratatui::buffer::Cell,
    out: &mut String,
    line_start: usize,
) -> usize {
    if cell.style().fg != theme::current().code_bar.fg || cell.symbol() != "│" {
        return 0;
    }
    let line = &out[line_start..];
    let prefix_len = if line.starts_with(CODE_BAR) {
        CODE_BAR.len()
    } else if line.starts_with(CODE_BAR_WRAP) {
        CODE_BAR_WRAP.len()
    } else {
        return 0;
    };
    out.drain(line_start..line_start + prefix_len);
    prefix_len
}

/// Trailing whitespace is trimmed per line. Consecutive blank lines are
/// collapsed so we don't emit a wall of empty newlines.
pub(crate) fn append_rows(
    buf: &Buffer,
    area: Rect,
    ss: &ScreenSelection,
    from: u16,
    to: u16,
    out: &mut String,
    breaks: &LineBreaks,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let right = area.x + area.width.saturating_sub(1);
    let row_start = from.max(area.y);
    let row_end = to.min(area.bottom());
    let mut pending_newlines = 0u16;
    let anchor = out.len();
    let mut prev_row_full = false;
    for row in row_start..row_end {
        let is_new_line = breaks.is_line_start(row - area.y);
        if is_new_line {
            prev_row_full = false;
        }

        let (col_start, col_end) = col_range(ss, area.x, right, row);
        let line_start = out.len();
        for col in col_start..=col_end {
            out.push_str(buf[(col, row)].symbol());
        }

        let stripped = if col_start == area.x {
            strip_code_bar_prefix(&buf[(col_start, row)], out, line_start)
        } else {
            0
        };

        let trimmed_len = out[line_start..].trim_end().len() + line_start;
        out.truncate(trimmed_len);
        let has_content = out.len() > line_start;
        let is_first_row = line_start == anchor;

        if is_new_line {
            if !has_content && !is_first_row {
                pending_newlines += 1;
            } else if !is_first_row {
                for _ in 0..pending_newlines {
                    out.insert(line_start, '\n');
                }
                pending_newlines = 0;
                out.insert(line_start, '\n');
            }
        } else if has_content && !is_first_row && !prev_row_full && stripped == 0 {
            out.insert(line_start, ' ');
        }

        prev_row_full = trimmed_len - line_start >= area.width as usize;
    }
}

/// Searched in reverse so overlays win over what is behind them.
pub fn extract_selected_text(
    buf: &Buffer,
    ss: &ScreenSelection,
    regions: &[ContentRegion<'_>],
) -> String {
    let mut out = String::new();
    let mut row = ss.start_row;

    while row <= ss.end_row {
        let region = regions
            .iter()
            .rev()
            .find(|r| r.area.y <= row && row < r.area.bottom());

        let Some(region) = region else {
            row += 1;
            continue;
        };

        let region_end = region.area.bottom();
        let fully_selected = ss.covers_rect(region.area);

        if !out.is_empty() {
            out.push('\n');
        }
        if fully_selected && !region.raw_text.is_empty() {
            out.push_str(region.raw_text);
        } else {
            let chunk_end = region_end.min(ss.end_row + 1);
            append_rows(
                buf,
                region.area,
                ss,
                row,
                chunk_end,
                &mut out,
                &region.line_breaks,
            );
        }
        row = region_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    use test_case::test_case;

    fn doc(row: u32, col: u16) -> DocPos {
        DocPos::new(row, col)
    }

    #[test_case(doc(0, 0), doc(5, 10), (doc(0, 0), doc(5, 10)) ; "forward_selection")]
    #[test_case(doc(5, 10), doc(0, 0), (doc(0, 0), doc(5, 10)) ; "backward_selection")]
    #[test_case(doc(3, 5), doc(3, 5), (doc(3, 5), doc(3, 5))   ; "same_point")]
    fn normalized(a: DocPos, c: DocPos, expected: (DocPos, DocPos)) {
        let sel = Selection {
            anchor: a,
            cursor: c,
            area: Rect::default(),
            zone: SelectionZone::Messages,
        };
        assert_eq!(sel.normalized(), expected);
    }

    fn test_buffer() -> (Buffer, Rect) {
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "Hello     ", Style::default());
        buf.set_string(0, 1, "World     ", Style::default());
        buf.set_string(0, 2, "Test      ", Style::default());
        (buf, area)
    }

    fn ss(sr: u16, sc: u16, er: u16, ec: u16) -> ScreenSelection {
        ScreenSelection {
            start_row: sr,
            start_col: sc,
            end_row: er,
            end_col: ec,
        }
    }

    #[test_case(ss(0, 0, 0, 4), "# Hello\n\nWorld\nTest", "Hello"         ; "single_row_partial")]
    #[test_case(ss(0, 0, 2, 9), "# Hello\n\nWorld\nTest", "# Hello\n\nWorld\nTest" ; "fully_selected_uses_raw")]
    #[test_case(ss(0, 0, 1, 4), "raw",                    "Hello\nWorld"   ; "multi_row_partial")]
    #[test_case(ss(0, 2, 2, 9), "should not use this",    "llo\nWorld\nTest" ; "partial_column_skips_raw")]
    fn extract_basic(sel: ScreenSelection, raw: &str, expected: &str) {
        let (buf, area) = test_buffer();
        let region = ContentRegion {
            area,
            raw_text: raw,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &sel, &[region]);
        assert_eq!(text, expected);
    }

    #[test]
    fn extract_skips_uncovered_rows() {
        let area = Rect::new(0, 0, 10, 5);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "Line 0    ", Style::default());
        buf.set_string(0, 1, "──────────", Style::default());
        buf.set_string(0, 2, "Line 2    ", Style::default());
        buf.set_string(0, 3, "──────────", Style::default());
        buf.set_string(0, 4, "Line 4    ", Style::default());

        let regions = vec![
            ContentRegion {
                area: Rect::new(0, 0, 10, 1),
                raw_text: "Line 0",
                ..Default::default()
            },
            ContentRegion {
                area: Rect::new(0, 2, 10, 1),
                raw_text: "Line 2",
                ..Default::default()
            },
            ContentRegion {
                area: Rect::new(0, 4, 10, 1),
                raw_text: "Line 4",
                ..Default::default()
            },
        ];
        let text = extract_selected_text(&buf, &ss(0, 0, 4, 7), &regions);
        assert_eq!(text, "Line 0\nLine 2\nLine 4");
    }

    #[test]
    fn extract_overlay_wins_over_base() {
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "base 0    ", Style::default());
        buf.set_string(0, 1, "overlay 1 ", Style::default());
        buf.set_string(0, 2, "base 2    ", Style::default());

        let base = ContentRegion {
            area: Rect::new(0, 0, 10, 3),
            raw_text: "base raw text",
            ..Default::default()
        };
        let overlay = ContentRegion {
            area: Rect::new(0, 0, 10, 3),
            raw_text: "overlay raw text",
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, 2, 9), &[base, overlay]);
        assert_eq!(text, "overlay raw text");
    }

    #[test]
    fn extract_multi_region_mixed_full_and_partial() {
        let area = Rect::new(0, 0, 20, 4);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "msg0 rendered       ", Style::default());
        buf.set_string(0, 1, "msg0 line2          ", Style::default());
        buf.set_string(0, 2, "msg1 rendered       ", Style::default());
        buf.set_string(0, 3, "msg1 line2          ", Style::default());

        let regions = vec![
            ContentRegion {
                area: Rect::new(0, 0, 20, 2),
                raw_text: "# msg0 raw",
                ..Default::default()
            },
            ContentRegion {
                area: Rect::new(0, 2, 20, 2),
                raw_text: "# msg1 raw",
                ..Default::default()
            },
        ];
        let text = extract_selected_text(&buf, &ss(1, 0, 2, 18), &regions);
        assert_eq!(text, "msg0 line2\nmsg1 rendered");
    }

    #[test]
    fn extract_no_matching_region_returns_empty() {
        let (buf, _) = test_buffer();
        assert_eq!(extract_selected_text(&buf, &ss(0, 0, 2, 7), &[]), "");

        let region = ContentRegion {
            area: Rect::new(0, 5, 10, 1),
            raw_text: "far away",
            ..Default::default()
        };
        assert_eq!(extract_selected_text(&buf, &ss(0, 0, 2, 7), &[region]), "");
    }

    #[test]
    fn fully_selected_empty_raw_text_extracts_from_buffer() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "Status    ", Style::default());
        let region = ContentRegion {
            area,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, 0, 9), &[region]);
        assert_eq!(text, "Status");
    }

    #[test_case(Rect::new(0,3,80,20), 15, 5, 10, 22 ; "normal_offset")]
    #[test_case(Rect::new(0,2,80,10), 15, 5,  0,  9 ; "clamped_below_area")]
    #[test_case(Rect::new(0,5,80,10),  2, 5,  7,  7 ; "clamped_above_area")]
    fn selection_start_doc_row(
        area: Rect,
        screen_row: u16,
        screen_col: u16,
        scroll: u32,
        expected_row: u32,
    ) {
        let sel = Selection::start(
            screen_row,
            screen_col,
            area,
            SelectionZone::Messages,
            scroll,
        );
        assert_eq!(sel.normalized().0.row, expected_row);
    }

    #[test_case(doc(5,2),  doc(8,10),  Rect::new(0,0,80,20),  0, Some(ss(5,2,8,10))    ; "fully_visible")]
    #[test_case(doc(2,5),  doc(12,8),  Rect::new(0,0,80,20),  5, Some(ss(0,0,7,8))     ; "partially_off_top")]
    #[test_case(doc(0,0),  doc(3,5),   Rect::new(0,0,80,20), 10, None                   ; "entirely_off_screen")]
    #[test_case(doc(5,5),  doc(5,5),   Rect::new(0,0,80,20),  0, None                   ; "empty_selection")]
    #[test_case(doc(5,3),  doc(12,8),  Rect::new(0,0,80,10),  0, Some(ss(5,3,9,79))     ; "cursor_below_area")]
    #[test_case(doc(12,5), doc(3,2),   Rect::new(0,0,80,10),  0, Some(ss(3,2,9,79))     ; "backward_from_below")]
    #[test_case(doc(58,5), doc(55,3),  Rect::new(0,2,80,20), 50, Some(ss(7,3,10,5))     ; "edge_scroll_reversal")]
    #[test_case(doc(9,0),  doc(9,5),   Rect::new(0,0,80,10),  0, Some(ss(9,0,9,5))      ; "at_viewport_bottom")]
    #[test_case(doc(9,0),  doc(9,5),   Rect::new(0,0,80,10), 10, None                   ; "scrolled_past_bottom")]
    #[test_case(doc(3,10), doc(12,50), Rect::new(0,0,80,10),  5, Some(ss(0,0,7,50))     ; "start_above_viewport")]
    #[test_case(doc(2,5),  doc(25,70), Rect::new(0,0,80,10),  5, Some(ss(0,0,9,79))     ; "both_ends_outside")]
    #[test_case(doc(0,8),  doc(5,20),  Rect::new(5,3,40,10),  0, Some(ss(3,8,8,20))     ; "nonzero_area_offset")]
    fn to_screen_cases(
        anchor: DocPos,
        cursor: DocPos,
        area: Rect,
        scroll: u32,
        expected: Option<ScreenSelection>,
    ) {
        let sel = Selection {
            anchor,
            cursor,
            area,
            zone: SelectionZone::Messages,
        };
        assert_eq!(sel.to_screen(scroll), expected);
    }

    #[test_case(Rect::new(0,2,80,20), 10, 5, 25, 5, 0, 19, 5 ; "clamp_row_to_bottom")]
    #[test_case(Rect::new(5,0,40,20), 10,10, 10,50, 0, 10,44 ; "clamp_col_to_right")]
    #[test_case(Rect::new(5,0,40,20), 10,10, 10, 2, 0, 10, 5 ; "clamp_col_to_left")]
    #[allow(clippy::too_many_arguments)]
    fn update_clamps(
        area: Rect,
        start_row: u16,
        start_col: u16,
        upd_row: u16,
        upd_col: u16,
        scroll: u32,
        expected_row: u32,
        expected_col: u16,
    ) {
        let mut sel = Selection::start(start_row, start_col, area, SelectionZone::Messages, 0);
        sel.update(upd_row, upd_col, scroll);
        assert_eq!(sel.cursor.row, expected_row);
        assert_eq!(sel.cursor.col, expected_col);
    }

    fn code_bar_buffer() -> (Buffer, Rect) {
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let code_bar_style = theme::current().code_bar;
        buf.set_string(0, 0, "│", code_bar_style);
        buf.set_string(2, 0, "fn main() {}        ", Style::default());
        buf.set_string(0, 1, "│", code_bar_style);
        buf.set_string(2, 1, "let x = 1;          ", Style::default());
        (buf, area)
    }

    #[test]
    fn strips_code_bar_prefix_from_partial_selection() {
        let (buf, area) = code_bar_buffer();
        let region = ContentRegion {
            area,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, 1, 18), &[region]);
        assert_eq!(text, "fn main() {}\nlet x = 1;");
    }

    #[test]
    fn does_not_strip_table_border_prefix() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let table_style = theme::current().table_border;
        buf.set_string(0, 0, "│", table_style);
        buf.set_string(2, 0, "cell content        ", Style::default());
        let region = ContentRegion {
            area,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, 0, 18), &[region]);
        assert_eq!(text, "│ cell content");
    }

    #[test]
    fn no_strip_when_selection_starts_mid_line() {
        let (buf, area) = code_bar_buffer();
        let region = ContentRegion {
            area,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 5, 0, 13), &[region]);
        assert_eq!(text, "main() {}");
    }

    #[test]
    fn strips_code_bar_wrap_prefix() {
        let area = Rect::new(0, 0, 12, 1);
        let mut buf = Buffer::empty(area);
        let code_bar_style = theme::current().code_bar;
        buf.set_string(0, 0, "│", code_bar_style);
        buf.set_string(1, 0, "continued  ", Style::default());
        let region = ContentRegion {
            area,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, 0, 10), &[region]);
        assert_eq!(text, "continued");
    }

    #[test_case(&[1, 1, 1], &[0, 1, 2]    ; "no_wrapping")]
    #[test_case(&[1, 3, 1], &[0, 1, 4]    ; "middle_line_wraps")]
    #[test_case(&[3, 3],    &[0, 3]        ; "all_lines_wrap")]
    fn line_breaks_from_heights(heights: &[u16], expected_starts: &[u16]) {
        let lb = LineBreaks::from_heights(heights.iter().copied());
        for row in 0..heights.iter().sum::<u16>().max(1) {
            let should_be_start = expected_starts.contains(&row);
            assert_eq!(
                lb.is_line_start(row),
                should_be_start,
                "row {row}: expected is_line_start={should_be_start}"
            );
        }
    }

    #[test]
    fn line_breaks_beyond_64_rows() {
        let lb = LineBreaks::from_heights([65, 1].iter().copied());
        assert!(lb.is_line_start(0));
        assert!(!lb.is_line_start(64));
        assert!(lb.is_line_start(65));
    }

    #[test]
    fn zone_at_overlay_wins_over_messages() {
        let msg_area = Rect::new(0, 0, 80, 20);
        let overlay_area = Rect::new(10, 5, 60, 10);
        let mut zones: ZoneRegistry = [None; SelectionZone::COUNT];
        zones[SelectionZone::Messages.idx()] = Some(SelectableZone {
            area: msg_area,
            highlight_area: msg_area,
            zone: SelectionZone::Messages,
        });
        zones[SelectionZone::Overlay.idx()] = Some(SelectableZone {
            area: overlay_area,
            highlight_area: overlay_area,
            zone: SelectionZone::Overlay,
        });

        assert_eq!(zone_at(&zones, 7, 20).unwrap().zone, SelectionZone::Overlay);
        assert_eq!(
            zone_at(&zones, 2, 20).unwrap().zone,
            SelectionZone::Messages
        );
        assert_eq!(zone_at(&zones, 7, 5).unwrap().zone, SelectionZone::Messages);
    }

    #[test_case(doc(0, 0), doc(2, 9), 0, 2, 0, 9, true  ; "exact_match")]
    #[test_case(doc(0, 0), doc(5, 9), 1, 3, 0, 9, true  ; "selection_exceeds_rect")]
    #[test_case(doc(0, 0), doc(2, 8), 0, 2, 0, 9, false ; "end_col_one_short")]
    #[test_case(doc(0, 1), doc(2, 9), 0, 2, 0, 9, false ; "start_col_one_past")]
    #[test_case(doc(1, 0), doc(2, 9), 0, 2, 0, 9, false ; "start_row_one_past")]
    #[test_case(doc(0, 0), doc(1, 9), 0, 2, 0, 9, false ; "end_row_one_short")]
    #[test_case(doc(5, 3), doc(5, 3), 5, 5, 3, 3, true  ; "single_cell")]
    fn range_covers_cases(
        sel_start: DocPos,
        sel_end: DocPos,
        rt: u32,
        rb: u32,
        rl: u16,
        rr: u16,
        expected: bool,
    ) {
        assert_eq!(range_covers(sel_start, sel_end, rt, rb, rl, rr), expected);
    }

    #[test]
    fn soft_wrap_continuation_inserts_space() {
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "hello               ", Style::default());
        buf.set_string(0, 1, "world               ", Style::default());
        let breaks = LineBreaks::from_heights([2].iter().copied());
        let region = ContentRegion {
            area,
            line_breaks: breaks,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, 1, 19), &[region]);
        assert_eq!(text, "hello world");
    }

    #[test]
    fn code_wrap_continuation_no_space() {
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let code_style = theme::current().code_bar;
        buf.set_string(0, 0, "│", code_style);
        buf.set_string(2, 0, "long_variable_na", Style::default());
        buf.set_string(0, 1, "│", code_style);
        buf.set_string(1, 1, "me_here         ", Style::default());
        let breaks = LineBreaks::from_heights([2].iter().copied());
        let region = ContentRegion {
            area,
            line_breaks: breaks,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, 1, 19), &[region]);
        assert_eq!(text, "long_variable_name_here");
    }

    #[test]
    fn selection_empty_until_updated() {
        let area = Rect::new(0, 0, 80, 20);
        let mut sel = Selection::start(5, 10, area, SelectionZone::Messages, 0);
        assert!(sel.is_empty());
        sel.update(6, 10, 0);
        assert!(!sel.is_empty());
    }

    #[test]
    fn selection_start_col_clamped() {
        let area = Rect::new(10, 5, 40, 20);
        let right = Selection::start(8, 200, area, SelectionZone::Messages, 0);
        assert_eq!(right.normalized().0.col, 49, "clamped to area right edge");
        let left = Selection::start(8, 0, area, SelectionZone::Messages, 0);
        assert_eq!(left.normalized().0.col, 10, "clamped to area left edge");
    }

    #[test]
    fn covers_rect_empty_area() {
        let sel = ss(0, 0, 10, 10);
        assert!(!sel.covers_rect(Rect::new(0, 0, 0, 5)));
        assert!(!sel.covers_rect(Rect::new(0, 0, 5, 0)));
        assert!(!sel.covers_rect(Rect::ZERO));
    }

    #[test]
    fn line_breaks_bitmap_zero_height_ignored() {
        let lb = LineBreaks::from_heights([1, 0, 0, 1].iter().copied());
        assert!(lb.is_line_start(0));
        assert!(lb.is_line_start(1));
        assert!(!lb.is_line_start(2));
    }

    #[test]
    fn line_breaks_empty_iterator() {
        let lb = LineBreaks::from_heights(std::iter::empty());
        assert!(matches!(lb, LineBreaks::Bitmap(ref v) if v.is_empty()));
    }

    #[test]
    fn line_breaks_bitmap_query_beyond_stored_bits() {
        let lb = LineBreaks::from_heights([1].iter().copied());
        assert!(!lb.is_line_start(100));
    }

    #[test]
    fn extract_trailing_blank_lines_collapsed() {
        let area = Rect::new(0, 0, 10, 4);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "Line A    ", Style::default());
        buf.set_string(0, 1, "          ", Style::default());
        buf.set_string(0, 2, "          ", Style::default());
        buf.set_string(0, 3, "Line D    ", Style::default());
        let region = ContentRegion {
            area,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, 3, 9), &[region]);
        assert_eq!(
            text, "Line A\n\n\nLine D",
            "two blank rows produce two pending newlines"
        );
    }

    #[test]
    fn extract_single_cell_selection() {
        let (buf, area) = test_buffer();
        let region = ContentRegion {
            area,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, 0, 0), &[region]);
        assert_eq!(text, "H");
    }

    #[test]
    fn col_range_single_row_selection() {
        let sel = ss(5, 10, 5, 20);
        let (start, end) = col_range(&sel, 0, 79, 5);
        assert_eq!(start, 10);
        assert_eq!(end, 20);
    }

    #[test]
    fn col_range_mid_row_full_width() {
        let sel = ss(3, 10, 7, 50);
        let (start, end) = col_range(&sel, 0, 79, 5);
        assert_eq!(start, 0, "mid row gets full left");
        assert_eq!(end, 79, "mid row gets full right");
    }

    #[test]
    fn zone_at_returns_none_when_outside() {
        let mut zones: ZoneRegistry = [None; SelectionZone::COUNT];
        assert!(zone_at(&zones, 10, 10).is_none(), "no zones registered");
        zones[SelectionZone::Messages.idx()] = Some(SelectableZone {
            area: Rect::new(0, 0, 80, 20),
            highlight_area: Rect::new(0, 0, 80, 20),
            zone: SelectionZone::Messages,
        });
        assert!(zone_at(&zones, 25, 10).is_none(), "outside all zones");
    }
}
