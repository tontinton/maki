use std::sync::Arc;

use crossterm::event::KeyEvent;
use maki_agent::{SharedBuf, SnapshotLine};
use maki_lua::{Anchor, Border, FloatConfig, TitlePos, WinCommand, WinEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::components::{
    Overlay, hint_line, keybindings::key_event_to_string, scrollbar::render_vertical_scrollbar,
    tool_display::resolve_span_style,
};
use crate::theme;

const BORDER_CHROME: u16 = 2;

struct FloatWindow {
    id: u32,
    buf: Arc<SharedBuf>,
    config: FloatConfig,
    scroll_offset: usize,
    cached_lines: Arc<Vec<SnapshotLine>>,
    viewport_h: u16,
    content_width: u16,
    content_height: u16,
    cursor: usize,
    event_tx: flume::Sender<WinEvent>,
    cmd_rx: flume::Receiver<WinCommand>,
}

impl FloatWindow {
    fn sync_scroll(&mut self) {
        self.cursor = clamp_cursor(
            self.cursor,
            self.cached_lines.len(),
            self.config.reserved_bottom,
        );
        self.scroll_offset = adjust_scroll(
            self.cursor,
            self.scroll_offset,
            self.cached_lines.len(),
            self.viewport_h,
            self.config.reserved_bottom,
        );
    }
}

pub(crate) struct FloatManager {
    windows: Vec<FloatWindow>,
    focused_id: Option<u32>,
    next_id: u32,
}

impl FloatManager {
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            focused_id: None,
            next_id: 0,
        }
    }

    pub fn open(
        &mut self,
        buf: Arc<SharedBuf>,
        config: FloatConfig,
        focus: bool,
        event_tx: flume::Sender<WinEvent>,
        cmd_rx: flume::Receiver<WinCommand>,
    ) {
        let cached_lines = buf.read_if_dirty().unwrap_or_default();
        let id = self.next_id;
        self.next_id += 1;

        let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let estimated_width = config
            .width
            .resolve(term_cols)
            .saturating_sub(BORDER_CHROME);
        let estimated_height = config
            .height
            .resolve(term_rows)
            .saturating_sub(BORDER_CHROME);

        let _ = event_tx.try_send(WinEvent::Resize {
            width: estimated_width,
            height: estimated_height,
        });

        let win = FloatWindow {
            id,
            buf,
            config,
            scroll_offset: 0,
            cached_lines,
            viewport_h: 1,
            content_width: estimated_width,
            content_height: estimated_height,
            cursor: 0,
            event_tx,
            cmd_rx,
        };

        self.windows.push(win);
        self.windows.sort_by_key(|w| w.config.zindex);

        if focus {
            self.focused_id = Some(id);
        }
    }

    pub fn tick(&mut self) {
        let mut closed_ids = Vec::new();

        for win in &mut self.windows {
            loop {
                match win.cmd_rx.try_recv() {
                    Ok(WinCommand::SetConfig(patch)) => {
                        win.config.apply_patch(patch);
                    }
                    Ok(WinCommand::SetCursor(row)) => {
                        win.cursor = row;
                        win.sync_scroll();
                    }
                    Ok(WinCommand::Close) => {
                        let _ = win.event_tx.try_send(WinEvent::Close);
                        closed_ids.push(win.id);
                        break;
                    }
                    Err(flume::TryRecvError::Empty) => break,
                    Err(flume::TryRecvError::Disconnected) => {
                        closed_ids.push(win.id);
                        break;
                    }
                }
            }

            if closed_ids.last() == Some(&win.id) {
                continue;
            }

            if let Some(lines) = win.buf.read_if_dirty() {
                win.cached_lines = lines;
                win.sync_scroll();
            }
        }

        if !closed_ids.is_empty() {
            self.windows.retain(|w| !closed_ids.contains(&w.id));
            if let Some(fid) = self.focused_id
                && !self.windows.iter().any(|w| w.id == fid)
            {
                self.focused_id = self.windows.last().map(|w| w.id);
            }
        }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> bool {
        let Some(fid) = self.focused_id else {
            return false;
        };
        let Some(win) = self.windows.iter().find(|w| w.id == fid) else {
            return false;
        };

        let key_str = key_event_to_string(&key_event);
        if !key_str.is_empty() {
            let _ = win.event_tx.try_send(WinEvent::Key {
                key: key_str,
                cursor: win.cursor,
            });
        }
        true
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let mut union = Rect::default();
        let t = theme::current();

        for win in &mut self.windows {
            let popup = resolve_rect(&win.config, area);
            if popup.width == 0 || popup.height == 0 {
                continue;
            }

            frame.render_widget(Clear, popup);

            let border_type = match win.config.border {
                Border::None => None,
                Border::Single => Some(BorderType::Plain),
                Border::Double => Some(BorderType::Double),
                Border::Rounded => Some(BorderType::Rounded),
            };

            let block = if let Some(bt) = border_type {
                let mut b = Block::default()
                    .borders(Borders::ALL)
                    .border_type(bt)
                    .border_style(t.panel_border)
                    .style(ratatui::style::Style::new().bg(t.background));

                if !win.config.title.is_empty() {
                    let alignment = match win.config.title_pos {
                        TitlePos::Left => ratatui::layout::Alignment::Left,
                        TitlePos::Center => ratatui::layout::Alignment::Center,
                        TitlePos::Right => ratatui::layout::Alignment::Right,
                    };
                    b = b
                        .title(win.config.title.as_str())
                        .title_alignment(alignment)
                        .title_style(t.panel_title);
                }
                b
            } else {
                Block::default().style(ratatui::style::Style::new().bg(t.background))
            };

            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let footer_h = u16::from(!win.config.footer.is_empty());
            let content_area = if footer_h > 0 && inner.height > footer_h {
                let footer_rect = Rect {
                    x: inner.x,
                    y: inner.y + inner.height - footer_h,
                    width: inner.width,
                    height: footer_h,
                };
                frame.render_widget(hint_line(&win.config.footer), footer_rect);
                Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: inner.height - footer_h,
                }
            } else {
                inner
            };

            let w = content_area.width;
            let h = content_area.height;
            if win.content_width != w || win.content_height != h {
                let _ = win.event_tx.try_send(WinEvent::Resize {
                    width: w,
                    height: h.saturating_sub(footer_h),
                });
            }
            win.content_width = w;
            win.content_height = h;

            let reserved = win.config.reserved_bottom.min(win.cached_lines.len());
            let scrollable_count = win.cached_lines.len() - reserved;
            let reserved_h = reserved as u16;

            let (scroll_area, pinned_area) = if reserved > 0 && content_area.height > reserved_h {
                let sa = Rect {
                    x: content_area.x,
                    y: content_area.y,
                    width: content_area.width,
                    height: content_area.height - reserved_h,
                };
                let pa = Rect {
                    x: content_area.x,
                    y: content_area.y + sa.height,
                    width: content_area.width,
                    height: reserved_h,
                };
                (sa, Some(pa))
            } else {
                (content_area, None)
            };

            win.viewport_h = scroll_area.height;
            win.sync_scroll();

            let vh = win.viewport_h as usize;
            let end = (win.scroll_offset + vh).min(scrollable_count);
            let visible = &win.cached_lines[win.scroll_offset..end];

            let lines: Vec<Line<'_>> = visible
                .iter()
                .enumerate()
                .map(|(i, sline)| {
                    let mut line = snapshot_to_line(sline);
                    if win.config.cursor_line && win.scroll_offset + i == win.cursor {
                        line = line.style(t.cmd_selected);
                    }
                    line
                })
                .collect();

            frame.render_widget(Paragraph::new(lines), scroll_area);

            if let Some(pa) = pinned_area {
                let pinned: Vec<Line<'_>> = win.cached_lines[scrollable_count..]
                    .iter()
                    .map(snapshot_to_line)
                    .collect();
                frame.render_widget(Paragraph::new(pinned), pa);
            }

            if scrollable_count as u16 > win.viewport_h {
                render_vertical_scrollbar(
                    frame,
                    scroll_area,
                    scrollable_count as u16,
                    win.scroll_offset as u16,
                );
            }

            union = union_rect(union, popup);
        }

        union
    }

    pub fn is_open(&self) -> bool {
        !self.windows.is_empty()
    }

    pub fn close_all(&mut self) {
        for win in &self.windows {
            let _ = win.event_tx.try_send(WinEvent::Close);
        }
        self.windows.clear();
        self.focused_id = None;
    }
}

