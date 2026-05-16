use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use maki_agent::{SharedBuf, SnapshotLine};
use maki_lua::{WinCommand, WinEvent, WinOpts};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::{
    Overlay, hint_line, is_ctrl,
    keybindings::{key, key_event_to_string},
    modal::{CHROME_LINES, Modal},
    scrollbar::render_vertical_scrollbar,
    tool_display::resolve_span_style,
};
use crate::theme;

const WIDTH_PERCENT: u16 = 60;
const MAX_HEIGHT_PERCENT: u16 = 70;

struct OpenState {
    buf: Arc<SharedBuf>,
    title: String,
    footer: Vec<(String, String)>,
    cursor_line: bool,
    reserved_bottom: usize,
    cursor: usize,
    scroll_offset: usize,
    event_tx: flume::Sender<WinEvent>,
    cmd_rx: flume::Receiver<WinCommand>,
    cached_lines: Arc<Vec<SnapshotLine>>,
    viewport_h: u16,
    content_width: u16,
}

pub(crate) struct LuaFloatWindow {
    state: Option<OpenState>,
}

impl LuaFloatWindow {
    pub fn new() -> Self {
        Self { state: None }
    }

    pub fn open(
        &mut self,
        buf: Arc<SharedBuf>,
        opts: WinOpts,
        event_tx: flume::Sender<WinEvent>,
        cmd_rx: flume::Receiver<WinCommand>,
    ) {
        let cached_lines = buf.read_if_dirty().unwrap_or_default();
        let estimated_width = crossterm::terminal::size()
            .map(|(cols, _)| cols * WIDTH_PERCENT / 100 - CHROME_LINES)
            .unwrap_or(0);
        let _ = event_tx.try_send(WinEvent::Resize {
            width: estimated_width,
        });
        self.state = Some(OpenState {
            buf,
            title: opts.title,
            footer: opts.footer,
            cursor_line: opts.cursor_line,
            reserved_bottom: opts.reserved_bottom,
            cursor: 0,
            scroll_offset: 0,
            event_tx,
            cmd_rx,
            cached_lines,
            viewport_h: 1,
            content_width: estimated_width,
        });
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) {
        let Some(s) = &mut self.state else { return };

        if key_event.code == KeyCode::Esc || (is_ctrl(&key_event) && key::QUIT.matches(key_event)) {
            self.close();
            return;
        }

        match key_event.code {
            KeyCode::Up => s.cursor = s.cursor.saturating_sub(1),
            KeyCode::Down => {
                s.cursor = clamp_cursor(s.cursor + 1, &s.cached_lines, s.reserved_bottom)
            }
            _ => {}
        }
        adjust_scroll(s);

        let key_str = key_event_to_string(&key_event);
        if !key_str.is_empty() {
            let _ = s.event_tx.try_send(WinEvent::Key {
                key: key_str,
                cursor: s.cursor,
            });
        }
    }

