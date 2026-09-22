use std::mem;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use maki_agent::file_index::MAX_ENTRIES;
use maki_agent::{FILE_MATCH_CONFIG, FileReader, file_haystack_owned, file_index};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Matcher, Nucleo};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tracing::warn;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::animation::spinner_frame;
use crate::components::Overlay;
use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::repaint::{Cadence, Dirty};
use crate::text_buffer::TextBuffer;
use crate::theme;

const TITLE: &str = " Files ";
const TITLE_WALKING: &str = " Files (scanning…) ";
const WIDTH_PERCENT: u16 = 60;
const MAX_HEIGHT_PERCENT: u16 = 80;
const SEARCH_ROW: u16 = 1;
const NO_MATCHES: &str = "  No matches";
const LABEL_INDENT: &str = "  ";
/// Not "empty": a directory full of ignored files walks up just as short.
const NOTHING_TO_PICK_MSG: &str = "Nothing to pick in the current directory";
pub(crate) const UNREADABLE_DIR_MSG: &str = "Cannot list the current directory";
const WALKER_CRASHED_MSG: &str = "File scanner crashed";
const PENDING_DEBOUNCE_MS: u128 = 100;
const MAX_MATERIALIZED: u32 = 640;
/// Paths handed to the matcher in one tick. A generation swap starts the
/// cursor over, so without a ceiling one tick on the render thread owns an
/// allocation and a haystack per path in the whole corpus.
const INJECT_BATCH: usize = 2048;

static TITLE_CAPPED: LazyLock<String> =
    LazyLock::new(|| format!(" Files (showing the first {MAX_ENTRIES} entries) "));

/// An empty directory, a fully ignored one and one that could not be opened
/// all look identical from the injector's side, so how the walk ended is what
/// tells them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Walk {
    Running,
    Listed,
    /// The walk stopped at `MAX_ENTRIES`, so the list is a prefix of the tree
    /// and which prefix differs from one walk to the next.
    Capped,
    Unreadable,
    Crashed,
}

impl Walk {
    /// What to tell the user when the walk is over and the list is still empty.
    /// Total over the state, so no ending can be forgotten.
    fn nothing_found_msg(self) -> Option<&'static str> {
        match self {
            Self::Running => None,
            Self::Listed | Self::Capped => Some(NOTHING_TO_PICK_MSG),
            Self::Unreadable => Some(UNREADABLE_DIR_MSG),
            Self::Crashed => Some(WALKER_CRASHED_MSG),
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Running => TITLE_WALKING,
            Self::Capped => &TITLE_CAPPED,
            Self::Listed | Self::Unreadable | Self::Crashed => TITLE,
        }
    }
}

pub enum FilePickerModalAction {
    Consumed,
    Select(String),
    Close,
}

struct Match {
    /// The path exactly as the walk found it. Never read back off the match
    /// column: a haystack holds the first codepoint of each grapheme, so
    /// `src/e\u{301}dit.rs` comes back out of it as `src/edit.rs`, a name the
    /// filesystem that handed us the first one does not have. This is what
    /// Enter inserts and what the editor is asked to open.
    path: String,
    /// The haystack units the query matched: bytes for an ASCII path,
    /// grapheme clusters for any other, which is what
    /// [`maki_agent::byte_highlights`] turns into byte ranges of
    /// [`Self::path`].
    indices: Vec<u32>,
}

struct Session {
    /// Carries the original path as item data, because the matcher column it
    /// is scored through cannot give it back (see [`Match::path`]).
    nucleo: Nucleo<String>,
    matcher: Matcher,
    matches: Vec<Match>,
    total_matches: u32,

    search: TextBuffer,
    selected: usize,
    /// The path the user had selected when a re-walk swapped the list under
    /// them, held until that path turns up in the rebuilt list or the user
    /// picks a row themselves.
    reselect: Option<String>,
    scroll_offset: usize,
    viewport_height: usize,
    inner_area: Rect,

    /// The walk this picker draws. Shared with every other file ranking in
    /// the process, so opening the picker twice walks the tree once, and held
    /// as a reader so closing the picker only stops the walk when nothing
    /// else is reading it.
    index: FileReader,
    /// Entries already handed to the matcher, so a tick only picks up what
    /// the walk added since the last one. Only meaningful against the
    /// generation below: a re-walk replaces the list rather than extending it,
    /// and a cursor into the old one then names different entries.
    injected: usize,
    /// The corpus generation `injected` counts into.
    generation: u64,
    /// Whether the root could be listed at all, asked once when the picker
    /// opened rather than read off a corpus that can be twenty seconds old.
    readable: bool,
    started_at: Instant,

    walk: Walk,
    /// The matcher owes an answer. Nothing delivers it, so `tick` has to look.
    matching: bool,
    visible: bool,
}

pub struct FilePickerModal {
    session: Option<Session>,
}

impl FilePickerModal {
    pub fn new() -> Self {
        Self { session: None }
    }

    pub fn open(&mut self, cwd: &str) {
        self.close();
        let root = Path::new(cwd);
        let index = file_index(root);
        // `Ctrl+S` is the user asking to see the tree as it is now, and the
        // last walk can be twenty seconds old. The fresh walk builds
        // alongside the list the last one left rather than over it, so the
        // rows the picker opens on are the rows it keeps until the whole new
        // tree is ready to take their place in one go.
        index.rescan();
        self.session = Some(session(index.reader(), readable(root)));
    }