fn resolve_rect(config: &FloatConfig, area: Rect) -> Rect {
    let w = config.width.resolve(area.width).min(area.width);
    let h = config.height.resolve(area.height).min(area.height);

    let (x, y) = match (config.col, config.row) {
        (None, None) => {
            let cx = area.x + (area.width.saturating_sub(w)) / 2;
            let cy = area.y + (area.height.saturating_sub(h)) / 2;
            (cx, cy)
        }
        (col, row) => {
            let c = col.unwrap_or(0);
            let r = row.unwrap_or(0);

            let x = match config.anchor {
                Anchor::NW | Anchor::SW => {
                    (area.x as i16 + c).clamp(area.x as i16, (area.x + area.width) as i16) as u16
                }
                Anchor::NE | Anchor::SE => ((area.x + area.width) as i16 - w as i16 + c)
                    .clamp(area.x as i16, (area.x + area.width) as i16)
                    as u16,
            };
            let y = match config.anchor {
                Anchor::NW | Anchor::NE => {
                    (area.y as i16 + r).clamp(area.y as i16, (area.y + area.height) as i16) as u16
                }
                Anchor::SW | Anchor::SE => ((area.y + area.height) as i16 - h as i16 + r)
                    .clamp(area.y as i16, (area.y + area.height) as i16)
                    as u16,
            };
            (x, y)
        }
    };

    let clamped_w = w.min(area.x + area.width - x);
    let clamped_h = h.min(area.y + area.height - y);

    Rect::new(x, y, clamped_w, clamped_h)
}

