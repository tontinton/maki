use std::mem;
use std::path::PathBuf;
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
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use tracing::warn;
use unicode_width::UnicodeWidthChar;

use crate::components::scrollbar::render_vertical_scrollbar;
use crate::theme;

const MAX_HEIGHT: u16 = 10;
const NO_MATCHES: &str = "  No matches";
const LABEL_INDENT: &str = "  ";
const EMPTY_DIR_MSG: &str = "Current directory is empty";
const WALKER_CRASHED_MSG: &str = "File scanner crashed";
const PENDING_DEBOUNCE_MS: u128 = 100;
const MAX_MATERIALIZED: u32 = 640;
const FOOTER_HINTS: &str = "↑↓ select · ← parent · → enter · esc close";

pub enum MentionAction {
    Consumed,
    Select(String),
    Navigate(String),
    Close,
    Passthrough,
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

    cwd: String,
    query: String,
    selected: usize,
    scroll_offset: usize,
    viewport_height: usize,

    cancel: Arc<AtomicBool>,
    done_rx: flume::Receiver<()>,
    started_at: Instant,

    walking: bool,
    matching: bool,
    visible: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct MentionFlyout {
    session: Option<Session>,
}

impl MentionFlyout {
    pub fn new() -> Self {
        Self { session: None }
    }

    pub fn open(&mut self, root_cwd: &str, cwd: &str, query: &str) {
        self.close();

        let notify = Arc::new(|| {});
        let nucleo = Nucleo::new(Config::DEFAULT.match_paths(), notify, None, 1);
        let injector = nucleo.injector();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();
        let (done_tx, done_rx) = flume::bounded(1);

        let root = PathBuf::from(root_cwd);
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
                let _ = done_tx.send(());
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
            cwd: cwd.to_string(),
            query: query.to_string(),
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            cancel: cancel_clone,
            done_rx,
            started_at: Instant::now(),
            walking: true,
            matching: false,
            visible: false,
        });

