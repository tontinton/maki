//! What the marks add to the rows the input draws: styles, syntax colors,
//! virtual text and conceals. Only the lines on screen are decorated, once per
//! frame.

#[cfg(test)]
use std::cell::Cell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem;
use std::ops::Range;

use flume::{Receiver, TryRecvError};
use maki_agent::SpanStyle;
use maki_highlight::Highlighter;
use ratatui::style::Style;
use ratatui::text::Span;

use super::layout::Row;
use crate::components::tool_display::resolve_span_style;
use crate::decor_worker;
use crate::highlight;
use crate::text_buffer::{Change, Concealed, Placed, TextBuffer, VirtTextPos};
use crate::theme;

/// Longer text is colored on the worker, since coloring it here would hold
/// up the frame.
const MAX_SYNC_LANG: usize = 2048;
/// Keeps end of line virtual text off the line, like Neovim does, and gives a
/// caret at the end of the line a cell of its own before it.
const EOL_GAP: &str = " ";

type Paint = (Range<usize>, Style);

#[derive(Default)]
pub(super) struct LineDecor {
    /// In the order they compose, each patching the ones before it.
    paints: Vec<Paint>,
    /// Sorted by byte, each drawn right before the char at that byte.
    inline: Vec<(usize, Vec<Span<'static>>)>,
    eol: Vec<Span<'static>>,
    /// Sorted and disjoint, each with the text drawn instead.
    conceals: Vec<(Range<usize>, String)>,
}

impl LineDecor {
    /// {bytes} is where the display line sits in {value}, and {marks} come
    /// in the order they compose. {selected} is painted last, so no mark
    /// color hides it. A conceal only takes a paint that covers all of it,
    /// and a selection always covers an atomic one whole.
    pub fn new(
        value: &str,
        bytes: Range<usize>,
        marks: &[Placed],
        conceals: &[Concealed],
        selected: Option<&Range<usize>>,
        langs: &mut LangCache,
    ) -> Self {
        let mut decor = Self {
            conceals: conceals
                .iter()
                .filter(|hidden| bytes.contains(&hidden.bytes.start))
                .map(|hidden| {
                    let at = hidden.bytes.start - bytes.start..hidden.bytes.end - bytes.start;
                    (at, hidden.text.to_owned())
                })
                .collect(),
            ..Self::default()
        };
        for mark in marks
            .iter()
            .filter(|mark| mark.bytes.start <= bytes.end && mark.bytes.end >= bytes.start)
        {
            let decoration = mark.decoration;
            let range = on_line(&mark.bytes, &bytes);
            if !range.is_empty() {
                if let Some(lang) = &decoration.lang {
                    match langs.range(lang, &mark.bytes, value) {
                        Some(paints) => {
                            let shown = bytes.start + range.start..bytes.start + range.end;
                            decor.paints.extend(clipped(
                                paints,
                                mark.bytes.start,
                                shown,
                                bytes.start,
                            ));
                        }
                        // A conceal can join lines, and these are colored
                        // one line at a time.
                        None => {
                            let mut at = range.start;
                            for piece in value[bytes.clone()][range.clone()].split('\n') {
                                decor.paints.extend(shifted(langs.line(lang, piece), at));
                                at += piece.len() + 1;
                            }
                        }
                    }
                }
                if let Some(hl) = &decoration.hl {
                    decor.paints.push((range, mark_style(hl)));
                }
            }
            // Virtual text belongs to the line its mark starts on.
            if mark.bytes.start < bytes.start || decoration.virt_text.is_empty() {
                continue;
            }
            let spans = decoration
                .virt_text
                .iter()
                .map(|span| Span::styled(span.text.clone(), mark_style(&span.style)));
            match decoration.virt_text_pos {
                VirtTextPos::Inline => decor
                    .inline
                    .push((mark.bytes.start - bytes.start, spans.collect())),
                VirtTextPos::Eol => decor.eol.extend(spans),
            }
        }
        if let Some(range) = selected.map(|selected| on_line(selected, &bytes))
            && !range.is_empty()
        {
            decor.paints.push((range, theme::current().item_selected));
        }
        decor.inline.sort_by_key(|(at, _)| *at);
        decor
    }