    pub fn tick(&mut self) {
        let Some(s) = &mut self.state else { return };

        loop {
            match s.cmd_rx.try_recv() {
                Ok(WinCommand::SetConfig { title, footer }) => {
                    if let Some(t) = title {
                        s.title = t;
                    }
                    if let Some(f) = footer {
                        s.footer = f;
                    }
                }
                Ok(WinCommand::SetCursor(row)) => {
                    s.cursor = clamp_cursor(row, &s.cached_lines, s.reserved_bottom);
                    adjust_scroll(s);
                }
                Ok(WinCommand::Close) => {
                    self.close();
                    return;
                }
                Err(flume::TryRecvError::Empty) => break,
                Err(flume::TryRecvError::Disconnected) => {
                    self.close();
                    return;
                }
            }
        }

        if let Some(lines) = s.buf.read_if_dirty() {
            s.cached_lines = lines;
            s.cursor = clamp_cursor(s.cursor, &s.cached_lines, s.reserved_bottom);
            adjust_scroll(s);
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let Some(s) = &mut self.state else {
            return Rect::default();
        };

        let content_h = s.cached_lines.len() as u16;
        let footer_h = u16::from(!s.footer.is_empty());
        let total_content = content_h + footer_h;

        let modal = Modal {
            title: &s.title,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let (popup, inner) = modal.render(frame, area, total_content);

        let (content_area, footer_area) = if footer_h > 0 {
            let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
            (chunks[0], Some(chunks[1]))
        } else {
            (inner, None)
        };

        let w = content_area.width;
        if s.content_width != w {
            let _ = s.event_tx.try_send(WinEvent::Resize { width: w });
        }
        s.content_width = w;

        let reserved = s.reserved_bottom.min(s.cached_lines.len());
        let scrollable_count = s.cached_lines.len() - reserved;
        let reserved_h = reserved as u16;

        let (scroll_area, pinned_area) = if reserved > 0 && content_area.height > reserved_h {
            let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(reserved_h)])
                .split(content_area);
            (chunks[0], Some(chunks[1]))
        } else {
            (content_area, None)
        };

        s.viewport_h = scroll_area.height;
        adjust_scroll(s);

        let vh = s.viewport_h as usize;
        let end = (s.scroll_offset + vh).min(scrollable_count);
        let visible = &s.cached_lines[s.scroll_offset..end];

        let t = theme::current();
        let lines: Vec<Line<'_>> = visible
            .iter()
            .enumerate()
            .map(|(i, sline)| {
                let mut line = snapshot_to_line(sline);
                if s.cursor_line && s.scroll_offset + i == s.cursor {
                    line = line.style(t.cmd_selected);
                }
                line
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), scroll_area);

        if let Some(pa) = pinned_area {
            let pinned: Vec<Line<'_>> = s.cached_lines[scrollable_count..]
                .iter()
                .map(snapshot_to_line)
                .collect();
            frame.render_widget(Paragraph::new(pinned), pa);
        }

        if let Some(fa) = footer_area {
            frame.render_widget(hint_line(&s.footer), fa);
        }

        if scrollable_count as u16 > s.viewport_h {
            render_vertical_scrollbar(
                frame,
                scroll_area,
                scrollable_count as u16,
                s.scroll_offset as u16,
            );
        }

        popup
    }

    pub fn is_open(&self) -> bool {
        self.state.is_some()
    }

    fn close(&mut self) {
        if let Some(s) = self.state.take() {
            let _ = s.event_tx.try_send(WinEvent::Close);
        }
    }
}

fn snapshot_to_line(sline: &SnapshotLine) -> Line<'_> {
    Line::from(
        sline
            .spans
            .iter()
            .map(|span| Span::styled(span.text.clone(), resolve_span_style(&span.style)))
            .collect::<Vec<_>>(),
    )
}

fn clamp_cursor(cursor: usize, lines: &[SnapshotLine], reserved: usize) -> usize {
    cursor.min(lines.len().saturating_sub(1 + reserved))
}

fn adjust_scroll(s: &mut OpenState) {
    let vh = s.viewport_h as usize;
    if vh == 0 {
        return;
    }
    let scrollable_count = s.cached_lines.len().saturating_sub(s.reserved_bottom);
    let max_offset = scrollable_count.saturating_sub(vh);
    s.scroll_offset = s.scroll_offset.min(max_offset);
    if s.cursor < s.scroll_offset {
        s.scroll_offset = s.cursor;
    } else if s.cursor >= s.scroll_offset + vh {
        s.scroll_offset = s.cursor + 1 - vh;
    }
}

impl Drop for LuaFloatWindow {
    fn drop(&mut self) {
        self.close();
    }
}

impl Overlay for LuaFloatWindow {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::{SnapshotSpan, SpanStyle};
    use test_case::test_case;

    fn make_line(text: &str) -> SnapshotLine {
        SnapshotLine {
            spans: vec![SnapshotSpan {
                text: text.to_string(),
                style: SpanStyle::Default,
            }],
        }
    }

