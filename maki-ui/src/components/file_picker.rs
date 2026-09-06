use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo, Utf32String};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tracing::warn;
use unicode_width::UnicodeWidthChar;

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

/// The walker answers once, and its answer only matters when the list came up
/// empty: an empty directory, a fully ignored one and one we could not open
/// look identical from the injector's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Walk {
    Running,
    Listed,
    Unreadable,
}

impl Walk {
    /// What to tell the user when the walk is over and the list is still empty.
    /// Total over the state, so no ending can be forgotten.
    fn nothing_found_msg(self) -> Option<&'static str> {
        match self {
            Self::Running => None,
            Self::Listed => Some(NOTHING_TO_PICK_MSG),
            Self::Unreadable => Some(UNREADABLE_DIR_MSG),
        }
    }
}

pub enum FilePickerModalAction {
    Consumed,
    Select(String),
    Close,
}

struct Match {
    path: String,
    indices: Vec<u32>,
}

struct Session {
    nucleo: Nucleo<()>,
    matcher: Matcher,
    matches: Vec<Match>,
    total_matches: u32,

    search: TextBuffer,
    selected: usize,
    scroll_offset: usize,
    viewport_height: usize,
    inner_area: Rect,

    cancel: Arc<AtomicBool>,
    done_rx: flume::Receiver<Walk>,
    started_at: Instant,

    walk: Walk,
    /// The matcher owes an answer. Nothing delivers it, so `tick` has to look.
    matching: bool,
    visible: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
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