    pub fn close(&mut self) {
        self.session = None;
    }

    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.visible && s.inner_area.contains(pos))
    }

    pub fn scroll(&mut self, delta: i32) {
        let Some(s) = &mut self.session else { return };
        if delta > 0 {
            move_selection(s, -(delta as isize));
        } else {
            move_selection(s, delta.unsigned_abs() as isize);
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        let Some(s) = &mut self.session else {
            return false;
        };
        s.search.insert_text(text);
        reparse_pattern(s);
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FilePickerModalAction {
        let Some(s) = &mut self.session else {
            return FilePickerModalAction::Close;
        };

        match key.code {
            KeyCode::Esc => return FilePickerModalAction::Close,
            KeyCode::Enter => {
                if !s.visible {
                    return FilePickerModalAction::Consumed;
                }
                if let Some(m) = s.matches.get(s.selected) {
                    return FilePickerModalAction::Select(m.path.clone());
                }
                return FilePickerModalAction::Close;
            }
            KeyCode::Up => move_selection(s, -1),
            KeyCode::Down => move_selection(s, 1),
            KeyCode::Backspace => {
                s.search.remove_char();
                reparse_pattern(s);
            }
            KeyCode::Left => s.search.move_left(),
            KeyCode::Right => s.search.move_right(),
            KeyCode::Home => s.search.move_home(),
            KeyCode::End => s.search.move_end(),
            _ if key::DELETE_WORD.matches(key) => {
                s.search.remove_word_before_cursor();
                reparse_pattern(s);
            }
            _ if key::SCROLL_HALF_UP.matches(key) => {
                move_selection(s, -((s.viewport_height / 2).max(1) as isize))
            }
            _ if key::SCROLL_HALF_DOWN.matches(key) => {
                move_selection(s, (s.viewport_height / 2).max(1) as isize)
            }
            _ if key::SCROLL_LINE_UP.matches(key) => move_selection(s, -1),
            _ if key::SCROLL_LINE_DOWN.matches(key) => move_selection(s, 1),
            _ if super::is_ctrl(&key) => {}
            KeyCode::Char(c) => {
                s.search.push_char(c);
                reparse_pattern(s);
            }
            _ => {}
        }
        FilePickerModalAction::Consumed
    }

    pub fn cadence(&self) -> Cadence {
        let Some(s) = self.session.as_ref() else {
            return Cadence::IDLE;
        };
        Cadence::any([
            Cadence::when(s.visible && s.walk == Walk::Running, Cadence::SPINNER),
            // Results stream in all through the walk, and the spinner above is
            // already bringing the loop back for them. Once it ends, every
            // keystroke leaves one last answer in flight, and the list sits on
            // the old query until someone looks.
            Cadence::when(s.matching && s.walk != Walk::Running, Cadence::PENDING),
            // A corpus is handed over a batch per tick, so the loop has to
            // come back for the rest of it however quiet everything else is.
            Cadence::when(s.injected < s.index.corpus().len(), Cadence::PENDING),
        ])
    }

    /// Returns the frame owed plus a message to flash if the picker gave up.
    pub fn tick(&mut self) -> (Dirty, Option<String>) {
        let Some(s) = self.session.as_mut() else {
            return (Dirty::NO, None);
        };

        let walked = pull(s);
        let status = s.nucleo.tick(0);
        s.matching = status.running;
        // The title says "scanning…" while walking, so finishing redraws too.
        let mut dirty = Dirty::from(status.changed) | Dirty::from(walked);

        // Counted here rather than off the injector: `injected_items` only
        // grows, so a re-walk that swapped in an empty or unreadable corpus
        // would be masked by whatever the previous one had found.
        let has_files = s.injected > 0;

        // A walk slow enough to cross the debounce is already on screen when it
        // answers, so the close cannot sit behind the visibility gate below.
        if !has_files && let Some(msg) = s.walk.nothing_found_msg() {
            self.session = None;
            return (Dirty::YES, Some(msg.into()));
        }

        if !s.visible && (has_files || s.started_at.elapsed().as_millis() >= PENDING_DEBOUNCE_MS) {
            s.visible = true;
            dirty = Dirty::YES;
        }

        if status.changed {
            refresh_matches(s);
            restore_selection(s);
            clamp_selection(s);
        }

        (dirty, None)
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let s = match &mut self.session {
            Some(s) if s.visible => s,
            _ => return Rect::default(),
        };

        let match_count = s.matches.len() as u16;

        let has_query_without_matches = s.matches.is_empty() && !s.search.value().is_empty();
        let max_visible = area.height.saturating_sub(SEARCH_ROW + 2);
        let content_rows = if has_query_without_matches {
            1
        } else {
            match_count.min(max_visible)
        };

        let modal = Modal {
            title: s.walk.title(),
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, content_rows + SEARCH_ROW);
        s.inner_area = inner;
        s.viewport_height = inner.height.saturating_sub(SEARCH_ROW) as usize;
        ensure_visible(s);

        let [list_area, search_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

        render_list(frame, list_area, s);
        render_search(frame, search_area, s);

        if match_count > s.viewport_height as u16 {
            render_vertical_scrollbar(
                frame,
                list_area,
                u32::from(match_count),
                s.scroll_offset as u32,
            );
        }

        popup
    }
}

impl Overlay for FilePickerModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }

    fn cadence(&self) -> Cadence {
        self.cadence()
    }
}

/// Only the directory itself can say whether an empty walk means "nothing to
/// pick" or "I could not even look", and the answer has to be about now rather
/// than about whenever the last walk happened to end.
fn readable(root: &Path) -> bool {
    match root.read_dir() {
        Ok(_) => true,
        Err(e) => {
            warn!("{UNREADABLE_DIR_MSG}: {}: {e}", root.display());
            false
        }
    }
}

/// A picker over {index}, hidden until the walk has something to show.
fn session(index: FileReader, readable: bool) -> Session {
    let notify = Arc::new(|| {});
    Session {
        nucleo: Nucleo::new(FILE_MATCH_CONFIG, notify, None, 1),
        matcher: Matcher::new(FILE_MATCH_CONFIG),
        matches: Vec::new(),
        total_matches: 0,
        search: TextBuffer::new(String::new()),
        selected: 0,
        reselect: None,
        scroll_offset: 0,
        viewport_height: 0,
        inner_area: Rect::default(),
        index,
        injected: 0,
        generation: 0,
        readable,
        started_at: Instant::now(),
        walk: Walk::Running,
        matching: false,
        visible: false,
    }
}

/// Hands the matcher whatever the shared walk has found since the last tick.
/// Returns whether that changed anything worth a frame, which for a walk
/// still running is the spinner and the title tracking it.
///
/// A re-walk replaces the list in an order no previous walk can predict, so a
/// cursor carried across that swap pushes an arbitrary suffix of the new list
/// on top of the old one. The matcher cannot take an item back, so a new
/// generation has to start the matcher over.
fn pull(s: &mut Session) -> bool {
    let corpus = s.index.corpus();
    // Two halves of one question: a walk is still owed this picker a list,
    // which is what `Ctrl+S` and a tool writing a file both leave behind, or
    // one is filling the list right now. Asking first is the point: a walk
    // another reader cancelled a moment before this picker opened, and a walk
    // the host had no slot to start, are both walks that happen only because
    // the reader waiting for one says so. A list that has landed and has not
    // been marked stale since answers no and starts nothing.
    let owed = s.index.scanning() || !corpus.complete;
    if corpus.generation() != s.generation {
        // The path on the selected row has to come back by name: the new list
        // is a different list, re-ranked and re-numbered, so row N of it is a
        // different file. Row 0 included, because with an empty query it is
        // whatever the fresh walk happened to reach first, and it is the row
        // the user reads and the row Enter takes without arrowing at all.
        s.reselect = s.matches.get(s.selected).map(|m| m.path.clone());
        // Not `restart(true)`: that empties the snapshot there and then, and
        // the list would collapse to nothing and grow back a batch a tick
        // while the user is looking at it. The rows on show stay the old
        // ones until the new match run has an answer to put in their place.
        s.nucleo.restart(false);
        s.generation = corpus.generation();
        s.injected = 0;
    }
    let injector = s.nucleo.injector();
    let mut pushed = 0;
    // A batch per tick. A swap starts the cursor over, and a whole corpus
    // pushed in one tick is up to `MAX_ENTRIES` owned paths and haystacks
    // built on the render thread.
    for path in corpus.tail(s.injected).take(INJECT_BATCH) {
        // Built where `maki.fs.fuzzy_files` builds its own, because a
        // haystack of different units is a different score and a different
        // order for the same path over the same corpus.
        injector.push(path.to_owned(), |path, cols| {
            cols[0] = file_haystack_owned(path)
        });
        pushed += 1;
    }
    s.injected += pushed;
    // Handing the corpus over is part of the scan as far as the user is
    // concerned: a path the matcher has not been told about yet cannot match,
    // and a title that says the scan is done over a list reading "No matches"
    // is a lie they act on. Read after the injection above, so the tick that
    // hands over the last batch is the tick the title settles on.
    let walk = match (owed || s.injected < corpus.len(), corpus.crashed) {
        (true, _) => Walk::Running,
        // Ahead of a crash: a dead re-walk keeps the capped list it had.
        (false, _) if corpus.truncated => Walk::Capped,
        (false, true) => Walk::Crashed,
        (false, false) if s.readable => Walk::Listed,
        (false, false) => Walk::Unreadable,
    };
    let moved = mem::replace(&mut s.walk, walk) != walk;
    moved || pushed > 0
}