    /// The spans of {row} and, when {caret} is a byte on it, how many chars
    /// come before the caret. Virtual text at the caret comes after it, so
    /// ghost text reads on from where the user types. {caret} is never inside
    /// a concealed range, see [`crate::text_buffer::TextBuffer::shown_caret`].
    pub fn draw(
        &self,
        line: &str,
        row: &Row,
        caret: Option<usize>,
    ) -> (Vec<Span<'static>>, Option<usize>) {
        let bytes = &row.bytes;
        let mut cuts: Vec<usize> = self
            .paints
            .iter()
            .flat_map(|(range, _)| [range.start, range.end])
            .chain(self.inline.iter().map(|(at, _)| *at))
            .chain(
                self.conceals
                    .iter()
                    .flat_map(|(hidden, _)| [hidden.start, hidden.end]),
            )
            .chain(caret)
            .chain([bytes.start, bytes.end])
            .filter(|at| (bytes.start..=bytes.end).contains(at))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();

        let mut spans = Vec::new();
        let mut chars = 0;
        let mut caret_chars = None;
        let mut hidden_until = 0;
        for (i, &at) in cuts.iter().enumerate() {
            if at < hidden_until {
                continue;
            }
            if caret == Some(at) {
                caret_chars = Some(chars);
            }
            if row.holds_virt(at) {
                for (_, virt) in self.inline.iter().filter(|(v, _)| *v == at) {
                    chars += virt
                        .iter()
                        .map(|span| span.content.chars().count())
                        .sum::<usize>();
                    spans.extend(virt.iter().cloned());
                }
            }
            if let Ok(found) = self
                .conceals
                .binary_search_by_key(&at, |(hidden, _)| hidden.start)
            {
                let (hidden, text) = &self.conceals[found];
                hidden_until = hidden.end;
                chars += text.chars().count();
                if !text.is_empty() {
                    spans.push(Span::styled(text.clone(), self.style_of(hidden.clone())));
                }
                continue;
            }
            if let Some(&next) = cuts.get(i + 1) {
                let text = &line[at..next];
                chars += text.chars().count();
                spans.push(Span::styled(text.to_owned(), self.style_of(at..next)));
            }
        }
        if row.last && !self.eol.is_empty() {
            spans.push(Span::raw(EOL_GAP));
            spans.extend(self.eol.iter().cloned());
        }
        (spans, caret_chars)
    }

    /// The paints over all of {bytes}. Text never straddles a paint's edge,
    /// since every edge is a cut, while a conceal takes only the paints
    /// over the whole of it.
    fn style_of(&self, bytes: Range<usize>) -> Style {
        self.paints
            .iter()
            .filter(|(range, _)| range.start <= bytes.start && bytes.end <= range.end)
            .fold(Style::default(), |style, (_, paint)| style.patch(*paint))
    }
}

/// Syntax colors of the `lang` ranges drawn in the last two frames.
///
/// A range is colored as a whole, so a construct spanning lines, like a block
/// comment, looks right on every line of it. Colors are kept by language and
/// where the range sits, and each frame moves them through the edits since
/// the last one, dropping the ones an edit touched. A frame then never reads
/// the text of a range it already colored, however big it is, and a keystroke
/// colors again only the range it changed.
///
/// A long range would hold up the frame, so it goes to the worker, one at a
/// time, and until it is back each of its lines on screen is colored on its
/// own. Typing in a long code block then never shows it plain, nor queues a
/// job per keystroke.
#[derive(Default)]
pub(super) struct LangCache {
    theme: u64,
    /// The buffer version the ranges below point into.
    version: u64,
    drawn: Ranges,
    before: Ranges,
    /// Lines of a range the worker is coloring, keyed by language and text.
    lines: HashMap<u64, Vec<Paint>>,
    lines_before: HashMap<u64, Vec<Paint>>,
    pending: Option<Pending>,
}

/// Paints counted from the first byte of their range.
type Ranges = HashMap<String, HashMap<Range<usize>, Vec<Paint>>>;

/// The range the worker is coloring, moved like the cached ones, or `None`
/// once an edit touched it and its paints no longer fit.
struct Pending {
    lang: String,
    bytes: Option<Range<usize>>,
    theme: u64,
    colored: Receiver<Vec<Paint>>,
}

impl LangCache {
    pub fn next_frame(&mut self, buffer: &TextBuffer) {
        let theme = maki_highlight::theme_generation();
        if theme != self.theme {
            self.theme = theme;
            self.drawn.clear();
            self.lines.clear();
        }
        if buffer.version() != self.version {
            self.catch_up(buffer.changes_since(self.version));
            self.version = buffer.version();
        }
        self.before = mem::take(&mut self.drawn);
        self.lines_before = mem::take(&mut self.lines);
    }

