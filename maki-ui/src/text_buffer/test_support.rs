use std::ops::Range;

use maki_lua::TextEdit;

use super::TextBuffer;
use crate::decor_worker;

pub(crate) fn text_edit(start: usize, stop: usize, text: &str) -> TextEdit {
    TextEdit {
        start,
        stop,
        text: text.into(),
    }
}

/// Where every placed mark sits, in order.
pub(crate) fn mark_ranges(buffer: &TextBuffer) -> Vec<Range<usize>> {
    let mut ranges: Vec<_> = buffer
        .marks()
        .touching(0..=usize::MAX)
        .map(|mark| mark.bytes)
        .collect();
    ranges.sort_by_key(|bytes| (bytes.start, bytes.end));
    ranges
}

/// Runs the parse worker until it has nothing left, taking in each result.
pub(crate) fn settle_syntax(buffer: &mut TextBuffer) {
    buffer.poll_syntax();
    while decor_worker::run_queued() > 0 {
        buffer.poll_syntax();
    }
}
