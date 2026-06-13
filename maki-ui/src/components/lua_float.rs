use std::sync::Arc;

use crossterm::event::KeyEvent;
use maki_agent::{SharedBuf, SnapshotLine};
use maki_lua::{Anchor, Axis, Border, FloatConfig, Split, TitlePos, WinCommand, WinEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::components::split_layout::SplitReq;
use crate::components::{
    Overlay, keybindings::key_event_to_string, scrollbar::render_vertical_scrollbar,
    tool_display::resolve_span_style,
};
use crate::theme;

/// A top band, a bottom band, and the scrollable middle. When the window is too
/// short for both bands the bottom wins, so footers like keybind hints survive
/// even when the header gets squeezed out.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Layout {
    reserved_top: usize,
    reserved_bot: usize,
    scrollable: usize,
}

impl Layout {
    fn new(reserved_top: usize, reserved_bottom: usize, line_count: usize) -> Self {
        let reserved_bot = reserved_bottom.min(line_count);
        let reserved_top = reserved_top.min(line_count - reserved_bot);
        Self {
            reserved_top,
            reserved_bot,
            scrollable: line_count - reserved_top - reserved_bot,
        }
    }

    fn max_offset(self, viewport_h: u16) -> usize {
        self.scrollable.saturating_sub(viewport_h as usize)
    }
}

/// A floating window managed by lua.
///
/// Every public method leaves these promises intact:
///
/// 1. `cursor` stays in bounds while `cached_lines` is non-empty.
/// 2. `scroll_offset` stays at or below `layout().max_offset(viewport_h)`.
/// 3. [`set_cursor`] and [`bring_cursor_into_view`] place the cursor inside
///    the visible band whenever there is anything to scroll.
/// 4. [`refresh_layout`] only ever clamps the offset down to fit. It never
///    drags it back toward the cursor, which is the bug that ate wheel input
///    on every frame.
struct FloatWindow {
    id: u32,
    buf: Arc<SharedBuf>,
    config: FloatConfig,
    scroll_offset: usize,
    cached_lines: Arc<Vec<SnapshotLine>>,
    /// Locked at the last render. Only [`refresh_layout`] writes here, so
    /// scroll math stays consistent between frames.
    viewport_h: u16,
    last_content: Rect,
    cursor: usize,
    visible: bool,
    event_tx: flume::Sender<WinEvent>,
    cmd_rx: flume::Receiver<WinCommand>,
}

impl FloatWindow {
    fn layout(&self) -> Layout {
        Layout::new(
            self.config.reserved_top,
            self.config.reserved_bottom,
            self.cached_lines.len(),
        )
    }

    /// Positive `delta` scrolls up (closer to the top of the buffer, smaller
    /// `scroll_offset`), negative scrolls down. The cursor is left alone on
    /// purpose so the user can scroll past it and scroll back.
    fn scroll_by(&mut self, delta: i32) {
        let max_offset = self.layout().max_offset(self.viewport_h);
        if delta >= 0 {
            self.scroll_offset = self.scroll_offset.saturating_sub(delta as usize);
        } else {
            self.scroll_offset =
                (self.scroll_offset + delta.unsigned_abs() as usize).min(max_offset);
        }
    }

    fn set_cursor(&mut self, row: usize) {
        self.cursor = row;
        self.bring_cursor_into_view();
    }

    /// Called once per frame from the render path. Only shrinks the offset
    /// when it falls off the end, never nudges it toward the cursor. That
    /// restraint is what keeps mouse wheel scroll from snapping back.
    fn refresh_layout(&mut self, viewport_h: u16) -> Layout {
        self.viewport_h = viewport_h;
        let layout = self.layout();
        let max_offset = layout.max_offset(viewport_h);
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }
        layout
    }

    /// Pulls the cursor into the scrollable band and then slides the offset
    /// to follow it. Use this after the cursor moves or the buffer changes,
    /// never on a plain redraw.
    fn bring_cursor_into_view(&mut self) {
        let layout = self.layout();
        let effective_cursor = self.cursor.saturating_sub(layout.reserved_top);
        let clamped = effective_cursor.min(layout.scrollable.saturating_sub(1));
        self.cursor = clamped + layout.reserved_top;
        self.scroll_offset = adjust_scroll(
            clamped,
            self.scroll_offset,
            layout.scrollable,
            self.viewport_h,
        );
    }
}

pub(crate) struct FloatManager {
    windows: Vec<FloatWindow>,
    focused_id: Option<u32>,
    focused_rect: Option<Rect>,
    next_id: u32,
}