    /// Moves every range through {changes}. Without them nothing is known to
    /// be where it was, so everything goes.
    fn catch_up<'a>(&mut self, changes: Option<impl Iterator<Item = &'a Change> + Clone>) {
        let moved = |bytes: Range<usize>| map_forward(bytes, changes.clone()?);
        for ranges in self.drawn.values_mut() {
            *ranges = mem::take(ranges)
                .into_iter()
                .filter_map(|(bytes, paints)| Some((moved(bytes)?, paints)))
                .collect();
        }
        if let Some(pending) = &mut self.pending {
            pending.bytes = pending.bytes.take().and_then(moved);
        }
    }

    /// Takes in what the worker colored. `true` when the frame has to show
    /// it. A job that died lets the next long range go to the worker.
    pub fn poll(&mut self) -> bool {
        let colored = match self
            .pending
            .as_ref()
            .map(|pending| pending.colored.try_recv())
        {
            None | Some(Err(TryRecvError::Empty)) => return false,
            Some(colored) => colored.ok(),
        };
        let Some(Pending {
            lang,
            bytes: Some(bytes),
            theme,
            ..
        }) = self.pending.take()
        else {
            return false;
        };
        let Some(paints) = colored.filter(|_| theme == self.theme) else {
            return false;
        };
        self.drawn.entry(lang).or_default().insert(bytes, paints);
        true
    }

    /// The paints of {bytes} of {value} as {lang}. `None` while the worker
    /// colors it.
    fn range(&mut self, lang: &str, bytes: &Range<usize>, value: &str) -> Option<&[Paint]> {
        if !highlight::is_ready() {
            return Some(&[]);
        }
        if !self
            .drawn
            .get(lang)
            .is_some_and(|ranges| ranges.contains_key(bytes))
        {
            let kept = self
                .before
                .get_mut(lang)
                .and_then(|ranges| ranges.remove(bytes));
            let paints = match kept {
                Some(paints) => paints,
                None if bytes.len() <= MAX_SYNC_LANG => highlight_text(lang, &value[bytes.clone()]),
                None => {
                    self.color_later(lang, bytes, value);
                    return None;
                }
            };
            self.drawn
                .entry(lang.to_owned())
                .or_default()
                .insert(bytes.clone(), paints);
        }
        self.drawn.get(lang)?.get(bytes).map(Vec::as_slice)
    }

    fn line(&mut self, lang: &str, text: &str) -> &[Paint] {
        if text.len() > MAX_SYNC_LANG || !highlight::is_ready() {
            return &[];
        }
        let key = cache_key(lang, text);
        match self.lines.entry(key) {
            Entry::Occupied(hit) => hit.into_mut(),
            Entry::Vacant(miss) => miss.insert(
                self.lines_before
                    .remove(&key)
                    .unwrap_or_else(|| highlight_text(lang, text)),
            ),
        }
    }

