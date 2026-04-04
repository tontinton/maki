use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use crate::animation::spinner_frame;
use crate::components::Overlay;
use crate::components::keybindings::key;
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::text_buffer::TextBuffer;
use crate::theme;

use tracing::warn;

use crossterm::event::{KeyCode, KeyEvent};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo, Utf32String};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

const TITLE: &str = " Files ";
// Memory/perf cap: materializing matches builds a Vec of styled strings for
// rendering. 640 keeps the allocation bounded while being far more than the
// viewport can display, so scrolling stays smooth without unbounded growth.
// Plus, as everybody knows, 640 "ought to be enough for anybody" ;)
const MAX_MATERIALIZED: u32 = 640;
const TITLE_WALKING: &str = " Files (scanning…) ";
const WIDTH_PERCENT: u16 = 60;
const MAX_HEIGHT_PERCENT: u16 = 80;
const SEARCH_ROW: u16 = 1;
const NO_MATCHES: &str = "  No matches";
const LABEL_INDENT: &str = "  ";
const EMPTY_DIR_MSG: &str = "Current directory is empty";
const PENDING_DEBOUNCE_MS: u128 = 100;
const WALKER_CRASHED_MSG: &str = "File scanner crashed";

pub enum FilePickerModalAction {
    Consumed,
    Select(String),
    Close,
}

struct FilePickerModalMatch {
    path: String,
    indices: Vec<u32>,
}

/// The active walker + matcher session. Created on open(), dropped on close().
/// `visible` tracks the pending→open transition: false while waiting for the
/// first file or the debounce timeout, true once the modal should be rendered.
struct FileWalkerSession {
    nucleo: Nucleo<()>,
    matcher: Matcher,
    matches: Vec<FilePickerModalMatch>,
    total_matches: u32,
    cancel: Arc<AtomicBool>,
    done_rx: flume::Receiver<()>,
    started_at: Instant,
    walking: bool,
    matching: bool,
    visible: bool,
}

impl Drop for FileWalkerSession {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl FileWalkerSession {
    fn reparse_pattern(&mut self, query: &str) {
        self.nucleo
            .pattern
            .reparse(0, query, CaseMatching::Smart, Normalization::Smart, false);
    }

    fn refresh_matches(&mut self) {
        let snapshot = self.nucleo.snapshot();
        self.total_matches = snapshot.matched_item_count();
        let count = self.total_matches.min(MAX_MATERIALIZED);

        self.matches.clear();

        let pattern = snapshot.pattern();
        let has_pattern = !pattern.column_pattern(0).atoms.is_empty();
        let mut indices_buf = Vec::new();

        for item in snapshot.matched_items(0..count) {
            let col = &item.matcher_columns[0];
            let path = col.to_string();

            let indices = if has_pattern {
                indices_buf.clear();
                pattern.column_pattern(0).indices(
                    col.slice(..),
                    &mut self.matcher,
                    &mut indices_buf,
                );
                mem::take(&mut indices_buf)
            } else {
                Vec::new()
            };

            self.matches.push(FilePickerModalMatch { path, indices });
        }
    }
}

pub struct FilePickerModal {
    search: TextBuffer,
    selected: usize,
    scroll_offset: usize,
    viewport_height: usize,
    inner_area: Rect,

    session: Option<FileWalkerSession>,
    flash: Option<String>,
}

impl FilePickerModal {
    pub fn new() -> Self {
        Self {
            search: TextBuffer::new(String::new()),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            inner_area: Rect::default(),

            session: None,
            flash: None,
        }
    }

    pub fn open(&mut self, cwd: &str) {
        self.close();

        // NOTE: tick() is called continuously while is_loading() is true
        // (which tracks both walking and matching), so we don't need nucleo
        // to wake the event loop.
        let notify = Arc::new(|| {});
        let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), notify, None, 1);
        let injector = nucleo.injector();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();