impl FloatManager {
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            focused_id: None,
            focused_rect: None,
            next_id: 0,
        }
    }

    fn split_window_idx(&self, dir: Split) -> Option<usize> {
        self.windows.iter().position(|w| w.config.split == dir)
    }

    /// The one path windows take to leave the manager. Routing every removal
    /// here is what keeps the close event, the window list, and `focused_id`
    /// from ever drifting apart.
    fn remove_windows(&mut self, should_remove: impl Fn(&FloatWindow) -> bool) {
        let focus_lost = self
            .focused_id
            .and_then(|fid| self.windows.iter().find(|w| w.id == fid))
            .is_some_and(&should_remove);

        self.windows.retain(|w| {
            let remove = should_remove(w);
            if remove {
                let _ = w.event_tx.try_send(WinEvent::Close);
            }
            !remove
        });

        if focus_lost {
            self.focused_id = self.windows.last().map(|w| w.id);
            self.focused_rect = None;
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

        // One split per direction, so evicting the old same-direction window
        // goes through the same removal path that guarantees it hears its close.
        if config.split != Split::None && config.split != Split::Panel {
            let dir = config.split;
            self.remove_windows(|w| w.config.split == dir);
        }

        let visible = config.visible;
        let win = FloatWindow {
            id,
            buf,
            config,
            scroll_offset: 0,
            cached_lines,
            viewport_h: 1,
            last_content: Rect::default(),
            cursor: 0,
            visible,
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
            if let Some(lines) = win.buf.read_if_dirty() {
                win.cached_lines = lines;
                win.bring_cursor_into_view();
            }

            loop {
                match win.cmd_rx.try_recv() {
                    Ok(WinCommand::SetConfig(patch)) => {
                        win.config.apply_patch(patch);
                    }
                    Ok(WinCommand::SetCursor(row)) => {
                        win.set_cursor(row);
                    }
                    Ok(WinCommand::SetVisible(v)) => {
                        win.visible = v;
                    }
                    Ok(WinCommand::Close) | Err(flume::TryRecvError::Disconnected) => {
                        closed_ids.push(win.id);
                        break;
                    }
                    Err(flume::TryRecvError::Empty) => break,
                }
            }
        }

        if !closed_ids.is_empty() {
            self.remove_windows(|w| closed_ids.contains(&w.id));
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
            let _ = win.event_tx.try_send(WinEvent::Key { key: key_str });
        }
        true
    }

    pub fn handle_paste(&self, text: &str) -> bool {
        let Some(fid) = self.focused_id else {
            return false;
        };
        let Some(win) = self.windows.iter().find(|w| w.id == fid) else {
            return false;
        };
        let _ = win.event_tx.try_send(WinEvent::Paste {
            text: text.to_owned(),
        });
        true
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        let mut union = Rect::default();

        for idx in 0..self.windows.len() {
            if self.windows[idx].config.split != Split::None {
                continue;
            }
            let popup = resolve_rect(&self.windows[idx].config, area);
            if popup.width == 0 || popup.height == 0 {
                continue;
            }
            self.render_window(frame, idx, popup);
            union = union_rect(union, popup);
        }

        union
    }

    /// Turns each open split's requested Dimension into a cell count. `carve`
    /// then clamps that against the chat minimum.
    pub fn split_reqs(&self, area: Rect) -> Vec<SplitReq> {
        self.windows
            .iter()
            .filter_map(|w| {
                let split = w.config.split;
                let edge = split.edge()?;
                let extent = match edge.axis {
                    Axis::Vertical => w.config.height.resolve(area.height),
                    Axis::Horizontal => w.config.width.resolve(area.width),
                };
                Some(SplitReq { split, extent })
            })
            .collect()
    }

    /// The layout owns the geometry; we only fill the rect it carved.
    /// render_window records focused_rect for the focused window alone, so a
    /// mouse click never lands on an unfocused split.
    pub fn view_split(&mut self, frame: &mut Frame, dir: Split, rect: Rect) {
        let Some(idx) = self.split_window_idx(dir) else {
            return;
        };
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.render_window(frame, idx, rect);
    }

    pub fn panel_reqs(&self) -> Vec<(usize, u16)> {
        let mut reqs: Vec<(usize, u16)> = self
            .windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.config.split == Split::Panel && w.visible)
            .map(|(i, w)| (i, w.config.height.resolve(100)))
            .collect();
        reqs.sort_by_key(|(i, _)| self.windows[*i].config.order);
        reqs
    }

    pub fn view_panel(&mut self, frame: &mut Frame, idx: usize, rect: Rect) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.render_window(frame, idx, rect);
    }

    fn render_window(&mut self, frame: &mut Frame, idx: usize, popup: Rect) {
        let t = theme::current();
        let win = &mut self.windows[idx];

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
            if !win.config.footer.is_empty() {
                b = b.title_bottom(hint_footer(&win.config.footer).right_aligned());
            }
            b
        } else {
            Block::default().style(ratatui::style::Style::new().bg(t.background))
        };

        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let content_area = inner;

        if win.last_content != content_area {
            let _ = win.event_tx.try_send(WinEvent::Resize {
                width: content_area.width,
                height: content_area.height,
            });
            win.last_content = content_area;
        }

        let layout = win.layout();
        let reserved_top_h = layout.reserved_top as u16;
        let reserved_bot_h = layout.reserved_bot as u16;
        let chrome_h = reserved_top_h + reserved_bot_h;

        let (pinned_top_area, scroll_area, pinned_bot_area) =
            if chrome_h > 0 && content_area.height > chrome_h {
                let top_area = (layout.reserved_top > 0).then_some(Rect {
                    x: content_area.x,
                    y: content_area.y,
                    width: content_area.width,
                    height: reserved_top_h,
                });
                let sa = Rect {
                    x: content_area.x,
                    y: content_area.y + reserved_top_h,
                    width: content_area.width,
                    height: content_area.height - chrome_h,
                };
                let bot_area = (layout.reserved_bot > 0).then_some(Rect {
                    x: content_area.x,
                    y: sa.y + sa.height,
                    width: content_area.width,
                    height: reserved_bot_h,
                });
                (top_area, sa, bot_area)
            } else {
                (None, content_area, None)
            };

        win.refresh_layout(scroll_area.height);
        let top = layout.reserved_top;
        let scrollable = layout.scrollable;

        let vh = win.viewport_h as usize;
        let end = (top + win.scroll_offset + vh).min(top + scrollable);
        let visible = &win.cached_lines[top + win.scroll_offset..end];

        let lines: Vec<Line<'_>> = visible
            .iter()
            .enumerate()
            .map(|(i, sline)| {
                let mut line = snapshot_to_line(sline);
                if win.config.cursor_line && top + win.scroll_offset + i == win.cursor {
                    line = line.style(t.item_selected);
                }
                line
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), scroll_area);

        if let Some(pa) = pinned_top_area {
            let pinned: Vec<Line<'_>> = win.cached_lines[..top]
                .iter()
                .map(snapshot_to_line)
                .collect();
            frame.render_widget(Paragraph::new(pinned), pa);
        }

        if let Some(pa) = pinned_bot_area {
            let pinned: Vec<Line<'_>> = win.cached_lines[top + scrollable..]
                .iter()
                .map(snapshot_to_line)
                .collect();
            frame.render_widget(Paragraph::new(pinned), pa);
        }

        if scrollable as u16 > win.viewport_h {
            render_vertical_scrollbar(
                frame,
                scroll_area,
                scrollable as u16,
                win.scroll_offset as u16,
            );
        }

        if Some(win.id) == self.focused_id {
            self.focused_rect = Some(popup);
        }
    }

    pub fn contains(&self, pos: ratatui::layout::Position) -> bool {
        self.focused_rect.is_some_and(|r| r.contains(pos))
    }

    pub fn scroll(&mut self, delta: i32) {
        let Some(fid) = self.focused_id else {
            return;
        };
        let Some(win) = self.windows.iter_mut().find(|w| w.id == fid) else {
            return;
        };
        win.scroll_by(delta);
    }

    pub fn is_open(&self) -> bool {
        !self.windows.is_empty()
    }

    pub fn close_all(&mut self) {
        self.remove_windows(|_| true);
    }
}

