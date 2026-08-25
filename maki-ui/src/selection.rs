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
//! `highlight_area()` / `msg_area()` are 1 column narrower than the full
//! area and all column math uses `width - 1`.
//!
//! When text is word-wrapped, ratatui eats the space at the break point,
//! so the copied text would lose that space unless we put it back. But we
//! should only add a space for word-boundary wraps, not mid-word ones
//! (where a long token just overflows the column). `LineBreaks` tracks
//! both line starts and word wraps: `from_lines()` replays ratatui's wrap
//! machine through [`crate::wrap`] and marks the rows that start after a
//! swallowed space, then `append_rows` calls `needs_space()`.

use std::cmp::Ordering;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;

use unicode_width::UnicodeWidthStr;

use crate::repaint::Cadence;
use crate::theme;
use crate::wrap::{self, Break};
use maki_markdown::render::{CODE_BAR, CODE_BAR_WRAP};

/// Position in the full logical document, not just the visible window. For
/// the messages zone `seg` is the transcript segment and `row` counts inside
/// it, which is what survives a resize; flat zones leave `seg` at 0. `col` is
/// a screen x coordinate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocPos {
    pub seg: usize,
    pub row: u16,
    pub col: u16,
}

impl DocPos {
    /// A position in a zone whose document is one flat list of rows.
    pub fn flat(row: u16, col: u16) -> Self {
        Self { seg: 0, row, col }
    }
}

impl PartialOrd for DocPos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DocPos {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.seg, self.row, self.col).cmp(&(other.seg, other.row, other.col))
    }
}

/// Where a [`DocPos`] falls relative to the viewport. `At` counts rows down
/// from the top of the zone's area, so it is not a screen row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowPos {
    Above,
    At(u16),
    Below,
}

impl RowPos {
    /// Projection for a zone whose document is one flat list of rows.
    pub fn flat(pos: DocPos, area: Rect, scroll: u16) -> Self {
        if pos.row < scroll {
            Self::Above
        } else if pos.row - scroll >= area.height {
            Self::Below
        } else {
            Self::At(pos.row - scroll)
        }
    }
}

/// Selection is locked to one zone for its entire lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionZone {
    Messages,
    Input,
    Overlay,
}

#[derive(Clone, Copy, Debug)]
pub struct SelectableZone {
    pub area: Rect,
    pub zone: SelectionZone,
}

const ZONE_CAP: usize = 12;

const EMPTY_ZONE: SelectableZone = SelectableZone {
    area: Rect::new(0, 0, 0, 0),
    zone: SelectionZone::Messages,
};

pub struct ZoneRegistry {
    entries: [SelectableZone; ZONE_CAP],
    len: u8,
}

impl ZoneRegistry {
    pub fn new() -> Self {
        Self {
            entries: [EMPTY_ZONE; ZONE_CAP],
            len: 0,
        }
    }

    pub fn push(&mut self, zone: SelectableZone) {
        let i = self.len as usize;
        if i >= ZONE_CAP {
            return;
        }
        self.entries[i] = zone;
        self.len += 1;
    }

    pub fn push_overlay(&mut self, area: Rect) {
        self.push(SelectableZone {
            area,
            zone: SelectionZone::Overlay,
        });
    }

    pub fn zone_at(&self, row: u16, col: u16) -> Option<SelectableZone> {
        let pos = ratatui::layout::Position::new(col, row);
        self.entries[..self.len as usize]
            .iter()
            .rev()
            .find(|z| z.area.contains(pos))
            .copied()
    }

    pub fn find(&self, zone: SelectionZone) -> Option<&SelectableZone> {
        self.entries[..self.len as usize]
            .iter()
            .find(|z| z.zone == zone)
    }

    pub fn find_area(&self, area: Rect) -> Option<&SelectableZone> {
        self.entries[..self.len as usize]
            .iter()
            .find(|z| z.area == area)
    }
}

/// Anchor + cursor in doc space. `area` and `zone` are captured at mouse-down
/// so layout changes mid-drag don't break the selection.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    anchor: DocPos,
    cursor: DocPos,
    pub area: Rect,
    pub zone: SelectionZone,
}

/// Clamps a screen row into `area` and returns how far down it sits.
pub fn row_in_area(screen_row: u16, area: Rect) -> u16 {
    screen_row.clamp(area.y, area.y + area.height.saturating_sub(1)) - area.y
}

pub fn clamp_col(col: u16, area: Rect) -> u16 {
    col.clamp(area.x, area.x + area.width.saturating_sub(1))
}