        // oneshot: walker sends () on completion
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
                    .hidden(false) // include dotfiles
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
                                .is_some_and(|ft| ft.is_file() || ft.is_symlink())
                            {
                                return ignore::WalkState::Continue;
                            }
                            let path = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                            let path_str = path.to_string_lossy();
                            injector.push((), |_, cols| {
                                cols[0] = Utf32String::from(path_str.as_ref());
                            });
                            ignore::WalkState::Continue
                        })
                    });
                let _ = done_tx.send(());
            })
        {
            warn!("{}: failed to spawn thread: {e}", WALKER_CRASHED_MSG);
            self.flash = Some(WALKER_CRASHED_MSG.into());
            return;
        }

        self.session = Some(FileWalkerSession {
            nucleo,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            matches: Vec::new(),
            total_matches: 0,
            cancel: cancel_clone,
            done_rx,
            started_at: Instant::now(),
            walking: true,
            matching: false,
            visible: false,
        });
    }

    pub fn close(&mut self) {
        self.session = None; // Drop cancels the walker
        self.search.clear();
        self.selected = 0;
        self.scroll_offset = 0;
    }

    /// Returns true while a session exists, including during the Pending state
    /// when the modal is not yet visible. This lets us intercept keystrokes
    /// before the modal is rendered. Use `is_visible()` when the rendered
    /// on-screen distinction matters.
    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    pub fn is_visible(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.visible)
    }

    pub fn is_loading(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.walking || s.matching)
    }

    pub fn take_flash(&mut self) -> Option<String> {
        self.flash.take()
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.is_visible() && self.inner_area.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        if delta > 0 {
            self.move_up_by(delta as usize);
        } else {
            self.move_down_by(delta.unsigned_abs() as usize);
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        if self.session.is_none() {
            return false;
        }
        self.search.insert_text(text);
        self.reparse_pattern();
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FilePickerModalAction {
        let visible = self.is_visible();

        match key.code {
            KeyCode::Esc => FilePickerModalAction::Close,
            KeyCode::Enter => {
                if !visible {
                    return FilePickerModalAction::Consumed;
                }
                if let Some(s) = &self.session
                    && let Some(m) = s.matches.get(self.selected)
                {
                    return FilePickerModalAction::Select(m.path.clone());
                }
                FilePickerModalAction::Close
            }
            KeyCode::Up => {
                self.move_up_by(1);
                FilePickerModalAction::Consumed
            }
            KeyCode::Down => {
                self.move_down_by(1);
                FilePickerModalAction::Consumed
            }
            KeyCode::Backspace => {
                self.search.remove_char();
                self.reparse_pattern();
                FilePickerModalAction::Consumed
            }
            KeyCode::Left => {
                self.search.move_left();
                FilePickerModalAction::Consumed
            }
            KeyCode::Right => {
                self.search.move_right();
                FilePickerModalAction::Consumed
            }
            KeyCode::Home => {
                self.search.move_home();
                FilePickerModalAction::Consumed
            }
            KeyCode::End => {
                self.search.move_end();
                FilePickerModalAction::Consumed
            }
            _ if key::DELETE_WORD.matches(key) => {
                self.search.remove_word_before_cursor();
                self.reparse_pattern();
                FilePickerModalAction::Consumed
            }
            _ if key::SCROLL_HALF_UP.matches(key) => {
                self.move_up_by((self.viewport_height / 2).max(1));
                FilePickerModalAction::Consumed
            }
            _ if key::SCROLL_HALF_DOWN.matches(key) => {
                self.move_down_by((self.viewport_height / 2).max(1));
                FilePickerModalAction::Consumed
            }
            _ if key::SCROLL_LINE_UP.matches(key) => {
                self.move_up_by(1);
                FilePickerModalAction::Consumed
            }
            _ if key::SCROLL_LINE_DOWN.matches(key) => {
                self.move_down_by(1);
                FilePickerModalAction::Consumed
            }
            _ if super::is_ctrl(&key) => FilePickerModalAction::Consumed,
            KeyCode::Char(c) => {
                self.search.push_char(c);
                self.reparse_pattern();
                FilePickerModalAction::Consumed
            }
            _ => FilePickerModalAction::Consumed,
        }
    }

    fn reparse_pattern(&mut self) {
        let query = self.search.value();
        if let Some(session) = &mut self.session {
            session.reparse_pattern(&query);
        }
        self.selected = 0;
        self.scroll_offset = 0;
    }

    fn matches(&self) -> &[FilePickerModalMatch] {
        self.session.as_ref().map_or(&[], |s| s.matches.as_slice())
    }

    fn move_up_by(&mut self, n: usize) {
        if self.matches().is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(n);
        self.ensure_visible();
    }

    fn move_down_by(&mut self, n: usize) {
        if self.matches().is_empty() {
            return;
        }
        self.selected = (self.selected + n).min(self.matches().len() - 1);
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        // clamp scroll so content fills the viewport to prevent gap after resizing
        let len = self.matches().len();
        if len > self.viewport_height {
            self.scroll_offset = self.scroll_offset.min(len - self.viewport_height);
        } else {
            self.scroll_offset = 0;
        }

        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + self.viewport_height {
            self.scroll_offset = self.selected + 1 - self.viewport_height;
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.matches().len();
        if len == 0 {
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected = self.selected.min(len - 1);
            self.ensure_visible();
        }
    }

    pub fn tick(&mut self) {
        let Some(session) = &mut self.session else {
            return;
        };

        // tick() is called from the render path so forcing nucleo to not block
        let status = session.nucleo.tick(0);
        session.matching = status.running;

        // walking: check if walker finished (or panicked)
        if session.walking {
            match session.done_rx.try_recv() {
                Ok(()) => {
                    session.walking = false;
                }
                Err(flume::TryRecvError::Disconnected) => {
                    warn!("{}: walker thread panicked", WALKER_CRASHED_MSG);
                    self.flash = Some(WALKER_CRASHED_MSG.into());
                    self.close();
                    return;
                }
                Err(flume::TryRecvError::Empty) => {}
            }
        }

        // NOTE: to avoid screen flicker with an empty modal we stay invisible
        // until files arrive or the debounce expires, so the user never sees a
        // blank picker flash on screen.
        if !session.visible {
            let has_files = session.nucleo.injector().injected_items() > 0;
            let debounce_elapsed = session.started_at.elapsed().as_millis() >= PENDING_DEBOUNCE_MS;

            if has_files || (session.walking && debounce_elapsed) {
                session.visible = true;
            } else if !session.walking {
                self.flash = Some(EMPTY_DIR_MSG.into());
                self.close();
                return;
            } else {
                return;
            }
        }

        if !status.changed {
            return;
        }

        session.refresh_matches();
        self.clamp_selection();
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let session = match &self.session {
            Some(s) if s.visible => s,
            _ => return Rect::default(),
        };

        let match_count = session.matches.len() as u16; // bounded by MAX_MATERIALIZED
        let walking = session.walking;
        let started_at = session.started_at;
        let title = if walking { TITLE_WALKING } else { TITLE };

        let has_query_without_matches =
            session.matches.is_empty() && !self.search.value().is_empty();
        // cap so the modal never grows taller than the screen can show
        // Modal::render will further cap at MAX_HEIGHT_PERCENT.
        let max_visible = area.height.saturating_sub(SEARCH_ROW + 2); // 2 = border chrome
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
        self.inner_area = inner;

        let viewport_h = inner.height.saturating_sub(SEARCH_ROW) as usize;
        self.viewport_height = viewport_h;
        self.ensure_visible();

        let [list_area, search_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

        self.render_list(frame, list_area, viewport_h);
        self.render_search(frame, search_area, walking, started_at);

        if match_count > viewport_h as u16 {
            render_vertical_scrollbar(frame, list_area, match_count, self.scroll_offset as u16);
        }

        popup
    }

    fn render_list(&self, frame: &mut Frame, area: Rect, viewport_height: usize) {
        let t = theme::current();
        let matches = self.matches();

        if matches.is_empty() {
            if !self.search.value().is_empty() {
                let line = Line::from(Span::styled(NO_MATCHES, t.cmd_desc));
                frame.render_widget(Paragraph::new(vec![line]), area);
            }
            return;
        }

        let more_results_available = self
            .session
            .as_ref()
            .is_some_and(|s| s.total_matches > MAX_MATERIALIZED);

        // when at the bottom of a truncated list, reserve the last row for the hint
        let at_bottom = self.scroll_offset + viewport_height >= matches.len();
        let hint_row = if more_results_available && at_bottom {
            1
        } else {
            0
        };
        let visible_rows = viewport_height - hint_row;

        let max_label_width = area.width.saturating_sub(LABEL_INDENT.len() as u16) as usize;
        let mut lines: Vec<Line> = Vec::new();
        let end = (self.scroll_offset + visible_rows).min(matches.len());

        for (i, m) in matches[self.scroll_offset..end].iter().enumerate() {
            let is_selected = self.scroll_offset + i == self.selected;
            let line =
                build_highlighted_line(&m.path, &m.indices, max_label_width, is_selected, &t);
            lines.push(line);
        }

        if hint_row > 0 {
            let total = self.session.as_ref().unwrap().total_matches;
            let truncated_count = total - MAX_MATERIALIZED;
            let hint = format!("{LABEL_INDENT}+{truncated_count} more files (not shown)");
            lines.push(Line::from(Span::styled(hint, t.cmd_desc)));
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_search(&self, frame: &mut Frame, area: Rect, walking: bool, started_at: Instant) {
        let t = theme::current();
        let query = self.search.value();
        let cursor_byte = TextBuffer::char_to_byte(&query, self.search.x());
        let (before, rest) = query.split_at(cursor_byte);
        let mut chars = rest.chars();
        let cursor_char = chars.next().unwrap_or(' ');
        let after = chars.as_str();

        let mut spans = vec![Span::styled(super::CHEVRON, t.picker_search_prefix)];

        if walking {
            let ch = spinner_frame(started_at.elapsed().as_millis());
            spans.push(Span::styled(format!("{ch} "), t.cmd_desc));
        }

        spans.extend([
            Span::styled(before.to_owned(), t.picker_search_text),
            Span::styled(cursor_char.to_string(), t.cursor),
            Span::styled(after.to_owned(), t.picker_search_text),
        ]);

        frame.render_widget(Paragraph::new(vec![Line::from(spans)]), area);
    }
}

impl Overlay for FilePickerModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }
}

fn build_highlighted_line<'a>(
    text: &str,
    indices: &[u32],
    max_width: usize,
    is_selected: bool,
    t: &'a theme::Theme,
) -> Line<'a> {
    let base_style = if is_selected {
        t.cmd_selected
    } else {
        t.cmd_name
    };
    let match_style = base_style
        .fg(t.highlight_text.fg.unwrap_or_default())
        .add_modifier(Modifier::BOLD);

    let mut spans = vec![Span::styled(LABEL_INDENT, base_style)];
    let mut current_highlighted = false;
    let mut run = String::new();
    let mut cell_width = 0usize;

    for (char_pos, ch) in text.chars().enumerate() {
        let cw = ch.width().unwrap_or(0);
        if cell_width + cw > max_width {
            break;
        }
        cell_width += cw;
        let is_match = indices.binary_search(&(char_pos as u32)).is_ok();

        if is_match != current_highlighted && !run.is_empty() {
            let style = if current_highlighted {
                match_style
            } else {
                base_style
            };
            spans.push(Span::styled(mem::take(&mut run), style));
        }
        current_highlighted = is_match;
        run.push(ch);
    }

    if !run.is_empty() {
        let style = if current_highlighted {
            match_style
        } else {
            base_style
        };
        spans.push(Span::styled(run, style));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use test_case::test_case;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Sets up a picker in Pending state with a controllable nucleo + done
    /// channel without spawning a real walker thread.
    fn pending_picker() -> (FilePickerModal, flume::Sender<()>) {
        let mut picker = FilePickerModal::new();
        let notify = Arc::new(|| {});
        let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), notify, None, 1);
        let (done_tx, done_rx) = flume::bounded(1);
        picker.session = Some(FileWalkerSession {
            nucleo,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            matches: Vec::new(),
            total_matches: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            done_rx,
            started_at: Instant::now(),
            walking: true,
            matching: false,
            visible: false,
        });
        (picker, done_tx)
    }

    fn inject_file(picker: &FilePickerModal, path: &str) {
        let session = picker.session.as_ref().unwrap();
        let injector = session.nucleo.injector();
        injector.push((), |_, cols| {
            cols[0] = Utf32String::from(path);
        });
    }

    #[test]
    fn pending_transitions_to_visible_when_files_arrive() {
        let (mut picker, _done_tx) = pending_picker();
        inject_file(&picker, "src/main.rs");

        picker.tick();
        assert!(picker.session.as_ref().unwrap().visible);
    }

    #[test]
    fn pending_closes_on_empty_walk() {
        let (mut picker, done_tx) = pending_picker();
        let _ = done_tx.send(()); // walker finishes normally with no files

        picker.tick(); // walking = false
        picker.tick(); // pending sees !walking + 0 files → close

        assert!(picker.session.is_none());
        assert_eq!(picker.flash.as_deref(), Some(EMPTY_DIR_MSG));
    }

    #[test]
    fn pending_transitions_to_visible_after_debounce() {
        let (mut picker, _done_tx) = pending_picker();
        picker.session.as_mut().unwrap().started_at =
            Instant::now() - std::time::Duration::from_millis(200);

        picker.tick();
        assert!(picker.session.as_ref().unwrap().visible);
    }

    #[test]
    fn pending_stays_pending_before_debounce() {
        let (mut picker, _done_tx) = pending_picker();

        picker.tick();
        assert!(!picker.session.as_ref().unwrap().visible);
    }

    #[test]
    fn walker_crash_flashes_and_closes() {
        let (mut picker, done_tx) = pending_picker();
        drop(done_tx); // simulate thread panic (Disconnected)

        picker.tick();
        assert!(picker.session.is_none());
        assert_eq!(picker.flash.as_deref(), Some(WALKER_CRASHED_MSG));
    }

    #[test]
    fn close_resets_all_state() {
        let (mut picker, _done_tx) = pending_picker();
        inject_file(&picker, "a.rs");
        picker.tick();
        assert!(picker.session.as_ref().unwrap().visible);

        picker.close();
        assert!(picker.session.is_none());
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.scroll_offset, 0);
    }

    #[test]
    fn open_while_already_open_resets() {
        let (mut picker, _done_tx) = pending_picker();
        inject_file(&picker, "a.rs");
        picker.tick();
        assert!(picker.session.as_ref().unwrap().visible);

        picker.open("/tmp");
        assert!(!picker.session.as_ref().unwrap().visible);
        assert!(picker.session.as_ref().unwrap().matches.is_empty());
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
        assert_eq!(picker.search.value(), "ma");
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
    fn clamp_selection_after_match_list_shrinks() {
        let (mut picker, _done_tx) = pending_picker();
        inject_file(&picker, "a.rs");
        inject_file(&picker, "b.rs");
        inject_file(&picker, "c.rs");
        picker.tick();
        picker.tick();

        picker.selected = 2;
        picker.session.as_mut().unwrap().matches.truncate(1);
        picker.clamp_selection();
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn matches_capped_at_max_materialized() {
        let (mut picker, _done_tx) = pending_picker();
        let count = MAX_MATERIALIZED + 50;
        for i in 0..count {
            inject_file(&picker, &format!("file_{i:05}.rs"));
        }
        // tick until nucleo has processed all items
        for _ in 0..20 {
            picker.tick();
        }

        let session = picker.session.as_ref().unwrap();
        assert_eq!(session.total_matches, count);
        assert_eq!(session.matches.len(), MAX_MATERIALIZED as usize);
    }

    /// Returns a visible picker with `n` synthetic matches (no nucleo timing).
    fn picker_with_matches(n: usize) -> FilePickerModal {
        let (mut picker, _done_tx) = pending_picker();
        let session = picker.session.as_mut().unwrap();
        session.visible = true;
        session.matches = (0..n)
            .map(|i| FilePickerModalMatch {
                path: format!("file_{i:03}.rs"),
                indices: Vec::new(),
            })
            .collect();
        session.total_matches = n as u32;
        picker
    }

    #[test]
    fn resize_taller_clamps_scroll_offset() {
        let mut picker = picker_with_matches(20);

        // simulate: small viewport, scrolled to the bottom
        picker.viewport_height = 5;
        picker.selected = 19;
        picker.scroll_offset = 15; // shows entries 15..20
        picker.ensure_visible();
        assert_eq!(picker.scroll_offset, 15);

        // simulate: terminal resize makes viewport much taller
        picker.viewport_height = 20;
        picker.ensure_visible();
        // scroll_offset must drop to 0 so 20 entries fill 20 rows (no gap)
        assert_eq!(picker.scroll_offset, 0);
    }

    #[test]
    fn resize_taller_keeps_selected_visible() {
        let mut picker = picker_with_matches(30);

        // small viewport, selected near the end, scrolled to show it
        picker.viewport_height = 5;
        picker.selected = 28;
        picker.scroll_offset = 25;
        picker.ensure_visible();
        assert_eq!(picker.scroll_offset, 25);

        // grow viewport to 15 rows — clamp reduces scroll but selected stays visible
        picker.viewport_height = 15;
        picker.ensure_visible();
        assert_eq!(picker.scroll_offset, 15); // 30 - 15
        assert!(picker.selected >= picker.scroll_offset);
        assert!(picker.selected < picker.scroll_offset + picker.viewport_height);
    }

    #[test]
    fn resize_viewport_larger_than_matches_resets_scroll() {
        let mut picker = picker_with_matches(5);

        picker.viewport_height = 3;
        picker.selected = 4;
        picker.scroll_offset = 2;
        picker.ensure_visible();
        assert_eq!(picker.scroll_offset, 2);

        // viewport grows larger than the total number of matches
        picker.viewport_height = 10;
        picker.ensure_visible();
        assert_eq!(picker.scroll_offset, 0);
    }

    #[test_case(&[], 3 ; "empty_indices")]
    #[test_case(&[0, 2], 5 ; "sparse_match")]
    fn build_highlighted_line_does_not_panic(indices: &[u32], max_width: usize) {
        let t = theme::current();
        let _ = build_highlighted_line("hello", indices, max_width, false, &t);
    }
}