    fn color_later(&mut self, lang: &str, bytes: &Range<usize>, value: &str) {
        if self.pending.is_some() {
            return;
        }
        let (paints, colored) = flume::bounded(1);
        self.pending = Some(Pending {
            lang: lang.to_owned(),
            bytes: Some(bytes.clone()),
            theme: self.theme,
            colored,
        });
        let (lang, text) = (lang.to_owned(), value[bytes.clone()].to_owned());
        decor_worker::spawn(move || {
            let _ = paints.send(highlight_text(&lang, &text));
        });
    }
}

/// {bytes} moved through {changes}, or `None` when one of them touches it,
/// since paints colored for the old text no longer fit.
fn map_forward<'a>(
    bytes: Range<usize>,
    mut changes: impl Iterator<Item = &'a Change>,
) -> Option<Range<usize>> {
    changes.try_fold(bytes, |bytes, change| {
        (change.start > bytes.end || change.old_end < bytes.start)
            .then(|| change.map_pos(bytes.start, false)..change.map_pos(bytes.end, false))
    })
}

fn cache_key(lang: &str, text: &str) -> u64 {
    #[cfg(test)]
    count_read(text);
    let mut hasher = DefaultHasher::new();
    (lang, text).hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
thread_local! {
    static BYTES_READ: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn count_read(text: &str) {
    BYTES_READ.set(BYTES_READ.get() + text.len());
}

/// How many bytes of text were hashed or colored on this thread since the
/// last call. Jobs run on the test thread under [`decor_worker::manual`], so
/// theirs count.
#[cfg(test)]
pub(super) fn take_bytes_read() -> usize {
    BYTES_READ.take()
}

/// A tab is one byte to the input, so it is swapped for a space, which keeps
/// every offset, before the highlighter expands it into several.
fn highlight_text(lang: &str, text: &str) -> Vec<Paint> {
    #[cfg(test)]
    count_read(text);
    let mut highlighter = Highlighter::for_token(lang);
    let mut paints = Vec::new();
    let mut line_start = 0;
    for line in text.split('\n') {
        let source = format!("{}\n", line.replace('\t', " "));
        let mut at = line_start;
        for span in highlight::highlight_line(&mut highlighter, &source) {
            let bytes = at..at + span.content.len();
            at = bytes.end;
            paints.push((bytes, span.style));
        }
        line_start += line.len() + 1;
    }
    paints
}

/// The part of {range} on the display line at {line}, counted from where
/// the line starts. Empty when it misses the line.
fn on_line(range: &Range<usize>, line: &Range<usize>) -> Range<usize> {
    let clamp = |at: usize| at.clamp(line.start, line.end) - line.start;
    clamp(range.start)..clamp(range.end)
}

fn shifted(paints: &[Paint], by: usize) -> impl Iterator<Item = Paint> + '_ {
    paints
        .iter()
        .map(move |(range, style)| (range.start + by..range.end + by, *style))
}

/// The {paints} of a range starting at byte {origin} that fall in {shown},
/// cut to it and counted from {base}. They follow each other without gaps,
/// so their ends are sorted too and the first one is found by bisection.
fn clipped(
    paints: &[Paint],
    origin: usize,
    shown: Range<usize>,
    base: usize,
) -> impl Iterator<Item = Paint> + '_ {
    let first = paints.partition_point(|(bytes, _)| origin + bytes.end <= shown.start);
    paints[first..]
        .iter()
        .take_while(move |(bytes, _)| origin + bytes.start < shown.end)
        .map(move |(bytes, style)| {
            let start = (origin + bytes.start).max(shown.start);
            let end = (origin + bytes.end).min(shown.end);
            (start - base..end - base, *style)
        })
}

/// A span with no style looks like the text around it, not like tool output.
fn mark_style(style: &SpanStyle) -> Style {
    match style {
        SpanStyle::Default => Style::default(),
        other => resolve_span_style(other),
    }
}