        let notify = Arc::new(|| {});
        let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), notify, None, 1);
        let injector = nucleo.injector();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();
        let (done_tx, done_rx) = flume::bounded(1);

        let root = PathBuf::from(cwd);
        if let Err(e) = thread::Builder::new()
            .name("file-walker".into())
            .spawn(move || {
                let overrides = OverrideBuilder::new(&root)
                    .add("!.git")
                    .unwrap()
                    .build()
                    .unwrap();
                WalkBuilder::new(&root)
                    .hidden(false)
                    // Depth 0 is the root, which strips to an empty name: a
                    // bare separator at the top of every list, selected by
                    // default.
                    .min_depth(Some(1))
                    .overrides(overrides)
                    .build_parallel()
                    .run(|| {
                        let injector = injector.clone();
                        let cancel = cancel.clone();
                        let root = root.clone();
                        Box::new(move |entry| {
                            if cancel.load(Ordering::Relaxed) {
                                return ignore::WalkState::Quit;
                            }
                            let Ok(entry) = entry else {
                                return ignore::WalkState::Continue;
                            };
                            if !entry
                                .file_type()
                                .is_some_and(|ft| ft.is_file() || ft.is_dir() || ft.is_symlink())
                            {
                                return ignore::WalkState::Continue;
                            }
                            let path = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                            let mut name = path.to_string_lossy().into_owned();
                            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                                name.push(std::path::MAIN_SEPARATOR);
                            }
                            injector.push((), |_, cols| {
                                cols[0] = Utf32String::from(name.as_str());
                            });
                            ignore::WalkState::Continue
                        })
                    });
                let _ = done_tx.send(walk_end(&root));
            })
        {
            warn!("{WALKER_CRASHED_MSG}: failed to spawn thread: {e}");
            return;
        }

        self.session = Some(Session {
            nucleo,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            matches: Vec::new(),
            total_matches: 0,
            search: TextBuffer::new(String::new()),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            inner_area: Rect::default(),
            cancel: cancel_clone,
            done_rx,
            started_at: Instant::now(),
            walk: Walk::Running,
            matching: false,
            visible: false,
        });
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
        ])
    }

    /// Returns the frame owed plus a message to flash if the picker gave up.
    pub fn tick(&mut self) -> (Dirty, Option<String>) {
        let Some(s) = self.session.as_mut() else {
            return (Dirty::NO, None);
        };

        let status = s.nucleo.tick(0);
        s.matching = status.running;
        // The title says "scanning…" while walking, so finishing redraws too.
        let mut dirty = Dirty::from(status.changed);

        if s.walk == Walk::Running {
            match s.done_rx.try_recv() {
                Ok(end) => {
                    s.walk = end;
                    dirty = Dirty::YES;
                }
                Err(flume::TryRecvError::Disconnected) => {
                    warn!("{WALKER_CRASHED_MSG}: walker thread panicked");
                    self.session = None;
                    return (Dirty::YES, Some(WALKER_CRASHED_MSG.into()));
                }
                Err(flume::TryRecvError::Empty) => {}
            }
        }

        let has_files = s.nucleo.injector().injected_items() > 0;

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
        let title = if s.walk == Walk::Running {
            TITLE_WALKING
        } else {
            TITLE
        };

        let has_query_without_matches = s.matches.is_empty() && !s.search.value().is_empty();
        let max_visible = area.height.saturating_sub(SEARCH_ROW + 2);
        let content_rows = if has_query_without_matches {
            1
        } else {
            match_count.min(max_visible)
        };

        let modal = Modal {
            title,
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
/// pick" or "I could not even look". Asking happens here, on the walker thread,
/// where a slow filesystem cannot stall the UI.
fn walk_end(root: &Path) -> Walk {
    match root.read_dir() {
        Ok(_) => Walk::Listed,
        Err(e) => {
            warn!("{UNREADABLE_DIR_MSG}: {}: {e}", root.display());
            Walk::Unreadable
        }
    }
}

fn reparse_pattern(s: &mut Session) {
    let query = s.search.value();
    s.nucleo
        .pattern
        .reparse(0, &query, CaseMatching::Smart, Normalization::Smart, false);
    s.selected = 0;
    s.scroll_offset = 0;
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
        let col = &item.matcher_columns[0];
        let path = col.to_string();

        let indices = if has_pattern {
            indices_buf.clear();
            pattern
                .column_pattern(0)
                .indices(col.slice(..), &mut s.matcher, &mut indices_buf);
            mem::take(&mut indices_buf)
        } else {
            Vec::new()
        };

        s.matches.push(Match { path, indices });
    }
}

fn move_selection(s: &mut Session, delta: isize) {
    if s.matches.is_empty() {
        return;
    }
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
    let base = if selected { t.item_selected } else { t.item };
    let highlight = base
        .fg(t.accent.fg.unwrap_or_default())
        .add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(LABEL_INDENT, base)];
    let mut in_match = false;
    let mut run = String::new();
    let mut width = 0usize;

    for (i, ch) in text.chars().enumerate() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > max_width {
            break;
        }
        width += cw;

        let is_match = indices.binary_search(&(i as u32)).is_ok();
        if is_match != in_match && !run.is_empty() {
            spans.push(Span::styled(
                mem::take(&mut run),
                if in_match { highlight } else { base },
            ));
        }
        in_match = is_match;
        run.push(ch);
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

    const MAIN_PATH: &str = "src/main.rs";
    const README_PATH: &str = "docs/readme.md";
    const README_QUERY: &str = "readme";
    const MISSING_DIR: &str = "gone";
    const MAIN_FILE: &str = "main.rs";

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

    fn pending_picker() -> (FilePickerModal, flume::Sender<Walk>) {
        let mut picker = FilePickerModal::new();
        let notify = Arc::new(|| {});
        let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), notify, None, 1);
        let (done_tx, done_rx) = flume::bounded(1);
        picker.session = Some(Session {
            nucleo,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            matches: Vec::new(),
            total_matches: 0,
            search: TextBuffer::new(String::new()),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            inner_area: Rect::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            done_rx,
            started_at: Instant::now(),
            walk: Walk::Running,
            matching: false,
            visible: false,
        });
        (picker, done_tx)
    }

    fn inject_file(picker: &FilePickerModal, path: &str) {
        let s = picker.session.as_ref().unwrap();
        s.nucleo.injector().push((), |_, cols| {
            cols[0] = Utf32String::from(path);
        });
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
        let (mut picker, _done_tx) = pending_picker();
        for path in files {
            inject_file(&picker, path);
        }
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

    /// Neither ending leaves anything to pick, so the picker closes itself and
    /// says why: one frame, one flash, and then quiet, or the loop never
    /// settles again. A walk slow enough to cross the debounce is already on
    /// screen when it comes back empty, and it still has to close, or the user
    /// is left staring at an empty list with no reason for it.
    #[test_case(Some(Walk::Listed), false => NOTHING_TO_PICK_MSG ; "walk_finished_with_nothing")]
    #[test_case(Some(Walk::Listed), true  => NOTHING_TO_PICK_MSG ; "shown_walk_finished_with_nothing")]
    #[test_case(None,               false => WALKER_CRASHED_MSG  ; "walker_died")]
    fn self_close_flashes_once_then_stays_quiet(end: Option<Walk>, on_screen: bool) -> String {
        let (mut picker, done_tx) = pending_picker();
        picker.session.as_mut().unwrap().visible = on_screen;
        match end {
            Some(end) => done_tx.send(end).unwrap(),
            None => drop(done_tx),
        }

        let (dirty, flash) = picker.tick();
        assert!(picker.session.is_none());
        assert_eq!(dirty, Dirty::YES, "{OWED}");
        assert_eq!(picker.tick(), (Dirty::NO, None), "{QUIET}");
        flash.unwrap()
    }

    /// Both endings inject nothing, and the flash is the only trace the user
    /// gets, so this is the difference between "there is nothing here" and "I
    /// could not look".
    #[test]
    fn walk_end_tells_an_empty_directory_from_an_unopenable_one() {
        let tmp = TempDir::new().unwrap();

        assert_eq!(walk_end(tmp.path()), Walk::Listed);
        assert_eq!(walk_end(&tmp.path().join(MISSING_DIR)), Walk::Unreadable);
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
        let (mut picker, _done_tx) = pending_picker();
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
        let (mut picker, done_tx) = pending_picker();
        inject_file(&picker, MAIN_PATH);
        done_tx.send(Walk::Listed).unwrap();

        // Waiting for a tick to owe nothing is not the same as settling: the
        // matcher answers on its own thread and can still be running on a tick
        // that changed nothing.
        let _ = tick_until(&mut picker, |s| !s.matching && s.matches.len() == 1)
            .expect(NEVER_CONVERGED);

        // An idle tick can still owe a poll while the worker drains the last
        // answer, so "settled" has to pin down both sides of the cadence.
        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        while picker.cadence() != Cadence::IDLE {
            assert!(Instant::now() < deadline, "the picker never stopped");
            std::thread::yield_now();
        }

        assert_eq!(picker.tick(), (Dirty::NO, None), "{QUIET}");
        assert_eq!(picker.cadence(), Cadence::IDLE);
    }

    /// The matcher answers on a worker thread, long after the keypress was
    /// handled, so typing is only redrawn because a later `tick` reports the
    /// change. Without that the list freezes on the previous query.
    #[test]
    fn query_change_owes_a_frame_from_a_later_tick() {
        let (mut picker, done_tx) = pending_picker();
        inject_file(&picker, MAIN_PATH);
        inject_file(&picker, README_PATH);
        done_tx.send(Walk::Listed).unwrap();
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
        let (mut picker, _done_tx) = pending_picker();
        let s = picker.session.as_mut().unwrap();
        s.visible = true;
        s.matching = matching;
        s.walk = walk;

        picker.cadence()
    }

    #[test]
    fn esc_returns_close() {
        let (mut picker, _done_tx) = pending_picker();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Esc)),
            FilePickerModalAction::Close
        ));
    }

    #[test]
    fn typing_during_pending_buffers_query() {
        let (mut picker, _done_tx) = pending_picker();
        picker.handle_key(key(KeyCode::Char('m')));
        picker.handle_key(key(KeyCode::Char('a')));
        assert_eq!(picker.session.as_ref().unwrap().search.value(), "ma");
    }

    #[test]
    fn enter_during_pending_is_consumed() {
        let (mut picker, _done_tx) = pending_picker();
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
        let (mut picker, _done_tx) = pending_picker();
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
        let (mut picker, _done_tx) = pending_picker();
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
        let (mut picker, _done_tx) = pending_picker();
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
        let (picker, _done_tx) = pending_picker();
        assert!(!picker.contains(Position::new(0, 0)));
    }
}
