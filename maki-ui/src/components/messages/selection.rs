use super::segment::SegmentCache;
use crate::selection::{self, LineBreaks, ScreenSelection, Selection};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget, Wrap};

pub(super) fn extract_selection_text(
    cache: &SegmentCache,
    viewport_width: u16,
    sel: &Selection,
    msg_area: Rect,
) -> String {
    let (doc_start, doc_end) = sel.normalized();
    let width = viewport_width;

    let heights: Vec<u16> = cache.segments().iter().map(|s| s.height(width)).collect();

    let mut out = String::new();
    let mut doc_row: u32 = 0;

    for (i, &h) in heights.iter().enumerate() {
        let seg_start = doc_row;
        let seg_end = doc_row + h as u32;
        doc_row = seg_end;

        if seg_end <= doc_start.row || seg_start > doc_end.row {
            continue;
        }

        if !out.is_empty() {
            out.push('\n');
        }

        let Some(seg) = cache.get(i) else { continue };

        if seg.lines().is_empty() {
            continue;
        }

        let rel_start = doc_start.row.saturating_sub(seg_start) as usize;
        let rel_end = ((doc_end.row + 1).saturating_sub(seg_start) as usize).min(h as usize);

        let start_col = if seg_start > doc_start.row {
            0
        } else {
            doc_start.col.saturating_sub(msg_area.x)
        };
        let end_col = if seg_end < doc_end.row + 1 {
            width.saturating_sub(1)
        } else {
            doc_end.col.saturating_sub(msg_area.x)
        };

        let content_start = msg_area.x + seg.prefix_width;
        let seg_fully_selected = seg_start >= doc_start.row
            && seg_end <= doc_end.row + 1
            && doc_start.col <= content_start
            && doc_end.col >= msg_area.x + width - 1;
        if seg_fully_selected && let Some(raw) = &seg.raw_text {
            out.push_str(raw);
            continue;
        }

        let tmp_area = Rect::new(0, 0, width, h);
        let mut tmp = Buffer::empty(tmp_area);
        Paragraph::new(seg.lines().to_vec())
            .wrap(Wrap { trim: false })
            .render(tmp_area, &mut tmp);

        let ss = ScreenSelection {
            start_row: rel_start as u16,
            start_col,
            end_row: rel_end.saturating_sub(1) as u16,
            end_col,
        };

        let breaks = LineBreaks::from_lines(seg.lines(), width);
        selection::append_rows(&tmp, tmp_area, &ss, rel_start as u16, rel_end as u16, &mut out, &breaks);
    }
    out
}