fn adjust_scroll(
    cursor: usize,
    scroll_offset: usize,
    content_len: usize,
    viewport_h: u16,
    reserved_bottom: usize,
) -> usize {
    let vh = viewport_h as usize;
    if vh == 0 {
        return scroll_offset;
    }
    let scrollable_count = content_len.saturating_sub(reserved_bottom);
    let max_offset = scrollable_count.saturating_sub(vh);
    let mut offset = scroll_offset.min(max_offset);
    if cursor < offset {
        offset = cursor;
    } else if cursor >= offset + vh {
        offset = cursor + 1 - vh;
    }
    offset
}

fn clamp_cursor(cursor: usize, line_count: usize, reserved_bottom: usize) -> usize {
    cursor.min(line_count.saturating_sub(1 + reserved_bottom))
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

fn union_rect(a: Rect, b: Rect) -> Rect {
    if a.width == 0 || a.height == 0 {
        return b;
    }
    if b.width == 0 || b.height == 0 {
        return a;
    }
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let x2 = (a.x + a.width).max(b.x + b.width);
    let y2 = (a.y + a.height).max(b.y + b.height);
    Rect::new(x, y, x2 - x, y2 - y)
}

impl Drop for FloatManager {
    fn drop(&mut self) {
        self.close_all();
    }
}

impl Overlay for FloatManager {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::{SnapshotSpan, SpanStyle};
    use maki_lua::{Dimension, FloatConfigPatch};
    use test_case::test_case;

    const EXPECT_OPEN: &str = "expected manager to have open windows";
    const EXPECT_CLOSED: &str = "expected manager to have no open windows";
    const EXPECT_CURSOR: &str = "unexpected cursor position";

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

    fn make_config() -> FloatConfig {
        FloatConfig {
            cursor_line: true,
            ..FloatConfig::default()
        }
    }

    fn make_buf(lines: &[&str]) -> Arc<SharedBuf> {
        let buf = Arc::new(SharedBuf::new());
        for l in lines {
            buf.append(make_line(l));
        }
        buf
    }

    fn open_with_lines(
        mgr: &mut FloatManager,
        lines: &[&str],
    ) -> (flume::Receiver<WinEvent>, flume::Sender<WinCommand>) {
        let (event_tx, cmd_rx, event_rx, cmd_tx) = make_channels();
        let buf = make_buf(lines);
        mgr.open(buf, make_config(), true, event_tx, cmd_rx);
        (event_rx, cmd_tx)
    }

    #[test]
    fn resolve_rect_percent() {
        let area = Rect::new(0, 0, 200, 100);
        let config = FloatConfig {
            width: Dimension::Percent(50),
            height: Dimension::Percent(40),
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.width, 100);
        assert_eq!(r.height, 40);
        assert_eq!(r.x, 50);
        assert_eq!(r.y, 30);
    }

    #[test]
    fn resolve_rect_absolute_positioned() {
        let area = Rect::new(0, 0, 80, 40);
        let config = FloatConfig {
            width: Dimension::Abs(20),
            height: Dimension::Abs(10),
            row: Some(5),
            col: Some(10),
            anchor: Anchor::NW,
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 5);
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 10);
    }

    #[test]
    fn resolve_rect_anchor_se() {
        let area = Rect::new(0, 0, 100, 50);
        let config = FloatConfig {
            width: Dimension::Abs(20),
            height: Dimension::Abs(10),
            row: Some(0),
            col: Some(0),
            anchor: Anchor::SE,
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.x, 80);
        assert_eq!(r.y, 40);
    }

    #[test]
    fn resolve_rect_clamps_to_area() {
        let area = Rect::new(0, 0, 30, 20);
        let config = FloatConfig {
            width: Dimension::Abs(50),
            height: Dimension::Abs(50),
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.width, 30);
        assert_eq!(r.height, 20);
    }

    #[test]
    fn resolve_rect_anchor_ne() {
        let area = Rect::new(0, 0, 100, 50);
        let config = FloatConfig {
            width: Dimension::Abs(20),
            height: Dimension::Abs(10),
            row: Some(5),
            col: Some(0),
            anchor: Anchor::NE,
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.x, 80);
        assert_eq!(r.y, 5);
    }

    #[test]
    fn resolve_rect_anchor_sw() {
        let area = Rect::new(0, 0, 100, 50);
        let config = FloatConfig {
            width: Dimension::Abs(20),
            height: Dimension::Abs(10),
            row: Some(0),
            col: Some(5),
            anchor: Anchor::SW,
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.x, 5);
        assert_eq!(r.y, 40);
    }

    #[test]
    fn resolve_rect_negative_offset() {
        let area = Rect::new(0, 0, 100, 50);
        let config = FloatConfig {
            width: Dimension::Abs(20),
            height: Dimension::Abs(10),
            row: Some(-5),
            col: Some(-10),
            anchor: Anchor::SE,
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.x, 70);
        assert_eq!(r.y, 35);
    }

    #[test]
    fn resolve_rect_nonzero_area_origin() {
        let area = Rect::new(10, 5, 80, 40);
        let config = FloatConfig {
            width: Dimension::Abs(20),
            height: Dimension::Abs(10),
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.x, 40);
        assert_eq!(r.y, 20);
        assert!(r.x >= area.x && r.x + r.width <= area.x + area.width);
        assert!(r.y >= area.y && r.y + r.height <= area.y + area.height);
    }

    #[test]
    fn resolve_rect_zero_size_area() {
        let area = Rect::new(0, 0, 0, 0);
        let config = FloatConfig {
            width: Dimension::Abs(20),
            height: Dimension::Abs(10),
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
    }

    #[test]
    fn resolve_rect_col_only_defaults_row_zero() {
        let area = Rect::new(0, 0, 100, 50);
        let config = FloatConfig {
            width: Dimension::Abs(20),
            height: Dimension::Abs(10),
            row: None,
            col: Some(10),
            anchor: Anchor::NW,
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area);
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 0, "only col is set, so row falls back to 0");
    }

    #[test_case(0, 5, 0, 10, 0 => 0 ; "empty_content")]
    #[test_case(3, 5, 10, 0, 0 => 5 ; "zero_viewport_is_noop")]
    #[test_case(2, 5, 20, 5, 0 => 2 ; "cursor_above_viewport")]
    #[test_case(15, 0, 20, 5, 0 => 11 ; "cursor_below_viewport")]
    #[test_case(7, 0, 10, 1, 0 => 7 ; "single_line_viewport")]
    #[test_case(7, 0, 10, 5, 2 => 3 ; "reserved_bottom_limits_max_offset")]
    #[test_case(4, 0, 10, 5, 0 => 0 ; "cursor_exactly_at_viewport_bottom_edge")]
    #[test_case(5, 0, 10, 5, 0 => 1 ; "cursor_one_past_viewport_bottom")]
    #[test_case(0, 0, 3, 10, 0 => 0 ; "content_smaller_than_viewport")]
    #[test_case(0, 99, 5, 3, 0 => 0 ; "scroll_offset_past_max_cursor_pulls_down")]
    fn adjust_scroll_cases(
        cursor: usize,
        scroll: usize,
        lines: usize,
        vh: u16,
        reserved: usize,
    ) -> usize {
        adjust_scroll(cursor, scroll, lines, vh, reserved)
    }

    #[test_case(0, 5, 0 => 0 ; "at_start")]
    #[test_case(10, 5, 0 => 4 ; "past_end")]
    #[test_case(3, 5, 1 => 3 ; "within_range_with_reserved")]
    #[test_case(4, 5, 1 => 3 ; "clamped_by_reserved")]
    #[test_case(0, 0, 0 => 0 ; "empty_content")]
    fn clamp_cursor_cases(cursor: usize, line_count: usize, reserved: usize) -> usize {
        clamp_cursor(cursor, line_count, reserved)
    }

    #[test]
    fn open_close_lifecycle() {
        let mut mgr = FloatManager::new();
        assert!(!mgr.is_open(), "{}", EXPECT_CLOSED);

        let (event_rx, _cmd_tx) = open_with_lines(&mut mgr, &["hello"]);
        assert!(mgr.is_open(), "{}", EXPECT_OPEN);

        mgr.close_all();
        assert!(!mgr.is_open(), "{}", EXPECT_CLOSED);
        assert!(
            event_rx.drain().any(|e| matches!(e, WinEvent::Close)),
            "expected Close event on close_all"
        );
    }

    #[test]
    fn multi_window_zindex_ordering() {
        let mut mgr = FloatManager::new();

        let mut cfg_low = make_config();
        cfg_low.zindex = 10;
        let (event_tx1, cmd_rx1, _event_rx1, _cmd_tx1) = make_channels();
        mgr.open(make_buf(&["low"]), cfg_low, true, event_tx1, cmd_rx1);

        let mut cfg_high = make_config();
        cfg_high.zindex = 90;
        let (event_tx2, cmd_rx2, _event_rx2, _cmd_tx2) = make_channels();
        mgr.open(make_buf(&["high"]), cfg_high, true, event_tx2, cmd_rx2);

        assert_eq!(mgr.windows.len(), 2);
        assert_eq!(mgr.windows[0].config.zindex, 10);
        assert_eq!(mgr.windows[1].config.zindex, 90);
    }

    #[test]
    fn focus_transfer() {
        let mut mgr = FloatManager::new();

        let cfg1 = make_config();
        let (tx1, rx1, _, _) = make_channels();
        mgr.open(make_buf(&["a"]), cfg1, true, tx1, rx1);
        assert_eq!(mgr.focused_id, Some(0));

        let cfg2 = make_config();
        let (tx2, rx2, _, _) = make_channels();
        mgr.open(make_buf(&["b"]), cfg2, false, tx2, rx2);
        assert_eq!(mgr.focused_id, Some(0), "focus=false keeps old focus");

        let cfg3 = make_config();
        let (tx3, rx3, _, _) = make_channels();
        mgr.open(make_buf(&["c"]), cfg3, true, tx3, rx3);
        assert_eq!(mgr.focused_id, Some(2), "focus=true steals focus");
    }

    #[test]
    fn gc_on_disconnect() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, cmd_tx) = make_channels();
        mgr.open(make_buf(&["a"]), make_config(), true, event_tx, cmd_rx);
        assert!(mgr.is_open(), "{}", EXPECT_OPEN);

        drop(cmd_tx);
        mgr.tick();
        assert!(!mgr.is_open(), "{}", EXPECT_CLOSED);
    }

    #[test]
    fn set_cursor_command() {
        let mut mgr = FloatManager::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a", "b", "c", "d", "e"]);

        cmd_tx.send(WinCommand::SetCursor(3)).unwrap();
        mgr.tick();
        assert_eq!(mgr.windows[0].cursor, 3, "{}", EXPECT_CURSOR);
    }

    #[test]
    fn apply_config_patch() {
        let mut mgr = FloatManager::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a"]);

        cmd_tx
            .send(WinCommand::SetConfig(FloatConfigPatch {
                title: Some("Updated".to_string()),
                zindex: Some(99),
                ..FloatConfigPatch::default()
            }))
            .unwrap();
        mgr.tick();

        assert_eq!(mgr.windows[0].config.title, "Updated");
        assert_eq!(mgr.windows[0].config.zindex, 99);
    }

    #[test]
    fn close_command_from_lua() {
        let mut mgr = FloatManager::new();
        let (event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a"]);

        cmd_tx.send(WinCommand::Close).unwrap();
        mgr.tick();
        assert!(!mgr.is_open(), "{}", EXPECT_CLOSED);
        assert!(event_rx.drain().any(|e| matches!(e, WinEvent::Close)));
    }

    #[test]
    fn key_forwarded_to_lua() {
        let mut mgr = FloatManager::new();
        let (event_rx, _cmd_tx) = open_with_lines(&mut mgr, &["line1"]);

        let key_event = KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        );
        let handled = mgr.handle_key(key_event);
        assert!(handled, "true when a window has focus");

        let evt = event_rx.drain().find(|e| matches!(e, WinEvent::Key { .. }));
        assert!(evt.is_some(), "key forwarded to lua");
    }

    #[test]
    fn handle_key_returns_false_when_empty() {
        let mut mgr = FloatManager::new();
        let key_event = KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert!(
            !mgr.handle_key(key_event),
            "handle_key should return false with no windows"
        );
    }

    #[test]
    fn buf_content_update() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        buf.append(make_line("initial"));
        mgr.open(buf.clone(), make_config(), true, event_tx, cmd_rx);
        assert_eq!(mgr.windows[0].cached_lines.len(), 1);

        buf.append(make_line("second"));
        mgr.tick();
        assert_eq!(mgr.windows[0].cached_lines.len(), 2);
    }

    #[test]
    fn cursor_clamps_on_content_shrink() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        for i in 0..5 {
            buf.append(make_line(&format!("line{i}")));
        }
        mgr.open(buf.clone(), make_config(), true, event_tx, cmd_rx);
        mgr.windows[0].cursor = 4;

        buf.set_lines(vec![make_line("only")]);
        mgr.tick();
        assert_eq!(mgr.windows[0].cursor, 0, "{}", EXPECT_CURSOR);
    }

    #[test]
    fn union_rect_identity_with_zero() {
        let a = Rect::new(10, 20, 30, 40);
        let zero = Rect::new(0, 0, 0, 0);
        assert_eq!(union_rect(zero, a), a);
        assert_eq!(union_rect(a, zero), a);
    }

    #[test]
    fn union_rect_overlapping() {
        let a = Rect::new(10, 10, 20, 20);
        let b = Rect::new(20, 20, 20, 20);
        let r = union_rect(a, b);
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 10);
        assert_eq!(r.width, 30);
        assert_eq!(r.height, 30);
    }

    #[test]
    fn union_rect_disjoint() {
        let a = Rect::new(0, 0, 5, 5);
        let b = Rect::new(50, 50, 10, 10);
        let r = union_rect(a, b);
        assert_eq!(r.x, 0);
        assert_eq!(r.y, 0);
        assert_eq!(r.width, 60);
        assert_eq!(r.height, 60);
    }

    #[test]
    fn union_rect_contained() {
        let outer = Rect::new(0, 0, 100, 100);
        let inner = Rect::new(10, 10, 20, 20);
        let r = union_rect(outer, inner);
        assert_eq!(r, outer);
    }

    #[test]
    fn close_focused_falls_back_to_last_by_zindex() {
        let mut mgr = FloatManager::new();

        let (tx1, rx1, _, _cmd_tx1) = make_channels();
        let mut cfg1 = make_config();
        cfg1.zindex = 10;
        mgr.open(make_buf(&["a"]), cfg1, true, tx1, rx1);

        let (tx2, rx2, _, cmd_tx2) = make_channels();
        let mut cfg2 = make_config();
        cfg2.zindex = 50;
        mgr.open(make_buf(&["b"]), cfg2, true, tx2, rx2);

        let (tx3, rx3, _, _cmd_tx3) = make_channels();
        let mut cfg3 = make_config();
        cfg3.zindex = 30;
        mgr.open(make_buf(&["c"]), cfg3, false, tx3, rx3);

        assert_eq!(mgr.focused_id, Some(1));
        cmd_tx2.send(WinCommand::Close).unwrap();
        mgr.tick();

        assert_eq!(mgr.windows.len(), 2);
        let fallback_id = mgr.focused_id.expect("should have fallback focus");
        let fallback_win = mgr.windows.iter().find(|w| w.id == fallback_id);
        assert!(
            fallback_win.is_some(),
            "fallback id should exist in windows"
        );
    }

    #[test]
    fn multiple_windows_close_in_same_tick() {
        let mut mgr = FloatManager::new();

        let (tx1, rx1, erx1, cmd_tx1) = make_channels();
        mgr.open(make_buf(&["a"]), make_config(), true, tx1, rx1);

        let (tx2, rx2, erx2, cmd_tx2) = make_channels();
        mgr.open(make_buf(&["b"]), make_config(), true, tx2, rx2);

        cmd_tx1.send(WinCommand::Close).unwrap();
        cmd_tx2.send(WinCommand::Close).unwrap();
        mgr.tick();

        assert!(!mgr.is_open(), "{}", EXPECT_CLOSED);
        assert!(erx1.drain().any(|e| matches!(e, WinEvent::Close)));
        assert!(erx2.drain().any(|e| matches!(e, WinEvent::Close)));
        assert_eq!(mgr.focused_id, None);
    }

    #[test]
    fn set_cursor_on_empty_buf() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        mgr.open(buf, make_config(), true, event_tx, cmd_rx);

        cmd_tx.send(WinCommand::SetCursor(5)).unwrap();
        mgr.tick();
        assert_eq!(mgr.windows[0].cursor, 0, "cursor clamps to 0 on empty buf");
    }

    #[test]
    fn multiple_commands_in_single_tick() {
        let mut mgr = FloatManager::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a", "b", "c", "d", "e"]);

        cmd_tx
            .send(WinCommand::SetConfig(FloatConfigPatch {
                title: Some("Updated".to_string()),
                ..FloatConfigPatch::default()
            }))
            .unwrap();
        cmd_tx.send(WinCommand::SetCursor(3)).unwrap();
        mgr.tick();

        assert_eq!(mgr.windows[0].config.title, "Updated");
        assert_eq!(mgr.windows[0].cursor, 3, "{}", EXPECT_CURSOR);
    }

    #[test]
    fn cursor_does_not_enter_reserved_bottom() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, cmd_tx) = make_channels();
        let buf = make_buf(&["a", "b", "c", "d", "e"]);
        let mut cfg = make_config();
        cfg.reserved_bottom = 2;
        mgr.open(buf, cfg, true, event_tx, cmd_rx);

        cmd_tx.send(WinCommand::SetCursor(99)).unwrap();
        mgr.tick();
        assert_eq!(
            mgr.windows[0].cursor, 2,
            "cursor stops before reserved bottom rows"
        );
    }

    #[test]
    fn reserved_bottom_clamp_on_shrink() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        for i in 0..5 {
            buf.append(make_line(&format!("line{i}")));
        }
        let mut cfg = make_config();
        cfg.reserved_bottom = 1;
        mgr.open(buf.clone(), cfg, true, event_tx, cmd_rx);
        mgr.windows[0].cursor = 3;

        buf.set_lines(vec![make_line("a"), make_line("b")]);
        mgr.tick();
        assert_eq!(
            mgr.windows[0].cursor, 0,
            "cursor clamps accounting for reserved rows"
        );
    }

    #[test]
    fn key_only_goes_to_focused_window() {
        let mut mgr = FloatManager::new();

        let (tx1, rx1, erx1, _) = make_channels();
        mgr.open(make_buf(&["a"]), make_config(), true, tx1, rx1);

        let (tx2, rx2, erx2, _) = make_channels();
        mgr.open(make_buf(&["b"]), make_config(), true, tx2, rx2);

        assert_eq!(mgr.focused_id, Some(1), "latest focused window");

        let key_event = KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        );
        mgr.handle_key(key_event);

        let win1_keys: Vec<_> = erx1
            .drain()
            .filter(|e| matches!(e, WinEvent::Key { .. }))
            .collect();
        let win2_keys: Vec<_> = erx2
            .drain()
            .filter(|e| matches!(e, WinEvent::Key { .. }))
            .collect();
        assert!(win1_keys.is_empty(), "unfocused window gets nothing");
        assert_eq!(win2_keys.len(), 1, "only focused window gets the key");
    }

    #[test]
    fn zindex_insertion_order_preserved_for_equal_zindex() {
        let mut mgr = FloatManager::new();

        let (tx1, rx1, _, _) = make_channels();
        let mut cfg1 = make_config();
        cfg1.zindex = 50;
        mgr.open(make_buf(&["first"]), cfg1, true, tx1, rx1);

        let (tx2, rx2, _, _) = make_channels();
        let mut cfg2 = make_config();
        cfg2.zindex = 50;
        mgr.open(make_buf(&["second"]), cfg2, true, tx2, rx2);

        assert!(mgr.windows[0].config.zindex <= mgr.windows[1].config.zindex);
        assert_eq!(mgr.windows.len(), 2);
    }
}