    fn make_channels() -> (
        flume::Sender<WinEvent>,
        flume::Receiver<WinCommand>,
        flume::Receiver<WinEvent>,
        flume::Sender<WinCommand>,
    ) {
        let (event_tx, event_rx) = flume::bounded::<WinEvent>(8);
        let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);
        (event_tx, cmd_rx, event_rx, cmd_tx)
    }

    fn make_opts() -> WinOpts {
        WinOpts {
            title: "Test".to_string(),
            footer: vec![],
            cursor_line: true,
            reserved_bottom: 0,
        }
    }

    fn open_with_lines(
        win: &mut LuaFloatWindow,
        lines: &[&str],
    ) -> (flume::Receiver<WinEvent>, flume::Sender<WinCommand>) {
        let (event_tx, cmd_rx, event_rx, cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        for l in lines {
            buf.append(make_line(l));
        }
        win.open(buf, make_opts(), event_tx, cmd_rx);
        (event_rx, cmd_tx)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn state(win: &LuaFloatWindow) -> &OpenState {
        win.state.as_ref().unwrap()
    }

    fn state_mut(win: &mut LuaFloatWindow) -> &mut OpenState {
        win.state.as_mut().unwrap()
    }

    #[test]
    fn key_forwarded_to_lua() {
        let mut win = LuaFloatWindow::new();
        let (event_rx, _cmd_tx) = open_with_lines(&mut win, &["line1"]);
        win.handle_key(key(KeyCode::Char('a')));
        let evt = event_rx
            .drain()
            .find(|e| matches!(e, WinEvent::Key { .. }))
            .expect("expected a Key event");
        assert!(matches!(evt, WinEvent::Key { key, cursor: 0 } if key == "a"));
    }

    #[test]
    fn cursor_movement_and_clamping() {
        let mut win = LuaFloatWindow::new();
        let (_event_rx, _cmd_tx) = open_with_lines(&mut win, &["a", "b", "c"]);
        win.handle_key(key(KeyCode::Up));
        assert_eq!(state(&win).cursor, 0, "clamps at top");
        win.handle_key(key(KeyCode::Down));
        win.handle_key(key(KeyCode::Down));
        assert_eq!(state(&win).cursor, 2);
        win.handle_key(key(KeyCode::Down));
        assert_eq!(state(&win).cursor, 2, "clamps at bottom");
        win.handle_key(key(KeyCode::Up));
        assert_eq!(state(&win).cursor, 1);
    }

    #[test]
    fn scroll_follows_cursor() {
        let mut win = LuaFloatWindow::new();
        let lines: Vec<&str> = (0..20).map(|_| "line").collect();
        let (_event_rx, _cmd_tx) = open_with_lines(&mut win, &lines);
        state_mut(&mut win).viewport_h = 5;
        for _ in 0..10 {
            win.handle_key(key(KeyCode::Down));
        }
        let s = state(&win);
        assert_eq!(s.cursor, 10);
        assert!(s.scroll_offset + (s.viewport_h as usize) > s.cursor);
        assert!(s.scroll_offset <= s.cursor);
    }

    #[test]
    fn set_cursor_command() {
        let mut win = LuaFloatWindow::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut win, &["a", "b", "c", "d", "e"]);
        state_mut(&mut win).viewport_h = 3;
        cmd_tx.send(WinCommand::SetCursor(4)).unwrap();
        win.tick();
        let s = state(&win);
        assert_eq!(s.cursor, 4);
        assert!(s.scroll_offset <= s.cursor);
    }

    #[test]
    fn close_command_from_lua() {
        let mut win = LuaFloatWindow::new();
        let (event_rx, cmd_tx) = open_with_lines(&mut win, &["a"]);
        cmd_tx.send(WinCommand::Close).unwrap();
        win.tick();
        assert!(!win.is_open());
        assert!(event_rx.drain().any(|e| matches!(e, WinEvent::Close)));
    }

    #[test]
    fn esc_closes() {
        let mut win = LuaFloatWindow::new();
        let (_event_rx, _cmd_tx) = open_with_lines(&mut win, &["a"]);
        win.handle_key(key(KeyCode::Esc));
        assert!(!win.is_open());
    }

    #[test]
    fn buf_content_update() {
        let mut win = LuaFloatWindow::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        buf.append(make_line("initial"));
        win.open(buf.clone(), make_opts(), event_tx, cmd_rx);
        assert_eq!(state(&win).cached_lines.len(), 1);
        buf.append(make_line("second"));
        win.tick();
        assert_eq!(state(&win).cached_lines.len(), 2);
    }

    #[test]
    fn cursor_clamps_on_content_shrink() {
        let mut win = LuaFloatWindow::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        for i in 0..5 {
            buf.append(make_line(&format!("line{i}")));
        }
        win.open(buf.clone(), make_opts(), event_tx, cmd_rx);
        state_mut(&mut win).cursor = 4;
        buf.set_lines(vec![make_line("only")]);
        win.tick();
        assert_eq!(state(&win).cursor, 0);
    }

    #[test]
    fn scroll_clamps_when_viewport_expands() {
        let mut win = LuaFloatWindow::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut win, &["a", "b"]);
        assert_eq!(state(&win).viewport_h, 1);
        cmd_tx.send(WinCommand::SetCursor(1)).unwrap();
        win.tick();
        assert_eq!(
            state(&win).scroll_offset,
            1,
            "scroll_offset pushed by small viewport"
        );
        state_mut(&mut win).viewport_h = 5;
        adjust_scroll(state_mut(&mut win));
        assert_eq!(
            state(&win).scroll_offset,
            0,
            "scroll_offset clamped after viewport grows"
        );
    }

    #[test]
    fn channel_disconnect_closes() {
        let mut win = LuaFloatWindow::new();
        let (event_tx, cmd_rx, _event_rx, cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        buf.append(make_line("a"));
        win.open(buf, make_opts(), event_tx, cmd_rx);
        drop(cmd_tx);
        win.tick();
        assert!(!win.is_open());
    }

    fn make_open_state(
        cursor: usize,
        scroll_offset: usize,
        num_lines: usize,
        viewport_h: u16,
        reserved_bottom: usize,
    ) -> OpenState {
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        let lines: Vec<_> = (0..num_lines).map(|_| make_line("x")).collect();
        OpenState {
            buf,
            title: String::new(),
            footer: vec![],
            cursor_line: true,
            reserved_bottom,
            cursor,
            scroll_offset,
            event_tx,
            cmd_rx,
            cached_lines: Arc::new(lines),
            viewport_h,
            content_width: 0,
        }
    }

    #[test_case(0, 5, 0, 10, 0 => 0 ; "empty_content")]
    #[test_case(3, 5, 10, 0, 0 => 5 ; "zero_viewport_is_noop")]
    #[test_case(2, 5, 20, 5, 0 => 2 ; "cursor_above_viewport")]
    #[test_case(15, 0, 20, 5, 0 => 11 ; "cursor_below_viewport")]
    #[test_case(7, 0, 10, 1, 0 => 7 ; "single_line_viewport")]
    #[test_case(7, 0, 10, 5, 2 => 3 ; "reserved_bottom_limits_max_offset")]
    fn adjust_scroll_cases(
        cursor: usize,
        scroll: usize,
        lines: usize,
        vh: u16,
        reserved: usize,
    ) -> usize {
        let mut s = make_open_state(cursor, scroll, lines, vh, reserved);
        adjust_scroll(&mut s);
        s.scroll_offset
    }

    #[test]
    fn set_cursor_on_empty_content() {
        let mut win = LuaFloatWindow::new();
        let (event_tx, cmd_rx, _event_rx, cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        win.open(buf, make_opts(), event_tx, cmd_rx);
        cmd_tx.send(WinCommand::SetCursor(5)).unwrap();
        win.tick();
        assert_eq!(state(&win).cursor, 0, "cursor clamps to 0 on empty buf");
    }

    #[test]
    fn multiple_commands_in_single_tick() {
        let mut win = LuaFloatWindow::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut win, &["a", "b", "c", "d", "e"]);
        cmd_tx
            .send(WinCommand::SetConfig {
                title: Some("Updated".to_string()),
                footer: None,
            })
            .unwrap();
        cmd_tx.send(WinCommand::SetCursor(3)).unwrap();
        win.tick();
        let s = state(&win);
        assert_eq!(s.title, "Updated");
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn reopen_after_close() {
        let mut win = LuaFloatWindow::new();
        let (_event_rx, _cmd_tx) = open_with_lines(&mut win, &["first"]);
        win.handle_key(key(KeyCode::Esc));
        assert!(!win.is_open());

        let (_event_rx2, _cmd_tx2) = open_with_lines(&mut win, &["second", "third"]);
        assert!(win.is_open());
        assert_eq!(state(&win).cached_lines.len(), 2);
        assert_eq!(state(&win).cursor, 0, "cursor resets on reopen");
    }

    #[test]
    fn cursor_does_not_enter_reserved_bottom() {
        let mut win = LuaFloatWindow::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        for i in 0..5 {
            buf.append(make_line(&format!("line{i}")));
        }
        let mut opts = make_opts();
        opts.reserved_bottom = 1;
        win.open(buf, opts, event_tx, cmd_rx);
        for _ in 0..10 {
            win.handle_key(key(KeyCode::Down));
        }
        assert_eq!(
            state(&win).cursor,
            3,
            "cursor stops before reserved bottom row"
        );
    }

    #[test]
    fn reserved_bottom_clamp_on_shrink() {
        let mut win = LuaFloatWindow::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        for i in 0..5 {
            buf.append(make_line(&format!("line{i}")));
        }
        let mut opts = make_opts();
        opts.reserved_bottom = 1;
        win.open(buf.clone(), opts, event_tx, cmd_rx);
        state_mut(&mut win).cursor = 3;
        buf.set_lines(vec![make_line("a"), make_line("b")]);
        win.tick();
        assert_eq!(
            state(&win).cursor,
            0,
            "cursor clamps accounting for reserved rows"
        );
    }
}