fn hint_footer<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Line<'static> {
    let t = crate::theme::current();
    let mut spans = Vec::with_capacity(pairs.len() * 3);
    for (key, desc) in pairs {
        spans.push(Span::raw(" "));
        for (i, part) in key.as_ref().split('/').enumerate() {
            if i > 0 {
                spans.push(Span::styled("/", t.tool_dim));
            }
            spans.push(Span::styled(part.to_string(), t.keybind_key));
        }
        spans.push(Span::styled(format!(" {}", desc.as_ref()), t.tool_dim));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
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
    scrollable_count: usize,
    viewport_h: u16,
) -> usize {
    let vh = viewport_h as usize;
    if vh == 0 {
        return scroll_offset;
    }
    let max_offset = scrollable_count.saturating_sub(vh);
    let mut offset = scroll_offset.min(max_offset);
    if cursor < offset {
        offset = cursor;
    } else if cursor >= offset + vh {
        offset = cursor + 1 - vh;
    }
    offset
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
        self.focused_id.is_some()
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
    const EXPECT_PASTE_TRUE: &str = "handle_paste should return true when focused";
    const EXPECT_PASTE_FALSE: &str = "handle_paste should return false with no focus";
    const PASTE_TEXT: &str = "hello";

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

    #[test_case(0, 5, 0, 10 => 0 ; "empty_content")]
    #[test_case(3, 5, 10, 0 => 5 ; "zero_viewport_is_noop")]
    #[test_case(2, 5, 20, 5 => 2 ; "cursor_above_viewport")]
    #[test_case(15, 0, 20, 5 => 11 ; "cursor_below_viewport")]
    #[test_case(7, 0, 10, 1 => 7 ; "single_line_viewport")]
    #[test_case(7, 0, 8, 5 => 3 ; "reserved_bottom_limits_max_offset")]
    #[test_case(4, 0, 10, 5 => 0 ; "cursor_exactly_at_viewport_bottom_edge")]
    #[test_case(5, 0, 10, 5 => 1 ; "cursor_one_past_viewport_bottom")]
    #[test_case(0, 0, 3, 10 => 0 ; "content_smaller_than_viewport")]
    #[test_case(0, 99, 5, 3 => 0 ; "scroll_offset_past_max_cursor_pulls_down")]
    fn adjust_scroll_cases(
        cursor: usize,
        scroll: usize,
        scrollable_count: usize,
        vh: u16,
    ) -> usize {
        adjust_scroll(cursor, scroll, scrollable_count, vh)
    }

    #[test_case(0, 0, 0 => (0, 0, 0) ; "empty_lines")]
    #[test_case(2, 3, 10 => (2, 3, 5) ; "both_fit")]
    #[test_case(5, 5, 6 => (1, 5, 0) ; "bottom_wins_when_tight")]
    #[test_case(5, 10, 3 => (0, 3, 0) ; "bottom_caps_at_line_count")]
    #[test_case(0, 0, 7 => (0, 0, 7) ; "no_chrome")]
    fn layout_chrome_cases(top: usize, bot: usize, lines: usize) -> (usize, usize, usize) {
        let l = Layout::new(top, bot, lines);
        (l.reserved_top, l.reserved_bot, l.scrollable)
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

    #[test]
    fn tick_reads_dirty_buf_before_processing_set_cursor() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, cmd_tx) = make_channels();
        let buf = Arc::new(SharedBuf::new());
        mgr.open(buf.clone(), make_config(), true, event_tx, cmd_rx);
        assert_eq!(mgr.windows[0].cached_lines.len(), 0);

        cmd_tx.send(WinCommand::SetCursor(5)).unwrap();
        for i in 0..10 {
            buf.append(make_line(&format!("line{i}")));
        }

        mgr.tick();

        assert_eq!(mgr.windows[0].cached_lines.len(), 10);
        assert_eq!(
            mgr.windows[0].cursor, 5,
            "{EXPECT_CURSOR}: SetCursor must be applied after the dirty buf is consumed",
        );
    }

    #[test]
    fn handle_paste_forwards_event_to_focused_window() {
        let mut mgr = FloatManager::new();
        let (event_rx, _cmd_tx) = open_with_lines(&mut mgr, &["a"]);

        assert!(mgr.handle_paste(PASTE_TEXT), "{EXPECT_PASTE_TRUE}");
        let found = event_rx
            .drain()
            .any(|e| matches!(e, WinEvent::Paste { text } if text == PASTE_TEXT));
        assert!(found, "expected Paste event with matching text");
    }

    #[test]
    fn handle_paste_returns_false_when_empty() {
        let mgr = FloatManager::new();
        assert!(!mgr.handle_paste("x"), "{EXPECT_PASTE_FALSE}");
    }

    #[test]
    fn paste_only_goes_to_focused_window() {
        let mut mgr = FloatManager::new();

        let (tx1, rx1, erx1, _) = make_channels();
        mgr.open(make_buf(&["a"]), make_config(), true, tx1, rx1);

        let (tx2, rx2, erx2, _) = make_channels();
        mgr.open(make_buf(&["b"]), make_config(), true, tx2, rx2);

        assert_eq!(mgr.focused_id, Some(1), "latest focused window");
        assert!(mgr.handle_paste(PASTE_TEXT), "{EXPECT_PASTE_TRUE}");

        let win1_pastes: Vec<_> = erx1
            .drain()
            .filter(|e| matches!(e, WinEvent::Paste { .. }))
            .collect();
        let win2_pastes: Vec<_> = erx2
            .drain()
            .filter(|e| matches!(e, WinEvent::Paste { .. }))
            .collect();
        assert!(win1_pastes.is_empty(), "unfocused window gets nothing");
        assert_eq!(win2_pastes.len(), 1, "only focused window gets the paste");
    }

    #[test]
    fn reserved_top_clamps_cursor_down() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, cmd_tx) = make_channels();
        let buf = make_buf(&["a", "b", "c", "d", "e"]);
        let mut cfg = make_config();
        cfg.reserved_top = 2;
        mgr.open(buf, cfg, true, event_tx, cmd_rx);

        cmd_tx.send(WinCommand::SetCursor(0)).unwrap();
        mgr.tick();
        assert_eq!(
            mgr.windows[0].cursor, 2,
            "{EXPECT_CURSOR}: cursor cannot enter reserved top rows",
        );
    }

    #[test]
    fn reserved_top_and_bottom_leave_single_scrollable_row() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, cmd_tx) = make_channels();
        let buf = make_buf(&["a", "b", "c", "d", "e"]);
        let mut cfg = make_config();
        cfg.reserved_top = 2;
        cfg.reserved_bottom = 2;
        mgr.open(buf, cfg, true, event_tx, cmd_rx);

        cmd_tx.send(WinCommand::SetCursor(99)).unwrap();
        mgr.tick();
        assert_eq!(
            mgr.windows[0].cursor, 2,
            "{EXPECT_CURSOR}: only row 2 is scrollable",
        );
    }

    #[test]
    fn reserved_top_yields_when_bottom_exceeds_content() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = make_buf(&["a", "b", "c", "d", "e"]);
        let mut cfg = make_config();
        cfg.reserved_top = 10;
        cfg.reserved_bottom = 10;
        mgr.open(buf, cfg, true, event_tx, cmd_rx);

        let layout = mgr.windows[0].layout();
        assert_eq!(
            layout.reserved_top, 0,
            "top yields when bottom consumes everything"
        );
        assert_eq!(layout.reserved_bot, 5);
        assert_eq!(layout.scrollable, 0);
    }

    #[test]
    fn scroll_clamps_at_max_offset_with_reserved_bottom() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let lines: Vec<String> = (0..10).map(|i| format!("line{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let buf = make_buf(&refs);
        let mut cfg = make_config();
        cfg.reserved_bottom = 3;
        mgr.open(buf, cfg, true, event_tx, cmd_rx);

        mgr.scroll(-1000);

        let win = &mgr.windows[0];
        let expected_max = win.layout().max_offset(win.viewport_h);
        assert_eq!(
            win.scroll_offset, expected_max,
            "scroll_offset must clamp at scrollable - viewport_h",
        );
    }

    #[test]
    fn tick_consumes_all_appends_accumulated_between_ticks() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = make_buf(&["initial"]);
        mgr.open(buf.clone(), make_config(), true, event_tx, cmd_rx);
        assert_eq!(mgr.windows[0].cached_lines.len(), 1);

        buf.append(make_line("second"));
        buf.append(make_line("third"));
        mgr.tick();

        assert_eq!(
            mgr.windows[0].cached_lines.len(),
            3,
            "all appends since last read must be visible after one tick",
        );
    }

    #[test]
    fn close_before_set_cursor_in_same_tick_is_safe() {
        let mut mgr = FloatManager::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a", "b", "c"]);

        cmd_tx.send(WinCommand::Close).unwrap();
        cmd_tx.send(WinCommand::SetCursor(2)).unwrap();
        mgr.tick();

        assert!(!mgr.is_open(), "{EXPECT_CLOSED}");
    }

    #[test]
    fn close_all_is_idempotent() {
        let mut mgr = FloatManager::new();
        let (_event_rx, _cmd_tx) = open_with_lines(&mut mgr, &["a"]);

        mgr.close_all();
        mgr.close_all();
        assert!(!mgr.is_open(), "{EXPECT_CLOSED}");
        assert_eq!(mgr.focused_id, None);
    }

    #[test]
    fn handle_key_after_focused_window_closed_returns_false() {
        let mut mgr = FloatManager::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a"]);

        cmd_tx.send(WinCommand::Close).unwrap();
        mgr.tick();

        let key_event = KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert!(
            !mgr.handle_key(key_event),
            "no windows remain, so handle_key must return false",
        );
    }

    #[test]
    fn drop_sends_close_to_all_windows() {
        let (tx1, rx1, erx1, _cmd_tx1) = make_channels();
        let (tx2, rx2, erx2, _cmd_tx2) = make_channels();
        {
            let mut mgr = FloatManager::new();
            mgr.open(make_buf(&["a"]), make_config(), true, tx1, rx1);
            mgr.open(make_buf(&["b"]), make_config(), true, tx2, rx2);
        }

        assert!(
            erx1.drain().any(|e| matches!(e, WinEvent::Close)),
            "Drop must send Close to window 1",
        );
        assert!(
            erx2.drain().any(|e| matches!(e, WinEvent::Close)),
            "Drop must send Close to window 2",
        );
    }

    const SCROLL_PRESERVED: &str = "refresh_layout must not pull offset toward cursor";
    const CURSOR_VISIBLE: &str = "cursor must be inside the viewport";
    const OFFSET_IN_RANGE: &str = "scroll_offset must be <= max_offset";

    fn make_window_n(line_count: usize) -> FloatWindow {
        let (event_tx, _event_rx) = flume::bounded::<WinEvent>(8);
        let (_cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);
        let lines: Vec<String> = (0..line_count).map(|i| format!("l{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let buf = make_buf(&refs);
        let cached_lines = buf.read_if_dirty().unwrap_or_default();
        FloatWindow {
            id: 0,
            buf,
            config: make_config(),
            scroll_offset: 0,
            cached_lines,
            viewport_h: 1,
            last_content: Rect::default(),
            cursor: 0,
            visible: true,
            event_tx,
            cmd_rx,
        }
    }

    fn assert_invariants(win: &FloatWindow) {
        if !win.cached_lines.is_empty() {
            assert!(
                win.cursor < win.cached_lines.len(),
                "cursor {} out of bounds for {} lines",
                win.cursor,
                win.cached_lines.len(),
            );
        }
        let max_offset = win.layout().max_offset(win.viewport_h);
        assert!(
            win.scroll_offset <= max_offset,
            "{OFFSET_IN_RANGE}: got {} > max {max_offset}",
            win.scroll_offset,
        );
    }

    fn assert_cursor_visible(win: &FloatWindow) {
        let lo = win.layout().reserved_top + win.scroll_offset;
        let hi = lo + win.viewport_h as usize;
        assert!(
            win.cursor >= lo && win.cursor < hi,
            "{CURSOR_VISIBLE}: cursor {} not in [{lo}, {hi})",
            win.cursor,
        );
    }

    /// Regression: `view()` used to re-snap the offset toward the cursor on
    /// every frame, so wheel scrolls were silently undone before the next
    /// paint.
    #[test_case(-3 ; "small_delta")]
    #[test_case(-7 ; "large_delta")]
    fn scroll_by_persists_across_refresh_layout(delta: i32) {
        let mut win = make_window_n(20);
        win.refresh_layout(5);
        win.scroll_by(delta);
        let after_scroll = win.scroll_offset;
        assert_eq!(win.cursor, 0, "cursor stayed put");

        win.refresh_layout(5);
        assert_eq!(win.scroll_offset, after_scroll, "{SCROLL_PRESERVED}");
    }

    #[test]
    fn set_cursor_brings_cursor_into_view() {
        let mut win = make_window_n(20);
        win.refresh_layout(5);
        win.set_cursor(19);
        assert_cursor_visible(&win);
    }

    #[test]
    fn bring_cursor_into_view_after_content_grows() {
        let mut win = make_window_n(3);
        win.refresh_layout(3);
        win.set_cursor(2);

        win.cached_lines = Arc::new((0..30).map(|i| make_line(&format!("l{i}"))).collect());
        win.bring_cursor_into_view();

        assert_cursor_visible(&win);
        assert_invariants(&win);
    }

    #[test]
    fn refresh_layout_clamps_when_viewport_grows() {
        let mut win = make_window_n(10);
        win.refresh_layout(3);
        win.scroll_by(-7);
        assert_eq!(win.scroll_offset, 7);

        win.refresh_layout(8);
        assert_eq!(win.scroll_offset, 2, "{OFFSET_IN_RANGE}");
    }

    #[test_case(0, 0, 5 => 0 ; "zero_delta")]
    #[test_case(3, 2, 5 => 1 ; "positive_delta_scrolls_up")]
    #[test_case(1, -2, 5 => 3 ; "negative_delta_scrolls_down")]
    #[test_case(1, 99, 5 => 0 ; "overshoot_up_clamps_to_zero")]
    #[test_case(1, -99, 5 => 5 ; "overshoot_down_clamps_to_max")]
    #[test_case(0, -3, 0 => 0 ; "no_room_to_scroll")]
    fn scroll_by_clamps_at_bounds(initial_offset: usize, delta: i32, max_offset: usize) -> usize {
        let mut win = make_window_n(max_offset + 1);
        win.refresh_layout(1);
        win.scroll_offset = initial_offset;
        win.scroll_by(delta);
        win.scroll_offset
    }

    #[test]
    fn invariants_hold_across_action_sequence() {
        let mut win = make_window_n(20);
        win.refresh_layout(4);

        for op in [
            &|w: &mut FloatWindow| w.scroll_by(-5) as _,
            &|w| w.set_cursor(15),
            &|w| {
                w.refresh_layout(8);
            },
            &|w| w.scroll_by(-100),
            &|w| w.set_cursor(0),
            &|w| {
                w.refresh_layout(2);
            },
            &|w| w.scroll_by(100),
            &|w| {
                w.refresh_layout(30);
            },
            &|w| w.set_cursor(19),
            &|w| w.scroll_by(-3),
        ] as [&dyn Fn(&mut FloatWindow); 10]
        {
            op(&mut win);
            assert_invariants(&win);
        }
    }

    const EXPECT_NO_REQS: &str = "expected no split reqs without a split window";
    const EXPECT_SINGLE_SPLIT: &str = "expected exactly one same-direction window after re-open";
    const EXPECT_SPLIT_DRAWN: &str = "expected the split window to receive its layout rect";

    fn split_config(dir: Split, extent: Dimension) -> FloatConfig {
        FloatConfig {
            width: extent,
            height: extent,
            border: Border::None,
            split: dir,
            ..FloatConfig::default()
        }
    }

    fn open_split(mgr: &mut FloatManager, dir: Split, extent: u16, focus: bool) -> SplitChannels {
        let (event_tx, cmd_rx, event_rx, cmd_tx) = make_channels();
        mgr.open(
            make_buf(&["split"]),
            split_config(dir, Dimension::Abs(extent)),
            focus,
            event_tx,
            cmd_rx,
        );
        (event_rx, cmd_tx)
    }

    type SplitChannels = (flume::Receiver<WinEvent>, flume::Sender<WinCommand>);

    fn render_into(
        mgr: &mut FloatManager,
        area: Rect,
        f: impl FnOnce(&mut FloatManager, &mut Frame),
    ) {
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| f(mgr, frame)).unwrap();
    }

    #[test]
    fn split_reqs_empty_without_split_window() {
        let mut mgr = FloatManager::new();
        open_with_lines(&mut mgr, &["a"]);
        let area = Rect::new(0, 0, 80, 40);
        assert!(mgr.split_reqs(area).is_empty(), "{EXPECT_NO_REQS}");
    }

    #[test_case(Split::Below, 10 ; "vertical_uses_height")]
    #[test_case(Split::Left, 30 ; "horizontal_uses_width")]
    fn split_reqs_resolves_extent_per_axis(dir: Split, extent: u16) {
        let mut mgr = FloatManager::new();
        let _ = open_split(&mut mgr, dir, extent, true);
        let area = Rect::new(0, 0, 80, 40);
        let reqs = mgr.split_reqs(area);
        assert_eq!(reqs, vec![SplitReq { split: dir, extent }]);
    }

    #[test]
    fn view_skips_split_window() {
        let mut mgr = FloatManager::new();
        let (event_rx, _ctx) = open_split(&mut mgr, Split::Below, 10, true);
        let area = Rect::new(0, 0, 80, 40);
        render_into(&mut mgr, area, |m, f| {
            let u = m.view(f, area);
            assert_eq!(u, Rect::default(), "overlay pass must not draw the split");
        });
        assert!(
            !event_rx
                .drain()
                .any(|e| matches!(e, WinEvent::Resize { .. })),
            "{EXPECT_SPLIT_DRAWN}: overlay pass must leave it undrawn",
        );
    }

    #[test]
    fn view_split_draws_into_given_rect() {
        let mut mgr = FloatManager::new();
        let dir = Split::Below;
        let rect = Rect::new(0, 30, 80, 10);
        let (event_rx, _ctx) = open_split(&mut mgr, dir, 10, true);
        let area = Rect::new(0, 0, 80, 40);
        render_into(&mut mgr, area, |m, f| m.view_split(f, dir, rect));

        let resize = event_rx
            .drain()
            .find_map(|e| match e {
                WinEvent::Resize { width, height } => Some((width, height)),
                _ => None,
            })
            .expect(EXPECT_SPLIT_DRAWN);
        assert_eq!(resize, (rect.width, rect.height), "{EXPECT_SPLIT_DRAWN}");
        assert!(
            mgr.contains(ratatui::layout::Position::new(rect.x + 1, rect.y + 1)),
            "scroll/contains must target the carved area",
        );
    }

    #[test]
    fn second_split_of_same_direction_replaces_first() {
        let mut mgr = FloatManager::new();
        let dir = Split::Below;
        let (erx1, _ctx1) = open_split(&mut mgr, dir, 5, true);
        let _ = open_split(&mut mgr, dir, 5, true);

        let split_count = mgr.windows.iter().filter(|w| w.config.split == dir).count();
        assert_eq!(split_count, 1, "{EXPECT_SINGLE_SPLIT}");
        assert!(
            erx1.drain().any(|e| matches!(e, WinEvent::Close)),
            "the replaced split must receive a Close event",
        );
    }

    #[test]
    fn splits_of_different_directions_coexist() {
        let mut mgr = FloatManager::new();
        let (erx_left, _) = open_split(&mut mgr, Split::Left, 20, true);
        let (erx_below, _) = open_split(&mut mgr, Split::Below, 10, true);

        assert!(
            mgr.split_window_idx(Split::Left).is_some(),
            "left split must survive opening a below split",
        );
        assert!(mgr.split_window_idx(Split::Below).is_some());
        assert!(
            !erx_left.drain().any(|e| matches!(e, WinEvent::Close)),
            "a different-direction split must not evict the left split",
        );
        let _ = erx_below;
    }

    const EXPECT_UNFOCUSED_NO_RECT: &str =
        "an unfocused split must not claim focused_rect (mouse hit-testing target)";
    const EXPECT_FOCUS_RECOVERS: &str =
        "removing the focused window must hand focus to a surviving window";

    #[test]
    fn unfocused_split_does_not_claim_focused_rect() {
        let mut mgr = FloatManager::new();
        let _ = open_split(&mut mgr, Split::Below, 10, false);
        let area = Rect::new(0, 0, 80, 40);
        let rect = Rect::new(0, 30, 80, 10);
        render_into(&mut mgr, area, |m, f| m.view_split(f, Split::Below, rect));
        assert!(
            !mgr.contains(ratatui::layout::Position::new(rect.x + 1, rect.y + 1)),
            "{EXPECT_UNFOCUSED_NO_RECT}",
        );
    }

    #[test]
    fn removing_focused_window_recovers_focus_to_survivor() {
        let mut mgr = FloatManager::new();
        let (tx1, rx1, _erx1, _ctx1) = make_channels();
        mgr.open(make_buf(&["a"]), FloatConfig::default(), false, tx1, rx1);
        let survivor = mgr.windows[0].id;

        let _ = open_split(&mut mgr, Split::Below, 5, true);

        mgr.remove_windows(|w| w.config.split == Split::Below);
        assert_eq!(mgr.focused_id, Some(survivor), "{EXPECT_FOCUS_RECOVERS}");
    }

    const EXPECT_ZERO_RECT_NOOP: &str =
        "a zero-size rect must skip drawing: no Resize, no focused_rect";
    const EXPECT_CLOSE_TO_SPLIT: &str = "close_all must send Close to the split window";

    #[test_case(Rect::new(0, 30, 80, 0) ; "zero_height")]
    #[test_case(Rect::new(0, 30, 0, 10) ; "zero_width")]
    fn view_split_zero_size_rect_is_noop(rect: Rect) {
        let mut mgr = FloatManager::new();
        let (event_rx, _ctx) = open_split(&mut mgr, Split::Below, 10, true);
        let area = Rect::new(0, 0, 80, 40);
        render_into(&mut mgr, area, |m, f| m.view_split(f, Split::Below, rect));

        assert!(
            !event_rx
                .drain()
                .any(|e| matches!(e, WinEvent::Resize { .. })),
            "{EXPECT_ZERO_RECT_NOOP}",
        );
        assert!(
            !mgr.contains(ratatui::layout::Position::new(rect.x, rect.y)),
            "{EXPECT_ZERO_RECT_NOOP}",
        );
    }

    #[test]
    fn view_overlays_float_and_skips_coexisting_split() {
        let mut mgr = FloatManager::new();
        let (ftx, frx, ferx, _fctx) = make_channels();
        mgr.open(
            make_buf(&["float"]),
            FloatConfig {
                width: Dimension::Abs(20),
                height: Dimension::Abs(10),
                ..FloatConfig::default()
            },
            true,
            ftx,
            frx,
        );

        let (serx, _sctx) = open_split(&mut mgr, Split::Below, 10, false);

        let area = Rect::new(0, 0, 80, 40);
        render_into(&mut mgr, area, |m, f| {
            let u = m.view(f, area);
            assert_ne!(u, Rect::default(), "overlay pass must draw the float");
        });

        assert!(
            ferx.drain().any(|e| matches!(e, WinEvent::Resize { .. })),
            "the float must be drawn by the overlay pass",
        );
        assert!(
            !serx.drain().any(|e| matches!(e, WinEvent::Resize { .. })),
            "{EXPECT_SPLIT_DRAWN}: overlay pass must skip the split",
        );

        let rect = Rect::new(0, 30, 80, 10);
        render_into(&mut mgr, area, |m, f| m.view_split(f, Split::Below, rect));
        assert!(
            serx.drain().any(|e| matches!(e, WinEvent::Resize { .. })),
            "{EXPECT_SPLIT_DRAWN}: split joins layout via view_split",
        );
    }

    #[test]
    fn close_all_notifies_split_window() {
        let mut mgr = FloatManager::new();
        let (event_rx, _ctx) = open_split(&mut mgr, Split::Below, 10, true);

        mgr.close_all();
        assert!(!mgr.is_open(), "{EXPECT_CLOSED}");
        assert!(
            event_rx.drain().any(|e| matches!(e, WinEvent::Close)),
            "{EXPECT_CLOSE_TO_SPLIT}",
        );
    }
}