impl Selection {
    pub fn start(pos: DocPos, area: Rect, zone: SelectionZone) -> Self {
        Self {
            anchor: pos,
            cursor: pos,
            area,
            zone,
        }
    }

    pub fn update(&mut self, pos: DocPos) {
        self.cursor = pos;
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    pub fn highlight_area(&self) -> Rect {
        match self.zone {
            SelectionZone::Messages => Rect::new(
                self.area.x,
                self.area.y,
                self.area.width.saturating_sub(1),
                self.area.height,
            ),
            _ => self.area,
        }
    }

    pub fn normalized(&self) -> (DocPos, DocPos) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Clips the selection to the viewport `project` describes. There is
    /// nothing to draw when all of it sits past one of the edges.
    pub fn to_screen(self, project: impl Fn(DocPos) -> RowPos) -> Option<ScreenSelection> {
        let (start, end) = self.normalized();
        if start == end {
            return None;
        }

        let (start_row, start_col) = match project(start) {
            RowPos::Below => return None,
            RowPos::Above => (self.area.y, self.area.x),
            RowPos::At(row) => (self.area.y + row, start.col),
        };
        let (end_row, end_col) = match project(end) {
            RowPos::Above => return None,
            RowPos::Below => (
                self.area.y + self.area.height.saturating_sub(1),
                self.area.x + self.area.width.saturating_sub(1),
            ),
            RowPos::At(row) => (self.area.y + row, end.col),
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

    /// Edge scroll advances on a timer while a drag rests against the edge of
    /// the viewport, with no further mouse events to wake the loop.
    pub fn cadence(&self) -> Cadence {
        Cadence::when(self.is_edge_scrolling(), Cadence::SMOOTH)
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
    Bitmap {
        line_starts: Vec<u64>,
        word_wraps: Vec<u64>,
    },
}

fn set_bit(bits: &mut Vec<u64>, row: u16) {
    let idx = (row / 64) as usize;
    if idx >= bits.len() {
        bits.resize(idx + 1, 0u64);
    }
    bits[idx] |= 1 << (row % 64);
}

fn get_bit(bits: &[u64], row: u16) -> bool {
    bits.get((row / 64) as usize)
        .is_some_and(|word| word & (1 << (row % 64)) != 0)
}

impl LineBreaks {
    pub fn from_heights(heights: impl Iterator<Item = u16>) -> Self {
        let mut line_starts = Vec::new();
        let mut row: u16 = 0;
        for h in heights {
            if h == 0 {
                continue;
            }
            set_bit(&mut line_starts, row);
            row = row.saturating_add(h);
        }
        Self::Bitmap {
            line_starts,
            word_wraps: Vec::new(),
        }
    }

    pub fn from_lines(lines: &[Line<'_>], width: u16) -> Self {
        if width == 0 {
            return Self::EveryRow;
        }
        let mut line_starts = Vec::new();
        let mut word_wraps = Vec::new();
        let mut row: u16 = 0;
        for line in lines {
            if is_code_wrap_continuation(line) {
                row += 1;
                continue;
            }
            set_bit(&mut line_starts, row);
            wrap::breaks(line, width, |kind| {
                row += 1;
                if kind == Break::Word {
                    set_bit(&mut word_wraps, row);
                }
            });
            row += 1;
        }
        Self::Bitmap {
            line_starts,
            word_wraps,
        }
    }

    pub fn is_line_start(&self, row: u16) -> bool {
        match self {
            Self::EveryRow => true,
            Self::Bitmap { line_starts, .. } => get_bit(line_starts, row),
        }
    }

    pub fn needs_space(&self, row: u16) -> bool {
        match self {
            Self::EveryRow => false,
            Self::Bitmap { word_wraps, .. } => get_bit(word_wraps, row),
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

impl ScreenSelection {
    /// Whether every cell of `area` is selected, so the caller can take its
    /// text wholesale instead of walking rows.
    pub fn covers_rect(&self, area: Rect) -> bool {
        if area.width == 0 || area.height == 0 {
            return false;
        }
        let bottom = area.bottom() - 1;
        let right = area.right() - 1;
        area.y >= self.start_row
            && bottom <= self.end_row
            && (area.y != self.start_row || self.start_col <= area.x)
            && (bottom != self.end_row || self.end_col >= right)
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
    if cell.style().fg != theme::current().code_gutter.fg || cell.symbol() != "│" {
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
    for row in row_start..row_end {
        let rel_row = row - area.y;
        let is_new_line = breaks.is_line_start(rel_row);

        let (col_start, col_end) = col_range(ss, area.x, right, row);
        let line_start = out.len();
        // Wide chars (CJK, emoji) leave continuation cells with " "; skip them.
        let mut skip_next: usize = if col_start > area.x {
            UnicodeWidthStr::width(buf[(col_start - 1, row)].symbol()).saturating_sub(1)
        } else {
            0
        };
        for col in col_start..=col_end {
            if skip_next > 0 {
                skip_next -= 1;
                continue;
            }
            let sym = buf[(col, row)].symbol();
            let w = UnicodeWidthStr::width(sym);
            out.push_str(sym);
            skip_next = w.saturating_sub(1);
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
        } else if has_content && !is_first_row && stripped == 0 && breaks.needs_space(rel_row) {
            out.insert(line_start, ' ');
        }
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

    fn doc(row: u16, col: u16) -> DocPos {
        DocPos::flat(row, col)
    }

    fn seg_doc(seg: usize, row: u16, col: u16) -> DocPos {
        DocPos { seg, row, col }
    }

    #[test_case(doc(0, 0), doc(5, 10), (doc(0, 0), doc(5, 10)) ; "forward_selection")]
    #[test_case(doc(5, 10), doc(0, 0), (doc(0, 0), doc(5, 10)) ; "backward_selection")]
    #[test_case(doc(3, 5), doc(3, 5), (doc(3, 5), doc(3, 5))   ; "same_point")]
    #[test_case(seg_doc(1, 0, 0), doc(99, 99), (doc(99, 99), seg_doc(1, 0, 0)) ; "a_later_segment_outranks_any_row")]
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

    #[test_case(Rect::new(0,3,80,20), 15, 12 ; "inside_area")]
    #[test_case(Rect::new(0,2,80,10), 15,  9 ; "clamped_below_area")]
    #[test_case(Rect::new(0,5,80,10),  2,  0 ; "clamped_above_area")]
    fn row_in_area_clamps(area: Rect, screen_row: u16, expected: u16) {
        assert_eq!(row_in_area(screen_row, area), expected);
    }

    #[test_case(Rect::new(10,5,40,20), 200, 49 ; "past_right_edge")]
    #[test_case(Rect::new(10,5,40,20),   0, 10 ; "past_left_edge")]
    #[test_case(Rect::new(10,5,40,20),  20, 20 ; "inside_area")]
    fn clamp_col_clamps(area: Rect, col: u16, expected: u16) {
        assert_eq!(clamp_col(col, area), expected);
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
        scroll: u16,
        expected: Option<ScreenSelection>,
    ) {
        let sel = Selection {
            anchor,
            cursor,
            area,
            zone: SelectionZone::Messages,
        };
        assert_eq!(
            sel.to_screen(|pos| RowPos::flat(pos, area, scroll)),
            expected
        );
    }

    fn code_bar_buffer() -> (Buffer, Rect) {
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let code_bar_style = theme::current().code_gutter;
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
        let code_bar_style = theme::current().code_gutter;
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
        let mut zones = ZoneRegistry::new();
        zones.push(SelectableZone {
            area: msg_area,
            zone: SelectionZone::Messages,
        });
        zones.push(SelectableZone {
            area: overlay_area,
            zone: SelectionZone::Overlay,
        });

        assert_eq!(zones.zone_at(7, 20).unwrap().zone, SelectionZone::Overlay);
        assert_eq!(zones.zone_at(2, 20).unwrap().zone, SelectionZone::Messages);
        assert_eq!(zones.zone_at(7, 5).unwrap().zone, SelectionZone::Messages);
    }

    #[test_case(ss(0, 0, 2, 9), Rect::new(0, 0, 10, 3), true  ; "exact_match")]
    #[test_case(ss(0, 0, 5, 9), Rect::new(0, 1, 10, 3), true  ; "selection_exceeds_rect")]
    #[test_case(ss(0, 0, 2, 8), Rect::new(0, 0, 10, 3), false ; "end_col_one_short")]
    #[test_case(ss(0, 1, 2, 9), Rect::new(0, 0, 10, 3), false ; "start_col_one_past")]
    #[test_case(ss(1, 0, 2, 9), Rect::new(0, 0, 10, 3), false ; "start_row_one_past")]
    #[test_case(ss(0, 0, 1, 9), Rect::new(0, 0, 10, 3), false ; "end_row_one_short")]
    #[test_case(ss(5, 3, 5, 3), Rect::new(3, 5,  1, 1), true  ; "single_cell")]
    #[test_case(ss(0, 0, 2, 9), Rect::new(0, 0,  0, 3), false ; "zero_width")]
    #[test_case(ss(0, 0, 2, 9), Rect::new(0, 0, 10, 0), false ; "zero_height")]
    fn covers_rect_cases(sel: ScreenSelection, area: Rect, expected: bool) {
        assert_eq!(sel.covers_rect(area), expected);
    }

    #[test]
    fn char_wrap_continuation_no_space_from_heights() {
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
        assert_eq!(text, "helloworld");
    }

    #[test]
    fn code_wrap_continuation_no_space() {
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let code_style = theme::current().code_gutter;
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
    fn line_breaks_bitmap_zero_height_ignored() {
        let lb = LineBreaks::from_heights([1, 0, 0, 1].iter().copied());
        assert!(lb.is_line_start(0));
        assert!(lb.is_line_start(1));
        assert!(!lb.is_line_start(2));
    }

    #[test]
    fn line_breaks_query_beyond_stored_bits() {
        let lb = LineBreaks::from_heights([1].iter().copied());
        assert!(!lb.is_line_start(100));
        assert!(!lb.needs_space(200));
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

    #[test_case(ss(5, 10, 5, 20), 5, (10, 20) ; "single_row")]
    #[test_case(ss(3, 10, 7, 50), 5, (0, 79)   ; "mid_row_full_width")]
    fn col_range_cases(sel: ScreenSelection, row: u16, expected: (u16, u16)) {
        assert_eq!(col_range(&sel, 0, 79, row), expected);
    }

    #[test]
    fn zone_at_returns_none_when_outside() {
        let mut zones = ZoneRegistry::new();
        assert!(zones.zone_at(10, 10).is_none(), "no zones registered");
        zones.push(SelectableZone {
            area: Rect::new(0, 0, 80, 20),
            zone: SelectionZone::Messages,
        });
        assert!(zones.zone_at(25, 10).is_none(), "outside all zones");
    }

    fn wrap_extract(input: &str, width: u16) -> String {
        use ratatui::widgets::{Paragraph, Widget, Wrap};
        let lines = vec![Line::from(input)];
        let p = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let height = p.line_count(width) as u16;
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);
        let breaks = LineBreaks::from_lines(&lines, width);
        let region = ContentRegion {
            area,
            line_breaks: breaks,
            ..Default::default()
        };
        extract_selected_text(&buf, &ss(0, 0, height - 1, width - 1), &[region])
    }

    #[test_case("hello world", 5, "hello world" ; "word_wrap_at_exact_boundary")]
    #[test_case("helloworld",  5, "helloworld"  ; "char_wrap_mid_word")]
    #[test_case("hello world", 6, "hello world" ; "word_wrap_space_not_at_boundary")]
    #[test_case("aa bb cc",    3, "aa bb cc"    ; "multi_word_three_rows")]
    #[test_case("abc ",        4, "abc"         ; "trailing_space_not_duplicated")]
    #[test_case("ab",          1, "ab"          ; "single_col_width")]
    #[test_case("abcdefgh ij", 5, "abcdefgh ij" ; "long_word_then_short")]
    #[test_case("abcde fghij", 5, "abcde fghij" ; "word_fills_row_exactly")]
    #[test_case("ab cd ef",    3, "ab cd ef"    ; "three_short_words")]
    #[test_case("a    b",      4, "a b"         ; "many_spaces_between_words")]
    #[test_case("你好世界",    4, "你好世界"   ; "cjk_wraps_no_extra_spaces")]
    #[test_case("你好world",  10, "你好world"  ; "cjk_mixed_ascii")]
    #[test_case("a你b好",     10, "a你b好"     ; "mixed_single_double_width")]
    fn wrap_copy(input: &str, width: u16, expected: &str) {
        assert_eq!(wrap_extract(input, width), expected);
    }

    #[test]
    fn from_lines_no_wrap_marks_each_line_as_start() {
        let lines = vec![Line::from("abc"), Line::from("def"), Line::from("ghi")];
        let lb = LineBreaks::from_lines(&lines, 80);
        assert!(lb.is_line_start(0));
        assert!(lb.is_line_start(1));
        assert!(lb.is_line_start(2));
        assert!(!lb.needs_space(0));
        assert!(!lb.needs_space(1));
        assert!(!lb.needs_space(2));
    }

    #[test]
    fn from_lines_word_wrap_marks_continuation_rows() {
        let lines = vec![Line::from("hello world")];
        let lb = LineBreaks::from_lines(&lines, 6);
        assert!(lb.is_line_start(0));
        assert!(!lb.is_line_start(1), "continuation row is not a line start");
        assert!(lb.needs_space(1), "word-boundary continuation needs space");
    }

    #[test]
    fn from_lines_code_wrap_continuation_skipped() {
        let wrap_line = Line::from(vec![ratatui::text::Span::raw(CODE_BAR_WRAP)]);
        let lines = vec![Line::from("first"), wrap_line, Line::from("second")];
        let lb = LineBreaks::from_lines(&lines, 80);
        assert!(lb.is_line_start(0));
        assert!(
            !lb.is_line_start(1),
            "code wrap continuation is not a line start"
        );
        assert!(lb.is_line_start(2));
    }

    #[test]
    fn from_lines_across_64_row_boundary() {
        let lines: Vec<Line<'_>> = (0..70).map(|_| Line::from("x")).collect();
        let lb = LineBreaks::from_lines(&lines, 80);
        assert!(lb.is_line_start(0));
        assert!(lb.is_line_start(63));
        assert!(lb.is_line_start(64));
        assert!(lb.is_line_start(69));
    }

    #[test]
    fn word_wrap_across_64_row_boundary_needs_space() {
        let long = (0..66).map(|_| "xx").collect::<Vec<_>>().join(" ");
        let lines = vec![Line::from(long.as_str())];
        let lb = LineBreaks::from_lines(&lines, 3);
        assert!(lb.is_line_start(0));
        assert!(!lb.is_line_start(1));
        assert!(lb.needs_space(1), "first continuation needs space");
        assert!(lb.needs_space(65), "continuation past row 64 needs space");
    }

    #[test_case(Rect::new(0, 0, 0, 5), Rect::new(0, 0, 1, 5), ss(0, 0, 4, 0), 0, 5 ; "zero_width")]
    #[test_case(Rect::new(0, 0, 10, 0), Rect::new(0, 0, 10, 1), ss(0, 0, 0, 9), 0, 1 ; "zero_height")]
    fn append_rows_degenerate_area_no_panic(
        area: Rect,
        buf_area: Rect,
        sel: ScreenSelection,
        from: u16,
        to: u16,
    ) {
        let buf = Buffer::empty(buf_area);
        let mut out = String::new();
        append_rows(&buf, area, &sel, from, to, &mut out, &LineBreaks::EveryRow);
        assert!(out.is_empty());
    }

    #[test]
    fn from_lines_mixed_wrapping_and_non_wrapping() {
        let lines = vec![
            Line::from("hello world"),
            Line::from("ok"),
            Line::from("foo bar baz"),
        ];
        let lb = LineBreaks::from_lines(&lines, 6);
        // row layout: "hello world" wraps to rows 0-1, "ok" is row 2,
        // "foo bar baz" wraps to rows 3-5
        assert!(lb.is_line_start(0));
        assert!(!lb.is_line_start(1));
        assert!(lb.needs_space(1));
        assert!(lb.is_line_start(2));
        assert!(lb.is_line_start(3));
        assert!(!lb.is_line_start(4));
        assert!(lb.needs_space(4));
        assert!(!lb.is_line_start(5));
        assert!(lb.needs_space(5));
    }

    #[test]
    fn wrap_extract_two_lines_first_wraps_second_doesnt() {
        use ratatui::widgets::{Paragraph, Widget, Wrap};
        let lines = vec![Line::from("hello world"), Line::from("ok")];
        let p = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
        let height = p.line_count(6) as u16;
        let area = Rect::new(0, 0, 6, height);
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);
        let breaks = LineBreaks::from_lines(&lines, 6);
        let region = ContentRegion {
            area,
            line_breaks: breaks,
            ..Default::default()
        };
        let text = extract_selected_text(&buf, &ss(0, 0, height - 1, 5), &[region]);
        assert_eq!(text, "hello world\nok");
    }

    #[test]
    fn append_rows_partial_from_wide_continuation() {
        use ratatui::widgets::{Paragraph, Widget, Wrap};
        let lines = vec![Line::from("你好")];
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, &mut buf);
        let mut out = String::new();
        // selection starts on the continuation cell of 你 (col 1)
        append_rows(
            &buf,
            area,
            &ss(0, 1, 0, 3),
            0,
            1,
            &mut out,
            &LineBreaks::EveryRow,
        );
        assert_eq!(out, "好");
    }
}