        if !query.is_empty() {
            self.reparse_pattern();
        }
    }

    pub fn close(&mut self) {
        self.session = None;
    }

    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    pub fn set_query(&mut self, cwd: &str, query: &str) {
        if let Some(s) = &mut self.session {
            s.cwd = cwd.to_string();
            s.query = query.to_string();
            self.reparse_pattern();
        }
    }

    fn reparse_pattern(&mut self) {
        let Some(s) = &mut self.session else { return };
        s.nucleo.pattern.reparse(
            0,
            &s.query,
            CaseMatching::Smart,
            Normalization::Smart,
            false,
        );
        s.selected = 0;
        s.scroll_offset = 0;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> MentionAction {
        let Some(s) = &mut self.session else {
            return MentionAction::Close;
        };

        match key.code {
            KeyCode::Esc => MentionAction::Close,
            KeyCode::Enter => {
                if !s.visible {
                    MentionAction::Consumed
                } else if let Some(m) = s.matches.get(s.selected) {
                    if m.path.ends_with('/') {
                        let new_cwd = format!("{}{}", s.cwd, m.path);
                        MentionAction::Navigate(new_cwd)
                    } else {
                        let full_path = format!("{}{}", s.cwd, m.path);
                        MentionAction::Select(full_path)
                    }
                } else {
                    MentionAction::Close
                }
            }
            KeyCode::Up => {
                Self::move_selection_impl(s, -1);
                MentionAction::Consumed
            }
            KeyCode::Down => {
                Self::move_selection_impl(s, 1);
                MentionAction::Consumed
            }
            KeyCode::Right => {
                if s.visible
                    && let Some(m) = s.matches.get(s.selected)
                    && m.path.ends_with('/')
                {
                    let new_cwd = format!("{}{}", s.cwd, m.path);
                    return MentionAction::Navigate(new_cwd);
                }
                MentionAction::Consumed
            }
            KeyCode::Left | KeyCode::Backspace => {
                if s.cwd.is_empty() {
                    MentionAction::Passthrough
                } else {
                    let new_cwd = Self::parent_cwd(&s.cwd);
                    MentionAction::Navigate(new_cwd)
                }
            }
            _ if super::is_ctrl(&key) => {
                if key.code == KeyCode::Char('n') {
                    Self::move_selection_impl(s, 1);
                    MentionAction::Consumed
                } else if key.code == KeyCode::Char('p') {
                    Self::move_selection_impl(s, -1);
                    MentionAction::Consumed
                } else {
                    MentionAction::Passthrough
                }
            }
            _ => MentionAction::Passthrough,
        }
    }

    pub fn parent_cwd(cwd: &str) -> String {
        if cwd.is_empty() {
            return String::new();
        }
        let trimmed = cwd.trim_end_matches('/');
        if let Some(last_slash) = trimmed.rfind('/') {
            format!("{}/", &trimmed[..last_slash])
        } else {
            String::new()
        }
    }

    fn move_selection_impl(s: &mut Session, delta: isize) {
        if s.matches.is_empty() {
            return;
        }
        let new = (s.selected as isize + delta).clamp(0, s.matches.len() as isize - 1);
        s.selected = new as usize;
        Self::ensure_visible_impl(s);
    }

    fn ensure_visible_impl(s: &mut Session) {
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

    pub fn tick(&mut self) -> Option<String> {
        let s = self.session.as_mut()?;

        let status = s.nucleo.tick(0);
        s.matching = status.running;

        if s.walking {
            match s.done_rx.try_recv() {
                Ok(()) => s.walking = false,
                Err(flume::TryRecvError::Disconnected) => {
                    warn!("{WALKER_CRASHED_MSG}: walker thread panicked");
                    self.session = None;
                    return Some(WALKER_CRASHED_MSG.into());
                }
                Err(flume::TryRecvError::Empty) => {}
            }
        }

        if !s.visible {
            let has_files = s.nucleo.injector().injected_items() > 0;
            let debounce_elapsed = s.started_at.elapsed().as_millis() >= PENDING_DEBOUNCE_MS;

            if has_files || (s.walking && debounce_elapsed) {
                s.visible = true;
            } else if !s.walking {
                self.session = None;
                return Some(EMPTY_DIR_MSG.into());
            }
        }

        if status.changed
            && let Some(s) = self.session.as_mut()
        {
            Self::refresh_matches_impl(s);
            Self::clamp_selection_impl(s);
        }

        None
    }

    fn refresh_matches_impl(s: &mut Session) {
        let snapshot = s.nucleo.snapshot();
        s.total_matches = snapshot.matched_item_count();
        let count = s.total_matches.min(MAX_MATERIALIZED);

        s.matches.clear();

        let pattern = snapshot.pattern();
        let has_pattern = !pattern.column_pattern(0).atoms.is_empty();
        let mut indices_buf = Vec::new();

        for item in snapshot.matched_items(0..count) {
            let col = &item.matcher_columns[0];
            let full_path = col.to_string();

            if !full_path.starts_with(&s.cwd) {
                continue;
            }

            let display_path = full_path[s.cwd.len()..].to_string();

            let indices = if has_pattern {
                indices_buf.clear();
                pattern
                    .column_pattern(0)
                    .indices(col.slice(..), &mut s.matcher, &mut indices_buf);
                let offset = s.cwd.chars().count() as u32;
                indices_buf
                    .iter()
                    .map(|&i| i.saturating_sub(offset))
                    .collect()
            } else {
                Vec::new()
            };

            s.matches.push(Match {
                path: display_path,
                indices,
            });
        }
    }

    fn clamp_selection_impl(s: &mut Session) {
        if s.matches.is_empty() {
            s.selected = 0;
            s.scroll_offset = 0;
        } else {
            s.selected = s.selected.min(s.matches.len() - 1);
            Self::ensure_visible_impl(s);
        }
    }

    pub fn height(&self, _width: u16) -> u16 {
        let s = match &self.session {
            Some(s) if s.visible => s,
            _ => return 0,
        };

        let match_count = s.matches.len() as u16;
        let has_query_without_matches = s.matches.is_empty() && !s.query.is_empty();
        let content_rows = if has_query_without_matches {
            1
        } else {
            match_count.min(MAX_HEIGHT)
        };
        content_rows + 2
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let s = match &mut self.session {
            Some(s) if s.visible => s,
            _ => return Rect::default(),
        };

        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(theme::current().tool_dim);
        frame.render_widget(block, area);

        let inner = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(2),
        );
        s.viewport_height = inner.height as usize;
        Self::ensure_visible_impl(s);
        Self::render_list_impl(frame, inner, s);

        let footer_area = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        Self::render_footer_impl(frame, footer_area, s);

        area
    }

    fn render_list_impl(frame: &mut Frame, area: Rect, s: &Session) {
        let t = theme::current();

        if s.matches.is_empty() {
            if !s.query.is_empty() {
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
        let visible_rows = s.viewport_height - hint_row;

        let max_label_width = area.width.saturating_sub(LABEL_INDENT.len() as u16) as usize;
        let end = (s.scroll_offset + visible_rows).min(s.matches.len());

        let mut lines: Vec<Line> = s.matches[s.scroll_offset..end]
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let selected = s.scroll_offset + i == s.selected;
                Self::build_highlighted_line_impl(
                    &m.path,
                    &m.indices,
                    max_label_width,
                    selected,
                    &t,
                )
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

        if s.matches.len() as u16 > s.viewport_height as u16 {
            render_vertical_scrollbar(frame, area, s.matches.len() as u16, s.scroll_offset as u16);
        }
    }

    fn render_footer_impl(frame: &mut Frame, area: Rect, s: &Session) {
        let t = theme::current();
        let match_count = s.total_matches;
        let footer_text = format!("{}   {} matches", FOOTER_HINTS, match_count);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(footer_text, t.item_desc))),
            area,
        );
    }

    fn build_highlighted_line_impl<'a>(
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn flyout_starts_closed() {
        let flyout = MentionFlyout::new();
        assert!(!flyout.is_open());
    }

    #[test]
    fn open_initializes_session() {
        let mut flyout = MentionFlyout::new();
        flyout.open("/tmp", "", "");
        assert!(flyout.is_open());
    }

    #[test]
    fn close_clears_session() {
        let mut flyout = MentionFlyout::new();
        flyout.open("/tmp", "", "");
        flyout.close();
        assert!(!flyout.is_open());
    }

    #[test]
    fn set_query_updates_pattern() {
        let mut flyout = MentionFlyout::new();
        flyout.open("/tmp", "", "");
        flyout.set_query("", "test");
        assert_eq!(flyout.session.as_ref().unwrap().query, "test");
    }

    #[test]
    fn esc_returns_close() {
        let mut flyout = MentionFlyout::new();
        flyout.open("/tmp", "", "");
        assert!(matches!(
            flyout.handle_key(key(KeyCode::Esc)),
            MentionAction::Close
        ));
    }

    #[test]
    fn enter_during_pending_is_consumed() {
        let mut flyout = MentionFlyout::new();
        flyout.open("/tmp", "", "");
        assert!(matches!(
            flyout.handle_key(key(KeyCode::Enter)),
            MentionAction::Consumed
        ));
    }

    #[test]
    fn parent_cwd_empty_returns_empty() {
        assert_eq!(MentionFlyout::parent_cwd(""), "");
    }

    #[test]
    fn parent_cwd_single_segment_returns_empty() {
        assert_eq!(MentionFlyout::parent_cwd("src/"), "");
    }

    #[test]
    fn parent_cwd_nested_returns_parent() {
        assert_eq!(MentionFlyout::parent_cwd("src/comp/"), "src/");
    }

    #[test]
    fn parent_cwd_deep_nested_returns_parent() {
        assert_eq!(MentionFlyout::parent_cwd("src/comp/mod/"), "src/comp/");
    }

    #[test]
    fn left_when_cwd_empty_passthrough() {
        let mut flyout = MentionFlyout::new();
        flyout.open("/tmp", "", "");
        assert!(matches!(
            flyout.handle_key(key(KeyCode::Left)),
            MentionAction::Passthrough
        ));
    }

    #[test]
    fn left_when_cwd_nonempty_navigates_parent() {
        let mut flyout = MentionFlyout::new();
        flyout.open("/tmp", "src/", "");
        assert!(matches!(
            flyout.handle_key(key(KeyCode::Left)),
            MentionAction::Navigate(_)
        ));
    }

    #[test]
    fn ctrl_n_moves_down() {
        let mut flyout = MentionFlyout::new();
        flyout.open("/tmp", "", "");
        let ctrl_n = KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(matches!(flyout.handle_key(ctrl_n), MentionAction::Consumed));
    }

    #[test]
    fn ctrl_p_moves_up() {
        let mut flyout = MentionFlyout::new();
        flyout.open("/tmp", "", "");
        let ctrl_p = KeyEvent {
            code: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(matches!(flyout.handle_key(ctrl_p), MentionAction::Consumed));
    }
}