fn reparse_pattern(s: &mut Session) {
    let query = s.search.value();
    s.nucleo
        .pattern
        .reparse(0, &query, CaseMatching::Smart, Normalization::Smart, false);
    s.selected = 0;
    s.scroll_offset = 0;
    s.reselect = None;
}

fn refresh_matches(s: &mut Session) {
    let snapshot = s.nucleo.snapshot();
    s.total_matches = snapshot.matched_item_count();
    let count = s.total_matches.min(MAX_MATERIALIZED);

    s.matches.clear();

    let pattern = snapshot.pattern();
    let has_pattern = !pattern.column_pattern(0).atoms.is_empty();
    let mut indices_buf = Vec::new();

    for item in snapshot.matched_items(0..count) {
        let indices = if has_pattern {
            indices_buf.clear();
            pattern.column_pattern(0).indices(
                item.matcher_columns[0].slice(..),
                &mut s.matcher,
                &mut indices_buf,
            );
            mem::take(&mut indices_buf)
        } else {
            Vec::new()
        };

        s.matches.push(Match {
            path: item.data.clone(),
            indices,
        });
    }
}

/// Puts the selection back on the path a generation swap took it off, once
/// the rebuilt list has that path again. By name rather than by index: the
/// new list is a different list, and row N of it is a different file.
fn restore_selection(s: &mut Session) {
    let Some(path) = s.reselect.take() else {
        return;
    };
    let Some(at) = s.matches.iter().position(|m| m.path == path) else {
        s.reselect = Some(path);
        return;
    };
    s.selected = at;
    ensure_visible(s);
}

fn move_selection(s: &mut Session, delta: isize) {
    if s.matches.is_empty() {
        return;
    }
    // The user moving is the user choosing, which outranks a row a swap was
    // still trying to get back to.
    s.reselect = None;
    let new = (s.selected as isize + delta).clamp(0, s.matches.len() as isize - 1);
    s.selected = new as usize;
    ensure_visible(s);
}

fn clamp_selection(s: &mut Session) {
    if s.matches.is_empty() {
        s.selected = 0;
        s.scroll_offset = 0;
    } else {
        s.selected = s.selected.min(s.matches.len() - 1);
        ensure_visible(s);
    }
}

fn ensure_visible(s: &mut Session) {
    let len = s.matches.len();
    if len > s.viewport_height {
        s.scroll_offset = s.scroll_offset.min(len - s.viewport_height);
    } else {
        s.scroll_offset = 0;
    }

    if s.selected < s.scroll_offset {
        s.scroll_offset = s.selected;
    } else if s.selected >= s.scroll_offset + s.viewport_height {
        s.scroll_offset = s.selected + 1 - s.viewport_height;
    }
}

