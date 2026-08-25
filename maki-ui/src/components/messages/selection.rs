use super::segment::SegmentCache;
use crate::selection::{self, LineBreaks, ScreenSelection, Selection};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Copies the segments the selection spans. The streaming tail lives past the
/// cache, so text still arriving is not copyable.
pub(super) fn extract_selection_text(
    cache: &SegmentCache,
    width: u16,
    sel: &Selection,
    msg_area: Rect,
) -> String {
    let (start, end) = sel.normalized();

    let mut out = String::new();
    for i in start.seg..end.seg.saturating_add(1).min(cache.len()) {
        let Some(seg) = cache.get(i) else { continue };
        if seg.lines().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }

        // The rows we copy come from this wrap at the real width, so clamp
        // against it: a reflow can shrink a segment under a stored row, and
        // the layout height can predate a resize entirely.
        let drawn = seg.drawn_height(width);
        let tmp_area = Rect::new(0, 0, width, drawn);
        let mut tmp = Buffer::empty(tmp_area);
        Paragraph::new(seg.lines().to_vec())
            .wrap(Wrap { trim: false })
            .render(tmp_area, &mut tmp);

        let (rel_start, start_col) = if i == start.seg {
            (start.row.min(drawn), start.col.saturating_sub(msg_area.x))
        } else {
            (0, 0)
        };
        let (rel_end, end_col) = if i == end.seg {
            (
                end.row.saturating_add(1).min(drawn),
                end.col.saturating_sub(msg_area.x),
            )
        } else {
            (drawn, width.saturating_sub(1))
        };

        let ss = ScreenSelection {
            start_row: rel_start,
            start_col,
            end_row: rel_end.saturating_sub(1),
            end_col,
        };

        let breaks = LineBreaks::from_lines(seg.lines(), width);
        selection::append_rows(&tmp, tmp_area, &ss, rel_start, rel_end, &mut out, &breaks);
    }
    out
}