fn render_list(frame: &mut Frame, area: Rect, s: &Session) {
    let t = theme::current();

    if s.matches.is_empty() {
        if !s.search.value().is_empty() {
            frame.render_widget(
                Paragraph::new(vec![Line::from(Span::styled(NO_MATCHES, t.item_desc))]),
                area,
            );
        }
        return;
    }

    let more = s.total_matches > MAX_MATERIALIZED;
    let at_bottom = s.scroll_offset + s.viewport_height >= s.matches.len();
    let hint_row = usize::from(more && at_bottom);
    // A viewport one row tall spends it all on the hint, and `end` below would
    // then slice backwards from `scroll_offset`.
    let visible_rows = s.viewport_height.saturating_sub(hint_row);

    let max_label_width = area.width.saturating_sub(LABEL_INDENT.len() as u16) as usize;
    let end = (s.scroll_offset + visible_rows).min(s.matches.len());

    let mut lines: Vec<Line> = s.matches[s.scroll_offset..end]
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let selected = s.scroll_offset + i == s.selected;
            build_highlighted_line(&m.path, &m.indices, max_label_width, selected, &t)
        })
        .collect();

    if hint_row > 0 {
        let n = s.total_matches - MAX_MATERIALIZED;
        lines.push(Line::from(Span::styled(
            format!("{LABEL_INDENT}+{n} more files (not shown)"),
            t.item_desc,
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_search(frame: &mut Frame, area: Rect, s: &Session) {
    let t = theme::current();
    let query = s.search.value();
    let cursor_byte = TextBuffer::char_to_byte(&query, s.search.x());
    let (before, rest) = query.split_at(cursor_byte);
    let mut chars = rest.chars();
    let cursor_char = chars.next().unwrap_or(' ');
    let after = chars.as_str();

    let mut spans = vec![super::chevron_span()];

    if s.walk == Walk::Running {
        let ch = spinner_frame(s.started_at.elapsed().as_millis());
        spans.push(Span::styled(format!("{ch} "), t.item_desc));
    }

    spans.extend([
        Span::styled(before.to_owned(), Style::default()),
        Span::styled(cursor_char.to_string(), t.cursor),
        Span::styled(after.to_owned(), Style::default()),
    ]);

    frame.render_widget(Paragraph::new(vec![Line::from(spans)]), area);
}

fn build_highlighted_line<'a>(
    text: &str,
    indices: &[u32],
    max_width: usize,
    selected: bool,
    t: &'a theme::Theme,
) -> Line<'a> {
    let (base, highlight) = match selected {
        true => (t.item_selected, t.item_match_selected),
        false => (t.item, t.item_match),
    };

    let mut spans = vec![Span::styled(LABEL_INDENT, base)];
    let mut in_match = false;
    let mut run = String::new();
    let mut width = 0usize;

    // By grapheme rather than by char, because that is what {indices} counts
    // for a path that is not ASCII. Stepping per char would slide every
    // highlight one place right of the mark it belongs to for each combining
    // mark before it, and could cut a cluster in half.
    for (i, unit) in text.graphemes(true).enumerate() {
        let unit_width = unit.width();
        if width + unit_width > max_width {
            break;
        }
        width += unit_width;

        let is_match = indices.binary_search(&(i as u32)).is_ok();
        if is_match != in_match && !run.is_empty() {
            spans.push(Span::styled(
                mem::take(&mut run),
                if in_match { highlight } else { base },
            ));
        }
        in_match = is_match;
        run.push_str(unit);
    }

    if !run.is_empty() {
        spans.push(Span::styled(run, if in_match { highlight } else { base }));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repaint::expect::{OWED, QUIET};
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use maki_agent::{FileIndex, FileQuery, Ranked, file_pattern};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tempfile::TempDir;
    use test_case::test_case;

    /// Waits on the matcher are bounded by wall clock, not by a tick budget:
    /// nucleo matches on a worker thread, and a tight loop can burn through N
    /// ticks before that thread is ever scheduled.
    const CONVERGE_TIMEOUT: Duration = Duration::from_secs(5);
    /// Far enough from `PENDING_DEBOUNCE_MS` that no scheduling delay can
    /// cross it in either direction.
    const DEBOUNCE_HELD_OFF: Duration = Duration::from_secs(60);
    const NEVER_CONVERGED: &str = "picker never rebuilt its matches from later ticks";
    const NEVER_CLOSED: &str = "picker never closed on an empty walk";
    const QUERY_ANSWERED: &str = "an uncancelled query answers";
    const NOT_A_TIE: &str = "the fixture was supposed to score the same for every path";

    const MAIN_PATH: &str = "src/main.rs";
    const README_PATH: &str = "docs/readme.md";
    const README_QUERY: &str = "readme";
    const MAIN_FILE: &str = "main.rs";
    /// Three ways of containing "main", scored high to low: at the start of
    /// the name, buried inside a word, and scattered across one. Distinct
    /// scores by construction, so the order below is the ranking rather than
    /// whatever an unstable sort did with a tie.
    const RANK_MIDWORD: &str = "vendor/xmain.rs";
    const RANK_SCATTERED: &str = "mail_n.rs";
    /// Never walked, so it never has to exist.
    const DETACHED_ROOT: &str = "/detached";
    const A: &str = "a.rs";
    const B: &str = "b.rs";
    const C: &str = "c.rs";
    const RENAMED: &str = "d.rs";
    const MAIN_QUERY: &str = "main";
    /// `docs/readme.md` has no `a` after either of its `m`s, so it is the one
    /// file in the corpus below that "main" cannot reach.
    const MAIN_MATCHES: usize = 3;
    /// A one character query scores on the boundary it lands after and
    /// nothing else, so these three score the same and only the tie break
    /// decides the order the user sees. The first two are the same length,
    /// which leaves insertion order to decide between them.
    const TIE_QUERY: &str = "a";
    const TIE_FIRST: &str = "two/a.rs";
    const TIE_SECOND: &str = "one/a.rs";
    const TIE_LONGER: &str = "three/nested/a.rs";
    /// `e` plus a combining acute: one grapheme, three bytes, and an ASCII
    /// first codepoint, so the index used to score nine bytes where the
    /// picker scored eleven graphemes. Both match "edit" right after the
    /// separator, so they score the same and the shorter haystack wins.
    const NFD_PATH: &str = "src/e\u{301}dit.rs";
    const NFD_OTHER: &str = "src/editor.rs";
    const NFD_QUERY: &str = "edit";
    /// The bytes of `NFD_PATH` the four matched units cover, 1-based and
    /// inclusive: one range, opening on the `e` and closing after the `t`,
    /// with the acute inside it rather than cut off its `e`.
    const NFD_HIGHLIGHT_RANGE: [(u32, u32); 1] = [(5, 10)];
    /// The units `NFD_QUERY` matches in `NFD_PATH`: the `e` carrying the
    /// acute, and the three graphemes after it.
    const NFD_MATCHED_UNITS: [u32; 4] = [4, 5, 6, 7];
    /// `NFD_PATH` cut into the spans that highlight names, indent first. The
    /// acute rides along with the `e` it sits on rather than opening a span
    /// of its own.
    const NFD_HIGHLIGHT_SPANS: [&str; 4] = [LABEL_INDENT, "src/", "e\u{301}dit", ".rs"];
    /// Enough that a walk of them is still going when the picker is reopened,
    /// and few enough that the reopened one finishes well inside the timeout.
    const REOPENED_FILES: usize = 64;
    /// Wide enough that the modal's share of it fits the capped title whole.
    const TERMINAL_WIDTH: u16 = 120;
    const TERMINAL_HEIGHT: u16 = 30;
    static NEVER: AtomicBool = AtomicBool::new(false);

    /// Ticks until `ready` holds, collecting the frames owed on the way, or
    /// `None` if the picker never got there.
    fn tick_until(picker: &mut FilePickerModal, ready: impl Fn(&Session) -> bool) -> Option<Dirty> {
        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        let mut dirty = Dirty::NO;
        while Instant::now() < deadline {
            let (owed, _) = picker.tick();
            dirty |= owed;
            if picker.session.as_ref().is_some_and(&ready) {
                return Some(dirty);
            }
            std::thread::yield_now();
        }
        None
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// A picker over an index nothing walks, so a test can put the walk in
    /// any state it likes and have it stay there.
    fn pending_picker() -> (FilePickerModal, FileIndex) {
        readable_picker(true)
    }

    fn readable_picker(readable: bool) -> (FilePickerModal, FileIndex) {
        let index = FileIndex::detached(DETACHED_ROOT);
        let mut picker = FilePickerModal::new();
        picker.session = Some(session(index.clone().reader(), readable));
        (picker, index)
    }

    fn owned(files: &[&str]) -> Vec<String> {
        files.iter().map(|p| (*p).to_owned()).collect()
    }

    /// A walk still filling, which extends the list a reader is following.
    fn walking(index: &FileIndex, files: &[&str]) {
        index.extend(owned(files), false);
    }

    /// A walk that finished, which replaces the whole list.
    fn walked(index: &FileIndex, files: &[&str]) {
        index.publish(owned(files), true);
    }

    /// The rows on screen. The previous list stays on show across a
    /// generation swap until the new match run answers, so a test waiting for
    /// a swap to land has to wait on the rows themselves and not on how many
    /// of them there are.
    fn in_order(s: &Session) -> Vec<String> {
        s.matches.iter().map(|m| m.path.clone()).collect()
    }

    fn sorted(s: &Session) -> Vec<String> {
        let mut paths = in_order(s);
        paths.sort();
        paths
    }

    fn shown_in_order(picker: &FilePickerModal) -> Vec<String> {
        in_order(picker.session.as_ref().unwrap())
    }

    fn shown_paths(picker: &FilePickerModal) -> Vec<String> {
        sorted(picker.session.as_ref().unwrap())
    }

    fn selected_path(picker: &FilePickerModal) -> String {
        let s = picker.session.as_ref().unwrap();
        s.matches[s.selected].path.clone()
    }

    fn typed(picker: &mut FilePickerModal, query: &str) {
        for c in query.chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
    }

    /// The same list through the other reader of the same corpus.
    fn ranked_by_index(index: &FileIndex, query: &str, limit: usize) -> Vec<String> {
        index
            .query(
                &FileQuery {
                    query,
                    limit,
                    highlights: false,
                },
                &NEVER,
            )
            .expect(QUERY_ANSWERED)
            .items
            .into_iter()
            .map(|item| item.path)
            .collect()
    }

    /// What the shared config scores {path} at, so a fixture that claims to be
    /// a tie has to prove it rather than be believed.
    fn score(path: &str, query: &str) -> Option<u32> {
        file_pattern(query).score(
            file_haystack_owned(path).slice(..),
            &mut Matcher::new(FILE_MATCH_CONFIG),
        )
    }

    /// Files are counted straight off the injector, so one tick settles the
    /// question. `started_at` in the future keeps the debounce out of it,
    /// however long the test is descheduled for.
    fn tick_once_before_the_debounce(picker: &mut FilePickerModal) {
        picker.session.as_mut().unwrap().started_at = Instant::now() + DEBOUNCE_HELD_OFF;
        let _ = picker.tick();
    }

    /// Nothing else in the app knows the walk is running, so the picker is the
    /// one that has to claim the spinner. `view` draws nothing until files
    /// arrive, so a hidden walk claiming `SPINNER` would animate pixels that
    /// are not on screen.
    #[test_case(&[MAIN_PATH] => Cadence::SPINNER ; "on_screen_walk_spins")]
    #[test_case(&[]          => Cadence::IDLE    ; "hidden_walk_does_not")]
    fn walking_picker_spins_only_once_it_is_on_screen(files: &[&str]) -> Cadence {
        let (mut picker, index) = pending_picker();
        walking(&index, files);
        tick_once_before_the_debounce(&mut picker);

        let s = picker.session.as_ref().unwrap();
        assert_eq!(s.walk, Walk::Running);
        assert_eq!(
            s.visible,
            !files.is_empty(),
            "the picker shows itself exactly when it has something"
        );
        picker.cadence()
    }

    /// A picker with nothing to pick closes itself: one frame, one flash, and
    /// then quiet, or the loop never settles again. The flash is the only
    /// trace the user gets, so it has to tell "there is nothing here" from "I
    /// could not look", and a picker already on screen has to close too.
    #[test_case(true,  false => NOTHING_TO_PICK_MSG  ; "walk_finished_with_nothing")]
    #[test_case(true,  true  => NOTHING_TO_PICK_MSG  ; "shown_walk_finished_with_nothing")]
    #[test_case(false, false => UNREADABLE_DIR_MSG   ; "root_could_not_be_listed")]
    fn self_close_flashes_once_then_stays_quiet(readable: bool, on_screen: bool) -> String {
        let (mut picker, index) = readable_picker(readable);
        picker.session.as_mut().unwrap().visible = on_screen;
        index.publish(Vec::new(), true);

        let (dirty, flash) = picker.tick();
        assert!(picker.session.is_none());
        assert_eq!(dirty, Dirty::YES, "{OWED}");
        assert_eq!(picker.tick(), (Dirty::NO, None), "{QUIET}");
        flash.unwrap()
    }

    /// Depth 0 is the root itself, which strips to an empty name: a bare
    /// separator at the top of the list, selected by default, one Enter away
    /// from picking the user's own directory. It also counts as an injected
    /// item, so every directory used to look non-empty.
    #[test]
    fn a_real_walk_offers_the_files_and_not_the_root_itself() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(MAIN_FILE), "").unwrap();

        let mut picker = FilePickerModal::new();
        picker.open(&tmp.path().to_string_lossy());
        let _ = tick_until(&mut picker, |s| !s.matches.is_empty()).expect(NEVER_CONVERGED);

        let s = picker.session.as_ref().unwrap();
        let paths: Vec<&str> = s.matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, [MAIN_FILE]);
    }

    /// An empty directory only reads as empty once the root stops counting as
    /// a find, so this is the close that never used to happen.
    #[test]
    fn a_real_walk_of_an_empty_directory_closes_the_picker() {
        let tmp = TempDir::new().unwrap();
        let mut picker = FilePickerModal::new();
        picker.open(&tmp.path().to_string_lossy());

        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        let flash = loop {
            if let (_, Some(flash)) = picker.tick() {
                break flash;
            }
            assert!(Instant::now() < deadline, "{NEVER_CLOSED}");
            std::thread::yield_now();
        };

        assert_eq!(flash, NOTHING_TO_PICK_MSG);
        assert!(!picker.is_open());
    }

    /// A walk with nothing to show yet still opens once it drags on, so the
    /// user is not left staring at an unchanged screen.
    #[test]
    fn pending_debounce_controls_visibility() {
        let (mut picker, _index) = pending_picker();
        tick_once_before_the_debounce(&mut picker);
        assert!(!picker.session.as_ref().unwrap().visible, "hidden so far");

        picker.session.as_mut().unwrap().started_at = Instant::now() - DEBOUNCE_HELD_OFF;
        let _ = picker.tick();
        assert!(
            picker.session.as_ref().unwrap().visible,
            "shown once the walk drags on"
        );
    }

    /// A finished walk with an unchanged query draws the same pixels every
    /// frame, so the loop has to be free to settle.
    #[test]
    fn settled_picker_owes_no_frame_and_does_not_animate() {
        let (mut picker, index) = pending_picker();
        walked(&index, &[MAIN_PATH]);

        // Waiting for a tick to owe nothing is not the same as settling: the
        // matcher answers on its own thread and can still be running on a tick
        // that changed nothing.
        let _ = tick_until(&mut picker, |s| !s.matching && s.matches.len() == 1)
            .expect(NEVER_CONVERGED);

        assert_eq!(picker.tick(), (Dirty::NO, None), "{QUIET}");
        assert_eq!(picker.cadence(), Cadence::IDLE);
    }

    /// The matcher answers on a worker thread, long after the keypress was
    /// handled, so typing is only redrawn because a later `tick` reports the
    /// change. Without that the list freezes on the previous query.
    #[test]
    fn query_change_owes_a_frame_from_a_later_tick() {
        let (mut picker, index) = pending_picker();
        walked(&index, &[MAIN_PATH, README_PATH]);
        let _ = tick_until(&mut picker, |s| s.matches.len() == 2).expect(NEVER_CONVERGED);

        for c in README_QUERY.chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }

        let dirty = tick_until(&mut picker, |s| s.matches.len() == 1).expect(NEVER_CONVERGED);
        assert_eq!(dirty, Dirty::YES, "{OWED}");
        assert_eq!(
            picker.session.as_ref().unwrap().matches[0].path,
            README_PATH
        );
    }

    /// Nucleo matches on a worker thread and hands the answer to nobody, long
    /// after the keystroke that started it. Only looking again finds it, so an
    /// idle cadence here leaves the list on the previous query until some
    /// unrelated poll comes round. It is not motion either: nothing lands, so
    /// there is nothing to paint.
    #[test_case(true,  Walk::Listed  => Cadence::PENDING ; "matching_after_the_walk")]
    #[test_case(true,  Walk::Running => Cadence::SPINNER ; "walk_spinner_already_comes_back")]
    #[test_case(false, Walk::Listed  => Cadence::IDLE    ; "settled")]
    fn a_matcher_mid_answer_keeps_the_loop_coming_back(matching: bool, walk: Walk) -> Cadence {
        let (mut picker, _index) = pending_picker();
        let s = picker.session.as_mut().unwrap();
        s.visible = true;
        s.matching = matching;
        s.walk = walk;

        picker.cadence()
    }

    /// `injected` carried across a generation swap leaves duplicate rows, a
    /// missing row for anything new before the cursor, and a stale name when
    /// the two lists are the same length. Nucleo cannot take an item back, so
    /// each of those would stay for the rest of the session.
    #[test_case(&[A, B, C], &[C, A]       ; "a_shorter_list_leaves_nothing_behind")]
    #[test_case(&[A, B],    &[A, RENAMED] ; "a_list_of_the_same_length_is_re_read")]
    #[test_case(&[A, B],    &[C, A, B]    ; "an_entry_before_the_cursor_is_not_missed")]
    fn a_corpus_swapped_under_the_picker_is_shown_once_and_whole(before: &[&str], after: &[&str]) {
        let (mut picker, index) = pending_picker();
        walked(&index, before);
        let mut first = owned(before);
        first.sort();
        let _ = tick_until(&mut picker, |s| sorted(s) == first).expect(NEVER_CONVERGED);

        walked(&index, after);
        let mut want = owned(after);
        want.sort();
        let _ = tick_until(&mut picker, |s| sorted(s) == want).expect(NEVER_CONVERGED);

        assert_eq!(shown_paths(&picker), want);
    }

    /// A swap starts the cursor over, and a fresh session starts at
    /// generation 0, so every open against a walked corpus used to re-inject
    /// the whole of it inside one tick on the render thread.
    #[test]
    fn a_swapped_corpus_is_handed_over_a_batch_at_a_time() {
        let (mut picker, index) = pending_picker();
        let files: Vec<String> = (0..INJECT_BATCH + 1).map(|i| format!("f{i}.rs")).collect();
        let total = files.len();
        index.publish(files, true);

        let _ = picker.tick();
        assert_eq!(
            picker.session.as_ref().unwrap().injected,
            INJECT_BATCH,
            "one batch, not the whole corpus"
        );
        assert_ne!(picker.cadence(), Cadence::IDLE, "and the loop comes back");

        let _ = picker.tick();
        assert_eq!(
            picker.session.as_ref().unwrap().injected,
            total,
            "the rest on the next tick"
        );
    }

    /// The other half of the same contract: a walk still filling only appends,
    /// and starting the matcher over for every batch would drop and re-rank
    /// the whole list several times a second while the tree is being read.
    #[test]
    fn a_walk_still_filling_keeps_what_the_picker_already_has() {
        let (mut picker, index) = pending_picker();
        walking(&index, &[A]);
        let _ = tick_until(&mut picker, |s| s.matches.len() == 1).expect(NEVER_CONVERGED);
        let generation = picker.session.as_ref().unwrap().generation;

        walking(&index, &[B]);
        let _ = tick_until(&mut picker, |s| s.matches.len() == 2).expect(NEVER_CONVERGED);

        assert_eq!(picker.session.as_ref().unwrap().generation, generation);
        assert_eq!(shown_paths(&picker), owned(&[A, B]));
    }

    /// The walk used to belong to the picker and die with it. It is shared
    /// now, so the session going away is the only thing left that can stop a
    /// walk of a whole repo nobody is watching any more.
    #[test]
    fn closing_the_picker_cancels_the_walk() {
        let (mut picker, index) = pending_picker();
        assert!(!index.cancelled());
        picker.close();
        assert!(index.cancelled());
    }

    /// A dead walker used to reach the picker as a disconnected channel. It
    /// reaches it as a flag on the corpus now, and without it a walker that
    /// fell over is indistinguishable from an empty directory.
    #[test]
    fn a_crashed_walk_closes_the_picker_and_says_so() {
        let (mut picker, index) = pending_picker();
        index.crash();

        let (dirty, flash) = picker.tick();
        assert_eq!(dirty, Dirty::YES, "{OWED}");
        assert_eq!(flash.unwrap(), WALKER_CRASHED_MSG);
        assert!(!picker.is_open());
    }

    /// `injected_items` only ever grows, so counting off the injector let a
    /// re-walk that came back with nothing keep the picker open on rows the
    /// matcher had already been told to forget.
    #[test]
    fn a_re_walk_that_finds_nothing_closes_the_picker() {
        let (mut picker, index) = pending_picker();
        walked(&index, &[A]);
        let _ = tick_until(&mut picker, |s| s.matches.len() == 1).expect(NEVER_CONVERGED);

        walked(&index, &[]);
        let (_, flash) = picker.tick();
        assert_eq!(flash.unwrap(), NOTHING_TO_PICK_MSG);
        assert!(!picker.is_open());
    }

    /// The picker ranks through nucleo and `maki.fs.fuzzy_files` ranks through
    /// the index, so only the shared config, haystack and order keep them
    /// agreeing. If they drift, the same query offers a different best match
    /// depending on who drew the list.
    #[test]
    fn the_picker_and_the_index_rank_a_query_the_same_way() {
        let files = [RANK_SCATTERED, RANK_MIDWORD, MAIN_FILE, README_PATH];
        let (mut picker, index) = pending_picker();
        walked(&index, &files);
        typed(&mut picker, MAIN_QUERY);
        let _ = tick_until(&mut picker, |s| {
            !s.matching && s.matches.len() == MAIN_MATCHES
        })
        .expect(NEVER_CONVERGED);

        let shown = shown_in_order(&picker);
        assert_eq!(shown.first().map(String::as_str), Some(MAIN_FILE));
        assert_eq!(ranked_by_index(&index, MAIN_QUERY, shown.len()), shown);
    }

    /// Equal scores are the common case on a short query, and the two sides
    /// used to break them differently: nucleo by the shorter haystack and then
    /// by insertion order, the index by walk order alone. On the NFD path they
    /// did not even agree on the score, since the index byte-indexed a
    /// haystack the picker held one entry per grapheme of.
    #[test_case(
        &[TIE_LONGER, TIE_FIRST, TIE_SECOND], TIE_QUERY, &[TIE_FIRST, TIE_SECOND, TIE_LONGER]
        ; "a_tie_broken_by_length_then_by_insertion"
    )]
    #[test_case(
        &[NFD_OTHER, NFD_PATH], NFD_QUERY, &[NFD_PATH, NFD_OTHER]
        ; "a_tie_with_one_path_in_nfd"
    )]
    fn the_picker_and_the_index_break_a_tie_the_same_way(
        files: &[&str],
        query: &str,
        want: &[&str],
    ) {
        let best = score(files[0], query);
        assert!(
            best.is_some() && files.iter().all(|p| score(p, query) == best),
            "{NOT_A_TIE}"
        );

        let (mut picker, index) = pending_picker();
        walked(&index, files);
        typed(&mut picker, query);
        let _ = tick_until(&mut picker, |s| {
            !s.matching && s.matches.len() == files.len()
        })
        .expect(NEVER_CONVERGED);

        let shown = shown_in_order(&picker);
        assert_eq!(shown, owned(want), "the order nucleo puts a tie in");
        assert_eq!(ranked_by_index(&index, query, shown.len()), shown);
    }

    /// A re-walk lands while the user is already arrowing down the list it
    /// replaces. The rows are re-ranked and re-numbered across that swap, so a
    /// selection kept by index is a different file, and Enter opens something
    /// the user never looked at.
    #[test]
    fn a_generation_swap_keeps_the_row_the_user_had_selected() {
        let (mut picker, index) = pending_picker();
        walked(&index, &[A, B, C]);
        let _ =
            tick_until(&mut picker, |s| in_order(s) == owned(&[A, B, C])).expect(NEVER_CONVERGED);
        picker.handle_key(key(KeyCode::Down));
        assert_eq!(selected_path(&picker), B, "the row the user arrowed to");

        walked(&index, &[C, A, B]);
        let _ = tick_until(&mut picker, |s| {
            in_order(s) == owned(&[C, A, B])
                && s.matches.get(s.selected).is_some_and(|m| m.path == B)
        })
        .expect(NEVER_CONVERGED);

        assert_eq!(shown_in_order(&picker), owned(&[C, A, B]), "a new list");
        assert_eq!(selected_path(&picker), B, "still the user's row");
    }

    /// Row 0 is a row too. It is the one the user reads first and the one
    /// Enter takes with no arrowing at all, and with an empty query it is
    /// whatever the fresh walk happened to reach first, so a swap landing
    /// between the read and the Enter used to put a path in the chat input
    /// that the user had never seen.
    #[test]
    fn a_generation_swap_keeps_row_zero_by_name() {
        let (mut picker, index) = pending_picker();
        walked(&index, &[A, B, C]);
        let _ =
            tick_until(&mut picker, |s| in_order(s) == owned(&[A, B, C])).expect(NEVER_CONVERGED);
        assert_eq!(selected_path(&picker), A, "the row the user is reading");

        walked(&index, &[C, B, A]);
        let _ = tick_until(&mut picker, |s| {
            in_order(s) == owned(&[C, B, A])
                && s.matches.get(s.selected).is_some_and(|m| m.path == A)
        })
        .expect(NEVER_CONVERGED);

        assert_eq!(selected_path(&picker), A, "still the path that was on it");
        assert_eq!(
            picker.session.as_ref().unwrap().selected,
            2,
            "found again at the row the new list puts it on"
        );
    }

    /// The corpus is handed to the matcher a batch a tick, so a walk that has
    /// landed whole is still many ticks away from being searchable on a big
    /// tree. A title that says the scan is over is the only thing telling the
    /// user that "No matches" is final, and for those ticks it was wrong.
    #[test]
    fn the_title_keeps_scanning_until_every_path_has_been_handed_over() {
        let (mut picker, index) = pending_picker();
        let files: Vec<String> = (0..INJECT_BATCH + 1).map(|i| format!("f{i}.rs")).collect();
        index.publish(files, true);

        let _ = picker.tick();
        assert_eq!(
            picker.session.as_ref().unwrap().walk,
            Walk::Running,
            "a batch of a finished walk is still to come"
        );

        let _ = picker.tick();
        assert_eq!(
            picker.session.as_ref().unwrap().walk,
            Walk::Listed,
            "and the tick that hands over the last of it settles the title"
        );
    }

    /// A walk stopped at `MAX_ENTRIES` is over like any other, and its list
    /// used to be drawn exactly like the whole tree, with the missing files
    /// different on every walk.
    #[test_case(true  => (Walk::Capped, true)  ; "capped_walk_says_so")]
    #[test_case(false => (Walk::Listed, false) ; "complete_walk_does_not")]
    fn a_capped_walk_says_the_list_is_short(capped: bool) -> (Walk, bool) {
        let (mut picker, index) = pending_picker();
        walked(&index, &[A, B]);
        if capped {
            index.cap();
        }
        let _ = tick_until(&mut picker, |s| s.visible && s.walk != Walk::Running)
            .expect(NEVER_CONVERGED);

        let mut terminal =
            Terminal::new(TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT)).unwrap();
        terminal
            .draw(|frame| {
                picker.view(frame, frame.area());
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        (
            picker.session.as_ref().unwrap().walk,
            screen.contains(TITLE_CAPPED.trim()),
        )
    }

    /// Opening the picker closes it first, so `Ctrl+S` on an open picker is a
    /// cancel and an ask on the same walk a moment apart. What the user ends
    /// up looking at has to be the whole tree, not the prefix the cancelled
    /// walk had reached when it was stopped.
    #[test]
    fn reopening_the_picker_still_lists_the_whole_tree() {
        let tmp = TempDir::new().unwrap();
        for i in 0..REOPENED_FILES {
            std::fs::write(tmp.path().join(format!("file_{i:03}.rs")), "").unwrap();
        }

        let mut picker = FilePickerModal::new();
        let cwd = tmp.path().to_string_lossy().into_owned();
        picker.open(&cwd);
        picker.open(&cwd);

        let _ = tick_until(&mut picker, |s| {
            s.walk == Walk::Listed && s.matches.len() == REOPENED_FILES
        })
        .expect(NEVER_CONVERGED);
    }

    #[test]
    fn esc_returns_close() {
        let (mut picker, _index) = pending_picker();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Esc)),
            FilePickerModalAction::Close
        ));
    }

    #[test]
    fn typing_during_pending_buffers_query() {
        let (mut picker, _index) = pending_picker();
        picker.handle_key(key(KeyCode::Char('m')));
        picker.handle_key(key(KeyCode::Char('a')));
        assert_eq!(picker.session.as_ref().unwrap().search.value(), "ma");
    }

    #[test]
    fn enter_during_pending_is_consumed() {
        let (mut picker, _index) = pending_picker();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            FilePickerModalAction::Consumed
        ));
    }

    #[test]
    fn matches_capped_at_max_materialized() {
        let mut picker = picker_with_matches(MAX_MATERIALIZED as usize + 50);
        let s = picker.session.as_mut().unwrap();
        s.total_matches = MAX_MATERIALIZED + 50;
        s.matches.truncate(MAX_MATERIALIZED as usize);
        assert_eq!(s.total_matches, MAX_MATERIALIZED + 50);
        assert_eq!(s.matches.len(), MAX_MATERIALIZED as usize);
    }

    fn picker_with_matches(n: usize) -> FilePickerModal {
        let (mut picker, _index) = pending_picker();
        let s = picker.session.as_mut().unwrap();
        s.walk = Walk::Listed;
        s.visible = true;
        s.matches = (0..n)
            .map(|i| Match {
                path: format!("file_{i:03}.rs"),
                indices: Vec::new(),
            })
            .collect();
        s.total_matches = n as u32;
        picker
    }

    #[test]
    fn resize_clamps_scroll_offset() {
        let mut picker = picker_with_matches(20);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 5;
        s.selected = 19;
        s.scroll_offset = 15;
        ensure_visible(s);
        assert_eq!(s.scroll_offset, 15);

        s.viewport_height = 20;
        ensure_visible(s);
        assert_eq!(s.scroll_offset, 0);
    }

    #[test_case(&[], 3 ; "empty_indices")]
    #[test_case(&[0, 2], 5 ; "sparse_match")]
    fn build_highlighted_line_no_panic(indices: &[u32], max_width: usize) {
        let t = theme::current();
        let _ = build_highlighted_line("hello", indices, max_width, false, &t);
    }

    #[test]
    fn build_highlighted_line_truncates_at_max_width() {
        let t = theme::current();
        let line = build_highlighted_line("verylongfilename.rs", &[], 5, false, &t);
        let text: String = line
            .spans
            .iter()
            .skip(1)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "veryl");
    }

    #[test]
    fn build_highlighted_line_unicode_width() {
        let t = theme::current();
        let line = build_highlighted_line("日本語.rs", &[], 6, false, &t);
        let text: String = line
            .spans
            .iter()
            .skip(1)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "日本語");
    }

    /// The indices name haystack units, and a unit of a path that is not
    /// ASCII is a grapheme. Stepping per char would shift the highlight one
    /// place right of every combining mark before it and cut the cluster the
    /// mark belongs to in half, so the acute would be painted apart from its
    /// `e`.
    #[test]
    fn build_highlighted_line_highlights_whole_graphemes() {
        let t = theme::current();
        let line = build_highlighted_line(NFD_PATH, &NFD_MATCHED_UNITS, NFD_PATH.len(), false, &t);
        let spans: Vec<&str> = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(spans, NFD_HIGHLIGHT_SPANS);
    }

    #[test_case(0, -10, 0 ; "clamps_at_start")]
    #[test_case(4, 10, 4 ; "clamps_at_end")]
    #[test_case(2, 1, 3 ; "moves_down")]
    #[test_case(2, -1, 1 ; "moves_up")]
    fn move_selection_behavior(start: usize, delta: isize, expected: usize) {
        let mut picker = picker_with_matches(5);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 10;
        s.selected = start;
        move_selection(s, delta);
        assert_eq!(s.selected, expected);
    }

    #[test]
    fn move_selection_empty_is_noop() {
        let mut picker = picker_with_matches(0);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 10;
        move_selection(s, 5);
        assert_eq!(s.selected, 0);
    }

    #[test_case(0, -3, 3 ; "negative_scrolls_down")]
    #[test_case(5, 2, 3 ; "positive_scrolls_up")]
    fn scroll_updates_selection(start: usize, delta: i32, expected: usize) {
        let mut picker = picker_with_matches(10);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 5;
        s.selected = start;
        picker.scroll(delta);
        assert_eq!(picker.session.as_ref().unwrap().selected, expected);
    }

    #[test]
    fn handle_paste_appends_to_search() {
        let (mut picker, _index) = pending_picker();
        picker.handle_key(key(KeyCode::Char('a')));
        assert!(picker.handle_paste("bc"));
        assert_eq!(picker.session.as_ref().unwrap().search.value(), "abc");
    }

    #[test]
    fn handle_paste_returns_false_when_closed() {
        let mut picker = FilePickerModal::new();
        assert!(!picker.handle_paste("test"));
    }

    #[test]
    fn enter_with_selection_returns_path() {
        let mut picker = picker_with_matches(3);
        picker.session.as_mut().unwrap().selected = 1;
        match picker.handle_key(key(KeyCode::Enter)) {
            FilePickerModalAction::Select(path) => assert_eq!(path, "file_001.rs"),
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn enter_with_no_matches_returns_close() {
        let mut picker = picker_with_matches(0);
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            FilePickerModalAction::Close
        ));
    }

    #[test]
    fn backspace_clears_search_and_reparses() {
        let (mut picker, _index) = pending_picker();
        picker.handle_key(key(KeyCode::Char('a')));
        picker.handle_key(key(KeyCode::Char('b')));
        picker.handle_key(key(KeyCode::Backspace));
        assert_eq!(picker.session.as_ref().unwrap().search.value(), "a");
    }

    #[test_case(10, 0, 6 ; "scrolls_down_when_below")]
    #[test_case(2, 10, 2 ; "scrolls_up_when_above")]
    fn ensure_visible_adjusts_scroll(
        selected: usize,
        initial_scroll: usize,
        expected_scroll: usize,
    ) {
        let mut picker = picker_with_matches(20);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 5;
        s.selected = selected;
        s.scroll_offset = initial_scroll;
        ensure_visible(s);
        assert_eq!(s.scroll_offset, expected_scroll);
    }

    #[test]
    fn ensure_visible_zero_viewport_no_panic() {
        let mut picker = picker_with_matches(5);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 0;
        s.selected = 3;
        ensure_visible(s);
    }

    #[test]
    fn clamp_selection_reduces_when_matches_shrink() {
        let mut picker = picker_with_matches(10);
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 5;
        s.selected = 9;
        s.matches.truncate(3);
        clamp_selection(s);
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn contains_returns_false_when_not_visible() {
        let (picker, _index) = pending_picker();
        assert!(!picker.contains(Position::new(0, 0)));
    }

    /// This picker highlights the units nucleo reports, and
    /// `maki.fs.fuzzy_files` hands a plugin byte ranges for the same job. On
    /// an ASCII path, where a unit is a byte, a run of units is the range
    /// that covers it.
    #[test]
    fn a_plugin_gets_the_characters_this_picker_highlights() {
        let (mut picker, index) = pending_picker();
        walked(&index, &[MAIN_PATH, README_PATH]);
        let _ = tick_until(&mut picker, |s| s.matches.len() == 2).expect(NEVER_CONVERGED);

        typed(&mut picker, MAIN_QUERY);
        let _ = tick_until(&mut picker, |s| !s.matching && s.matches.len() == 1)
            .expect(NEVER_CONVERGED);

        let shown = &picker.session.as_ref().unwrap().matches[0];
        let ranked = highlights_from_index(&index, MAIN_QUERY);
        let item = &ranked.items[0];

        assert_eq!(item.path, shown.path, "the same row");
        assert!(!item.highlights.is_empty(), "the query did match it");
        assert_eq!(
            painted(&item.path, &item.highlights),
            marked_units(shown),
            "the same characters marked"
        );
    }

    /// On a path that is not ASCII a unit is a grapheme rather than a byte, so
    /// the ranges are different numbers for the same highlight and have to
    /// cover whole clusters. Reading them off the wrong indexing cuts the
    /// combining mark off its `e`.
    #[test]
    fn a_plugin_gets_byte_ranges_for_the_graphemes_this_picker_highlights() {
        let (mut picker, index) = pending_picker();
        walked(&index, &[NFD_PATH]);
        typed(&mut picker, NFD_QUERY);
        let _ = tick_until(&mut picker, |s| !s.matching && s.matches.len() == 1)
            .expect(NEVER_CONVERGED);

        let shown = &picker.session.as_ref().unwrap().matches[0];
        let ranked = highlights_from_index(&index, NFD_QUERY);
        let item = &ranked.items[0];

        assert_eq!(item.path, shown.path, "the same row");
        assert_eq!(item.highlights, NFD_HIGHLIGHT_RANGE, "the whole cluster");
        assert_eq!(
            painted(&item.path, &item.highlights),
            marked_units(shown),
            "the same graphemes marked"
        );
    }

    /// The text the picker paints as matched: the haystack units it marked,
    /// spelled back out of the path they name.
    fn marked_units(shown: &Match) -> String {
        shown
            .path
            .graphemes(true)
            .enumerate()
            .filter(|(i, _)| shown.indices.binary_search(&(*i as u32)).is_ok())
            .map(|(_, unit)| unit)
            .collect()
    }

    /// The same text through the ranges, sliced the way Lua's `sub` slices.
    fn painted(path: &str, highlights: &[(u32, u32)]) -> String {
        highlights
            .iter()
            .map(|(from, to)| &path[*from as usize - 1..*to as usize])
            .collect()
    }

    fn highlights_from_index(index: &FileIndex, query: &str) -> Ranked {
        index
            .query(
                &FileQuery {
                    query,
                    limit: 1,
                    highlights: true,
                },
                &NEVER,
            )
            .expect(QUERY_ANSWERED)
    }
}
