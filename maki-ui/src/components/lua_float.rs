use std::sync::Arc;

use crossterm::event::KeyEvent;
use maki_agent::{SharedBuf, SnapshotLine, SpanStyle};
use maki_lua::{Anchor, Axis, Border, FloatConfig, Key, Split, TitlePos, WinCommand, WinEvent};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::animation::{animation_elapsed_ms, spinner_str};
use crate::components::split_layout::SplitReq;
use crate::components::{
    Overlay,
    scrollbar::render_vertical_scrollbar,
    tool_display::{SPINNER_STYLE_NAME, SPINNER_STYLE_PREFIX, resolve_span_style},
};
use crate::repaint::{Cadence, Dirty};
use crate::theme;

/// Blank rows kept between two windows of the same stack.
const STACK_GAP: u16 = 1;
/// Cells a border takes from a row, one at each end.
const BORDER_CELLS: u16 = 2;

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
    /// Whether the window asked for focus when it opened. A window that did
    /// not is never handed focus later either: it is up while the user types
    /// somewhere else, and focusing it would turn it into a key sink.
    opened_focused: bool,
    /// Set by [`render_window`] while the frame paints this window into a
    /// rect with cells in it, and moved to `on_screen` when the frame ends.
    painting: bool,
    /// Whether the last frame really put this window on screen. `visible` is
    /// the plugin's own switch and says nothing about geometry: a
    /// plugin-supplied width or height of zero paints nothing, and a claim on
    /// a window nobody can see is a key taken from the user with no footer to
    /// tell them where it went.
    on_screen: bool,
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

    /// The windows this frame lays out. A hidden one takes no cells and is
    /// never painted, so `on_screen` clears at the end of the frame and its
    /// keys stop being claimed too.
    ///
    /// Commands, ticks and events do not go through here. A plugin can keep
    /// working on a window nobody can see.
    ///
    /// Every layout pass should use this. When each pass checked `visible` on
    /// its own, two of them forgot and a hidden split kept its cells.
    fn laid_out(&self) -> impl Iterator<Item = (usize, &FloatWindow)> {
        self.windows.iter().enumerate().filter(|(_, w)| w.visible)
    }

    fn split_window_idx(&self, dir: Split) -> Option<usize> {
        self.laid_out()
            .find(|(_, w)| w.config.split == dir)
            .map(|(i, _)| i)
    }

    /// The one path windows take to leave the manager. Routing every removal
    /// here is what keeps the close event, the window list, and `focused_id`
    /// from ever drifting apart.
    ///
    /// Focus given up by a closing window goes to the topmost window that
    /// asked for focus when it opened, and to nothing if there is none. A
    /// window opened `focus = false` is up while the user works somewhere
    /// else: handing it the keyboard would turn a popup that takes five keys
    /// into one that takes every key and drops the rest.
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
            self.focused_id = self
                .windows
                .iter()
                .rev()
                .find(|w| w.opened_focused && w.config.split != Split::Panel)
                .map(|w| w.id);
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
            opened_focused: focus,
            painting: false,
            on_screen: false,
            event_tx,
            cmd_rx,
        };

        self.windows.push(win);
        self.windows.sort_by_key(|w| w.config.zindex);

        if focus {
            self.focused_id = Some(id);
        }
    }

    /// Runs for backgrounded sessions too, or a plugin writing to a window
    /// nobody is looking at would lose its output.
    pub fn tick(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        for win in &mut self.windows {
            if let Some(lines) = win.buf.read_if_dirty() {
                win.cached_lines = lines;
                win.bring_cursor_into_view();
                dirty = Dirty::YES;
            }
        }
        dirty | self.drain_commands()
    }

    /// Applies what the plugins asked of their windows since the last look and
    /// removes the ones that asked to close.
    ///
    /// Run before key dispatch as well as on tick: a plugin closes its window
    /// on the Lua thread, and until that command is drained the window is
    /// still here, still claiming its keys, and the next key would be taken
    /// from the user by a window that is on its way out and hands it to a loop
    /// that has already stopped reading.
    ///
    /// A patch that carried a `zindex` re-sorts the list, because z-order is
    /// what decides both who is drawn in front and who answers a claimed key:
    /// a window raised over another and left where it was opened would be drawn
    /// in front while the one underneath went on taking the key. The sort is
    /// stable, so windows sharing a `zindex` keep open order.
    fn drain_commands(&mut self) -> Dirty {
        let mut closed_ids = Vec::new();
        let mut dirty = Dirty::NO;
        let mut restack = false;

        for win in &mut self.windows {
            loop {
                match win.cmd_rx.try_recv() {
                    Ok(WinCommand::SetConfig(patch)) => {
                        restack |= patch.zindex.is_some();
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
                dirty = Dirty::YES;
            }
        }

        if restack {
            self.windows.sort_by_key(|w| w.config.zindex);
        }

        if !closed_ids.is_empty() {
            self.remove_windows(|w| closed_ids.contains(&w.id));
            dirty = Dirty::YES;
        }
        dirty
    }

    /// Float snapshots bake spinner spans at render time, so an open float has
    /// to keep painting for plugin spinners to turn.
    pub fn cadence(&self) -> Cadence {
        Cadence::when(self.is_open(), Cadence::SPINNER)
    }

    /// Whether a window on screen is waiting on the user. It checks `visible`
    /// itself instead of using [`Self::laid_out`], because this is about who
    /// can answer, not about layout. Nobody can answer a window they cannot see.
    pub fn needs_input(&self) -> bool {
        self.windows
            .iter()
            .any(|win| win.visible && win.config.needs_input)
    }

    /// The focused window is handed every key, ahead of every overlay the host
    /// owns: it is the thing the user is looking at and typing into.
    ///
    /// A press no notation names, like `Super+Enter`, is still spent here with
    /// nothing sent. Letting it through would run a built-in binding on the
    /// chat hidden behind the window.
    pub fn handle_focused_key(&self, key: KeyEvent) -> bool {
        let Some(win) = self
            .focused_id
            .and_then(|fid| self.windows.iter().find(|w| w.id == fid))
        else {
            return false;
        };
        if let Some(key) = Key::from_event(key) {
            send_key(win, key);
        }
        true
    }

    /// The keys an unfocused window declared at open, which it takes only
    /// after every overlay the host owns has passed: a claim is up while the
    /// user goes on working under it, so it must not outrank a modal opened
    /// over it. A key it does claim is consumed here and never also reaches
    /// the chat input under it, a plugin binding, or a built-in key.
    ///
    /// Nothing has to be released. The claim list lives on the window, so it
    /// goes when the window does, through the one removal path, and a list can
    /// never outlive what the user can see.
    pub fn handle_claimed_key(&mut self, key: KeyEvent) -> bool {
        // The frame this key was pressed in repaints whatever this drops, so
        // the debt is already owed and there is nothing to report.
        let _ = self.drain_commands();
        let Some(key) = Key::from_event(key) else {
            return false;
        };
        let Some(win) = self.claimant(key) else {
            return false;
        };
        send_key(win, key);
        true
    }

    /// The topmost window on screen claiming {key}. `windows` is sorted by
    /// `zindex`, so walking it backwards is the order the user sees, front
    /// first, and a popup opened over a popup answers the key.
    ///
    /// Being on screen is the whole gate, and it means the last frame painted
    /// this window into a rect with cells in it. A window sized to nothing, one
    /// that has not painted yet and a float the plugin hid all paint nothing,
    /// so all three advertise nothing. Otherwise a window nobody can see would
    /// hold keys for the rest of the run while drawing no footer to say so, and
    /// unlike a `maki.keymap.set` binding a claim appears in no list the user
    /// can read.
    fn claimant(&self, key: Key) -> Option<&FloatWindow> {
        self.windows
            .iter()
            .rev()
            .find(|w| w.on_screen && w.config.keys.contains(&key))
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

    /// A stacked window sits below (or above, for the south anchors and a
    /// caret the host placed above) every stacked window laid out before it in
    /// the same corner, so its offset can only be known here, where the whole
    /// list is in scope. Recomputing it each frame is what makes survivors
    /// close the hole left by a window that went away, with nothing to keep
    /// in sync.
    ///
    /// The rows summed are the ones each earlier window really takes: a caret
    /// anchor trims its height to the side it landed on, and summing the
    /// request would leave a gap as tall as the rows that were cut.
    fn stack_offset(&self, idx: usize, area: Rect, caret: Option<Position>) -> u16 {
        let win = &self.windows[idx];
        if !stacks(win) {
            return 0;
        }
        self.laid_out()
            .map(|(_, w)| w)
            .filter(|w| stacks(w) && w.config.anchor == win.config.anchor && w.id < win.id)
            .fold(0, |acc, w| {
                acc.saturating_add(effective_height(&w.config, area, caret))
                    .saturating_add(STACK_GAP)
            })
    }

    /// {caret} is the cell the frame being painted put the chat input caret
    /// on, so an [`Anchor::InputCaret`] window follows it through wraps and
    /// resizes with nobody re-placing it.
    ///
    /// The last float pass of the frame, so it is also where the frame's
    /// painting is settled: the splits and panels drawn earlier have already
    /// marked themselves, and what every window did this frame becomes what it
    /// did on the last one, which is what a claim is weighed against.
    pub fn view(&mut self, frame: &mut Frame, area: Rect, caret: Option<Position>) -> Rect {
        let floats: Vec<usize> = self
            .laid_out()
            .filter(|(_, w)| w.config.split == Split::None)
            .map(|(i, _)| i)
            .collect();
        let mut union = Rect::default();

        for idx in floats {
            let popup = resolve_rect(
                &self.windows[idx].config,
                area,
                self.stack_offset(idx, area, caret),
                caret,
            );
            if popup.width == 0 || popup.height == 0 {
                continue;
            }
            self.render_window(frame, idx, popup);
            union = union_rect(union, popup);
        }

        for win in &mut self.windows {
            win.on_screen = std::mem::take(&mut win.painting);
        }

        union
    }

    /// Turns each split this frame lays out into a cell count. `carve` then
    /// clamps that against the chat minimum.
    pub fn split_reqs(&self, area: Rect) -> Vec<SplitReq> {
        self.laid_out()
            .filter_map(|(_, w)| {
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
            .laid_out()
            .filter(|(_, w)| w.config.split == Split::Panel)
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

    /// Every caller resolves {popup} first and skips a window it left with no
    /// cells, so reaching here is what puts a window on screen this frame.
    fn render_window(&mut self, frame: &mut Frame, idx: usize, popup: Rect) {
        let t = theme::current();
        let win = &mut self.windows[idx];
        win.painting = true;

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
                scrollable as u32,
                win.scroll_offset as u32,
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

fn send_key(win: &FloatWindow, key: Key) {
    let _ = win.event_tx.try_send(WinEvent::Key { key });
}

fn stacks(win: &FloatWindow) -> bool {
    win.config.stack && win.config.split == Split::None
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

/// Sits a window of {w} by {h} on the caret cell, and reports whether it
/// landed above the caret.
///
/// The roomier side of the caret wins: a caret near the top of the screen has
/// two rows above it and the whole transcript below. The height is trimmed to
/// that side, and the column pulled left far enough that the whole width lands
/// on screen.
///
/// A stack of these windows grows along the side reported, away from the
/// caret, so the second window clears the first instead of landing back on the
/// input box both were placed off.
fn caret_rect(caret: Position, w: u16, h: u16, area: Rect) -> (Rect, bool) {
    let above = caret.y.saturating_sub(area.y);
    let below = (area.y + area.height).saturating_sub(caret.y + 1);
    let height = h.min(above.max(below));
    let sits_above = above >= below;
    let y = if sits_above {
        caret.y - height
    } else {
        caret.y + 1
    };
    let x = caret.x.clamp(area.x, area.x + area.width - w);
    (Rect::new(x, y, w, height), sits_above)
}

/// The rows {config} really takes on this frame, which is its request for
/// every anchor but [`Anchor::InputCaret`]: that one is trimmed to the side
/// of the caret it sits on.
fn effective_height(config: &FloatConfig, area: Rect, caret: Option<Position>) -> u16 {
    let h = config.height.resolve(area.height).min(area.height);
    match caret {
        Some(caret) if config.anchor == Anchor::InputCaret => {
            caret_rect(caret, float_width(config, area), h, area)
                .0
                .height
        }
        _ => h,
    }
}

/// Widened to fit the title and footer. The footer is often the only place a
/// popup lists its keys, and a key cut off there is one the user never learns.
/// Doing it here also spares every plugin from measuring our footer layout.
fn float_width(config: &FloatConfig, area: Rect) -> u16 {
    config
        .width
        .resolve(area.width)
        .max(chrome_width(config))
        .min(area.width)
}

/// Title and footer are drawn on the border, so a borderless window has none.
fn chrome_width(config: &FloatConfig) -> u16 {
    if config.border == Border::None {
        return 0;
    }
    let footer = match config.footer.is_empty() {
        true => 0,
        false => hint_footer(&config.footer).width(),
    };
    let widest = config.title.as_str().width().max(footer);
    u16::try_from(widest)
        .unwrap_or(u16::MAX)
        .saturating_add(BORDER_CELLS)
}

/// Widened to `i32` before the shift: a column near `u16::MAX` plus a
/// positive {delta} overflows an `i16` and panics in debug, and the clamp
/// that follows brings the result back into `u16` anyway.
fn shift(coord: u16, delta: i16, lo: u16, hi: u16) -> u16 {
    (i32::from(coord) + i32::from(delta)).clamp(i32::from(lo), i32::from(hi)) as u16
}

/// `stack_offset` slides the window along the anchor's vertical direction
/// after `config.row` has been applied, so `row` stays the point the stack
/// grows from. Stacks grow downwards except from the southern anchors and from
/// an [`Anchor::InputCaret`] the host put above the caret: stacking downwards
/// from there would walk the next window back over the caret and into the
/// input box.
///
/// {caret} is where this frame put the chat input caret. Without one,
/// [`Anchor::InputCaret`] falls back to the centred default, because a form, a
/// prompt or a `below` split takes the input box away often enough that
/// erroring would make the anchor unusable.
///
/// `row` and `col` shift the window off whatever origin its anchor picked. For
/// the caret that origin is the corner beside it, or the centre of the screen
/// when there is no caret.
fn resolve_rect(
    config: &FloatConfig,
    area: Rect,
    stack_offset: u16,
    caret: Option<Position>,
) -> Rect {
    let w = float_width(config, area);
    let h = config.height.resolve(area.height).min(area.height);
    let (left, top) = (area.x, area.y);
    let (right, bottom) = (area.x + area.width, area.y + area.height);
    let centred = || {
        (
            left + area.width.saturating_sub(w) / 2,
            top + area.height.saturating_sub(h) / 2,
        )
    };

    let placed = match caret {
        Some(caret) if config.anchor == Anchor::InputCaret => Some(caret_rect(caret, w, h, area)),
        _ => None,
    };
    let stacks_up = match placed {
        Some((_, sits_above)) => sits_above,
        None => matches!(config.anchor, Anchor::SW | Anchor::SE),
    };

    let (x, y, h) = if config.anchor == Anchor::InputCaret {
        let (origin, h) = match placed {
            Some((rect, _)) => ((rect.x, rect.y), rect.height),
            None => (centred(), h),
        };
        (
            shift(origin.0, config.col.unwrap_or(0), left, right),
            shift(origin.1, config.row.unwrap_or(0), top, bottom),
            h,
        )
    } else if config.col.is_none() && config.row.is_none() {
        let (x, y) = centred();
        (x, y, h)
    } else {
        let (c, r) = (config.col.unwrap_or(0), config.row.unwrap_or(0));
        let x = match config.anchor {
            Anchor::NE | Anchor::SE => shift(right - w, c, left, right),
            _ => shift(left, c, left, right),
        };
        let y = match config.anchor {
            Anchor::SW | Anchor::SE => shift(bottom - h, r, top, bottom),
            _ => shift(top, r, top, bottom),
        };
        (x, y, h)
    };

    let y = if stacks_up {
        y.saturating_sub(stack_offset)
    } else {
        y.saturating_add(stack_offset)
    }
    .clamp(top, bottom);

    Rect::new(x, y, w.min(right - x), h.min(bottom - y))
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

/// Same convention as tool snapshots: spinner-named spans bake to the live
/// animation frame, so plugins animate without redrawing (floats already
/// repaint every tick while open). `"spinner:<style>"` takes `<style>`, so
/// rows can keep the glyph on e.g. their selection background.
fn snapshot_to_line(sline: &SnapshotLine) -> Line<'_> {
    Line::from(
        sline
            .spans
            .iter()
            .map(|span| match &span.style {
                SpanStyle::Named(n)
                    if n == SPINNER_STYLE_NAME || n.starts_with(SPINNER_STYLE_PREFIX) =>
                {
                    Span::styled(
                        spinner_str(animation_elapsed_ms()),
                        theme::style_by_name(n.strip_prefix(SPINNER_STYLE_PREFIX).unwrap_or(n)),
                    )
                }
                style => Span::styled(span.text.clone(), resolve_span_style(style)),
            })
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

    fn is_modal(&self) -> bool {
        self.focused_id
            .and_then(|id| self.windows.iter().find(|win| win.id == id))
            .is_some_and(|win| win.config.split == Split::None)
    }

    fn close(&mut self) {
        self.close_all();
    }

    fn cadence(&self) -> Cadence {
        self.cadence()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repaint::expect::{OWED, QUIET};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use maki_agent::SnapshotSpan;
    use maki_lua::{Dimension, FloatConfigPatch};
    use test_case::test_case;

    const EXPECT_OPEN: &str = "expected manager to have open windows";
    const EXPECT_CLOSED: &str = "expected manager to have no open windows";
    const EXPECT_CURSOR: &str = "unexpected cursor position";
    const EXPECT_PASTE_TRUE: &str = "handle_paste should return true when focused";
    const EXPECT_PASTE_FALSE: &str = "handle_paste should return false with no focus";
    const PASTE_TEXT: &str = "hello";
    const CLAIM_NOT_DELIVERED: &str = "the window a key was claimed for never got it";
    const CLAIM_LEAKED: &str = "a key nobody claimed was taken from what is underneath";
    const EXPECT_PAINTED: &str = "a float with cells to fill must be on screen after a frame";
    const EXPECT_HIDDEN_UNPAINTED: &str =
        "a hidden float must be off the screen, footer, claims and all";
    const EXPECT_MODAL: &str = "expected a focused float to be modal";
    const EXPECT_NOT_MODAL: &str = "expected a focused split to not be modal";
    const NO_STACK_OFFSET: u16 = 0;
    const NO_CARET: Option<Position> = None;
    const EXPECT_NAMEABLE: &str = "the test named a key no notation spells";
    const EXPECT_FOCUS_OWNS_KEY: &str = "a focused window spends every key it is handed";
    const EXPECT_NOTHING_SENT: &str = "a key no notation names has no event to send";

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

    #[test_case("spinner", "spinner" ; "bare_name_takes_spinner_style")]
    #[test_case("spinner:match_selected", "match_selected" ; "prefixed_name_takes_suffix_style")]
    fn spinner_span_bakes_to_live_glyph(span_style: &str, expected_style: &str) {
        let placeholder = "· ";
        let line = SnapshotLine {
            spans: vec![SnapshotSpan {
                text: placeholder.to_string(),
                style: SpanStyle::Named(span_style.into()),
            }],
        };
        let baked = snapshot_to_line(&line);
        assert_ne!(baked.spans[0].content, placeholder);
        assert_eq!(baked.spans[0].style, theme::style_by_name(expected_style));
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

    #[test_case(true ; "visible")]
    #[test_case(false ; "hidden")]
    fn needs_input_requires_visibility(visible: bool) {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _, _) = make_channels();
        let config = FloatConfig {
            visible,
            needs_input: true,
            ..FloatConfig::default()
        };
        mgr.open(make_buf(&[]), config, false, event_tx, cmd_rx);
        assert_eq!(mgr.needs_input(), visible);
    }

    const CHROME_AREA_HEIGHT: u16 = 40;
    const NARROW_WIDTH: u16 = 10;
    const LONG_TITLE: &str = "a title wider than the rows under it";

    fn chrome_config(border: Border, title: &str, footer: &[(&str, &str)]) -> FloatConfig {
        FloatConfig {
            width: Dimension::Abs(NARROW_WIDTH),
            height: Dimension::Abs(5),
            border,
            title: title.to_owned(),
            footer: footer
                .iter()
                .map(|(key, desc)| ((*key).to_owned(), (*desc).to_owned()))
                .collect(),
            ..FloatConfig::default()
        }
    }

    #[test_case(Border::Rounded, "", &[("Up/Down", "move"), ("Esc", "close")], 80 => 26 ; "widened_to_its_footer")]
    #[test_case(Border::Rounded, LONG_TITLE, &[], 80 => 38 ; "widened_to_its_title")]
    #[test_case(Border::Rounded, "", &[("Up/Down", "move"), ("Esc", "close")], 20 => 20 ; "the_screen_edge_still_cuts_it")]
    #[test_case(Border::None, LONG_TITLE, &[("Esc", "close")], 80 => NARROW_WIDTH ; "borderless_draws_no_chrome")]
    #[test_case(Border::Rounded, "", &[], 80 => NARROW_WIDTH ; "no_chrome_keeps_the_width_asked_for")]
    fn a_float_fits_its_chrome(
        border: Border,
        title: &str,
        footer: &[(&str, &str)],
        screen_width: u16,
    ) -> u16 {
        let area = Rect::new(0, 0, screen_width, CHROME_AREA_HEIGHT);
        let config = chrome_config(border, title, footer);
        resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET).width
    }

    #[test]
    fn resolve_rect_percent() {
        let area = Rect::new(0, 0, 200, 100);
        let config = FloatConfig {
            width: Dimension::Percent(50),
            height: Dimension::Percent(40),
            ..FloatConfig::default()
        };
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
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
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
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
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
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
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
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
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
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
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
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
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
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
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
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
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
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
        let r = resolve_rect(&config, area, NO_STACK_OFFSET, NO_CARET);
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 0, "only col is set, so row falls back to 0");
    }

    const CARET_AREA: Rect = Rect::new(0, 0, 80, 24);
    const CARET_WIDTH: u16 = 30;
    const CARET_HEIGHT: u16 = 7;
    const EXPECT_ON_SCREEN: &str = "the whole window has to land on screen";

    /// {caret_y}, {caret_x}, then the row, column and height the window lands
    /// at. The area is 24 rows, so a caret on row 20 has 20 above it and 3
    /// below, and one on row 2 has 2 above and 21 below.
    #[test_case(20, 5 => (13, 5, CARET_HEIGHT) ; "above_when_the_caret_is_near_the_bottom")]
    #[test_case(2, 5 => (3, 5, CARET_HEIGHT) ; "below_when_the_caret_is_near_the_top")]
    #[test_case(0, 0 => (1, 0, CARET_HEIGHT) ; "below_with_nothing_above")]
    #[test_case(23, 0 => (16, 0, CARET_HEIGHT) ; "above_with_nothing_below")]
    #[test_case(12, 0 => (5, 0, CARET_HEIGHT) ; "above_by_one_row_wins")]
    #[test_case(11, 0 => (12, 0, CARET_HEIGHT) ; "below_by_one_row_wins")]
    #[test_case(5, 60 => (6, 50, CARET_HEIGHT) ; "column_pulled_left_to_fit_the_width")]
    #[test_case(5, 79 => (6, 50, CARET_HEIGHT) ; "column_on_the_last_cell")]
    fn caret_rect_picks_the_roomier_side_and_keeps_the_width_on_screen(
        caret_y: u16,
        caret_x: u16,
    ) -> (u16, u16, u16) {
        let (r, sits_above) = caret_rect(
            Position::new(caret_x, caret_y),
            CARET_WIDTH,
            CARET_HEIGHT,
            CARET_AREA,
        );
        assert!(r.x + r.width <= CARET_AREA.width, "{EXPECT_ON_SCREEN}");
        assert!(r.y + r.height <= CARET_AREA.height, "{EXPECT_ON_SCREEN}");
        assert_eq!(
            sits_above,
            r.y < caret_y,
            "the side reported is the side the window landed on, and a stack grows along it"
        );
        (r.y, r.x, r.height)
    }

    /// Asking for more rows than the side has leaves a window that would run
    /// off the screen, so the height is trimmed to what is there. A caret with
    /// nowhere to go at all resolves to zero rows, which `view` skips.
    #[test_case(2, 40 => (3, 21) ; "trimmed_to_the_room_below")]
    #[test_case(21, 20 => (1, 20) ; "trimmed_to_the_room_above")]
    #[test_case(0, 40 => (1, 23) ; "trimmed_to_the_whole_screen")]
    fn caret_rect_trims_the_height_to_the_side_it_picked(caret_y: u16, h: u16) -> (u16, u16) {
        let (r, _) = caret_rect(Position::new(0, caret_y), CARET_WIDTH, h, CARET_AREA);
        (r.y, r.height)
    }

    /// One row of terminal: neither side of the caret holds anything, and a
    /// zero-height rect is what `view` already drops.
    #[test]
    fn caret_rect_gives_up_when_neither_side_has_a_row() {
        let area = Rect::new(0, 0, 80, 1);
        let (r, _) = caret_rect(Position::new(0, 0), CARET_WIDTH, CARET_HEIGHT, area);
        assert_eq!(r.height, 0);
    }

    fn caret_config() -> FloatConfig {
        FloatConfig {
            width: Dimension::Abs(CARET_WIDTH),
            height: Dimension::Abs(CARET_HEIGHT),
            anchor: Anchor::InputCaret,
            ..FloatConfig::default()
        }
    }

    /// A caret anchor is unusable if it errors the moment a form, a prompt or
    /// a `below` split takes the input box, which is often.
    #[test]
    fn caret_anchor_without_a_caret_falls_back_to_the_default_placement() {
        let config = caret_config();
        let centered = resolve_rect(&config, CARET_AREA, NO_STACK_OFFSET, NO_CARET);
        assert_eq!(
            (centered.x, centered.y),
            (25, 8),
            "no caret means the centred default"
        );

        let anchored = resolve_rect(
            &config,
            CARET_AREA,
            NO_STACK_OFFSET,
            Some(Position::new(5, 2)),
        );
        assert_eq!((anchored.x, anchored.y), (5, 3));
    }

    /// Returning before the stack offset left `stack = true` a no-op on this
    /// anchor, so a second caret window drew exactly on top of the first.
    #[test_case(2, 3, 11 ; "grows_downwards_from_a_window_below_the_caret")]
    #[test_case(21, 14, 6 ; "grows_upwards_from_a_window_above_the_caret")]
    fn caret_anchored_windows_stack_away_from_the_caret(caret_y: u16, first_y: u16, second_y: u16) {
        const OFFSET: u16 = CARET_HEIGHT + STACK_GAP;
        let config = caret_config();
        let caret = Some(Position::new(5, caret_y));

        let first = resolve_rect(&config, CARET_AREA, NO_STACK_OFFSET, caret);
        let second = resolve_rect(&config, CARET_AREA, OFFSET, caret);

        assert_eq!((first.y, second.y), (first_y, second_y));
        assert_eq!(second.x, first.x);
        assert_eq!(
            second.height, CARET_HEIGHT,
            "a stacked window keeps its rows instead of being clipped to a sliver"
        );
        assert!(
            second.y + second.height <= CARET_AREA.height,
            "{EXPECT_ON_SCREEN}"
        );
        assert_eq!(
            first.y.abs_diff(second.y) - CARET_HEIGHT,
            STACK_GAP,
            "{EXPECT_STACK_STEPS}"
        );
        assert!(
            !(second.y..second.y + second.height).contains(&caret_y),
            "a stacked window must leave the caret row, and the input box it is in, alone"
        );
    }

    /// The gap is the rows the earlier window really took. Summing what it
    /// asked for pushes the next one down by every row the caret trim cut.
    #[test]
    fn a_caret_stack_steps_over_the_trimmed_height() {
        const ROOM_ABOVE: u16 = 12;
        const TALLER_THAN_THE_SIDE: u16 = 40;
        let caret = Some(Position::new(0, ROOM_ABOVE));
        let tall = || FloatConfig {
            height: Dimension::Abs(TALLER_THAN_THE_SIDE),
            stack: true,
            ..caret_config()
        };

        let mut mgr = FloatManager::new();
        open_float(&mut mgr, tall());
        let second = open_float(&mut mgr, tall());
        let idx = mgr.windows.iter().position(|w| w.id == second).unwrap();

        assert_eq!(
            mgr.stack_offset(idx, CARET_AREA, caret),
            ROOM_ABOVE + STACK_GAP,
            "{EXPECT_STACK_STEPS}"
        );
    }

    /// The offsets used to apply only when the caret happened to be missing,
    /// so the same config placed the window in two different ways.
    #[test_case(Some(Position::new(5, 2)), 3, 5 ; "from_the_corner_next_to_the_caret")]
    #[test_case(NO_CARET, 8, 25 ; "from_the_centred_fallback")]
    fn caret_anchor_honours_row_and_col(caret: Option<Position>, base_y: u16, base_x: u16) {
        const ROW: i16 = 2;
        const COL: i16 = 1;
        let shifted = FloatConfig {
            row: Some(ROW),
            col: Some(COL),
            ..caret_config()
        };
        let r = resolve_rect(&shifted, CARET_AREA, NO_STACK_OFFSET, caret);
        assert_eq!((r.y, r.x), (base_y + ROW as u16, base_x + COL as u16));
    }

    const STACK_AREA: Rect = Rect::new(0, 0, 100, 50);
    const STACK_ROW: i16 = 1;
    const STACK_HEIGHT: u16 = 4;
    const EXPECT_STACK_STEPS: &str =
        "a stacked float must clear every earlier float in its corner, plus the gap";
    const EXPECT_STACK_CLOSES_GAP: &str = "survivors must take over the closed float's slot";
    const EXPECT_PLAIN_UNSTACKED: &str =
        "a float without stack must neither move nor take up stack room";

    fn stack_config(anchor: Anchor, stack: bool) -> FloatConfig {
        FloatConfig {
            width: Dimension::Abs(20),
            height: Dimension::Abs(STACK_HEIGHT),
            row: Some(STACK_ROW),
            col: Some(0),
            anchor,
            stack,
            ..FloatConfig::default()
        }
    }

    fn open_float(mgr: &mut FloatManager, config: FloatConfig) -> u32 {
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        mgr.open(make_buf(&["x"]), config, false, event_tx, cmd_rx);
        mgr.next_id - 1
    }

    /// Rows the windows would be painted at this frame, keyed by id so the
    /// zindex sort of `windows` cannot make the expectations drift. A hidden
    /// window is not laid out, so it gets no row at all.
    fn rows_by_id(mgr: &FloatManager) -> Vec<(u32, u16)> {
        let mut rows: Vec<(u32, u16)> = mgr
            .laid_out()
            .map(|(idx, win)| {
                let offset = mgr.stack_offset(idx, STACK_AREA, NO_CARET);
                (
                    win.id,
                    resolve_rect(&win.config, STACK_AREA, offset, NO_CARET).y,
                )
            })
            .collect();
        rows.sort_by_key(|(id, _)| *id);
        rows
    }

    #[test_case(Anchor::NE, [1, 6, 11] ; "ne_grows_downwards")]
    #[test_case(Anchor::SE, [47, 42, 37] ; "se_grows_upwards")]
    fn stacked_floats_offset_past_earlier_windows(anchor: Anchor, expected: [u16; 3]) {
        let mut mgr = FloatManager::new();
        for _ in 0..expected.len() {
            open_float(&mut mgr, stack_config(anchor, true));
        }

        let rows: Vec<u16> = rows_by_id(&mgr).into_iter().map(|(_, y)| y).collect();
        assert_eq!(rows, expected, "{EXPECT_STACK_STEPS}");
    }

    #[test]
    fn closing_first_stacked_float_moves_survivors_up() {
        let mut mgr = FloatManager::new();
        let first = open_float(&mut mgr, stack_config(Anchor::NE, true));
        let second = open_float(&mut mgr, stack_config(Anchor::NE, true));
        let third = open_float(&mut mgr, stack_config(Anchor::NE, true));

        mgr.remove_windows(|w| w.id == first);

        assert_eq!(
            rows_by_id(&mgr),
            vec![(second, 1), (third, 6)],
            "{EXPECT_STACK_CLOSES_GAP}",
        );
    }

    /// A hidden float is not drawn, so it holds no slot either: the float
    /// behind it takes the rows it had, the way it would if it had closed.
    #[test]
    fn a_hidden_stacked_float_gives_up_its_slot() {
        let mut mgr = FloatManager::new();
        let first = open_float(&mut mgr, stack_config(Anchor::NE, true));
        open_float(
            &mut mgr,
            FloatConfig {
                visible: false,
                ..stack_config(Anchor::NE, true)
            },
        );
        let last = open_float(&mut mgr, stack_config(Anchor::NE, true));

        assert_eq!(
            rows_by_id(&mgr),
            vec![(first, 1), (last, 6)],
            "{EXPECT_STACK_CLOSES_GAP}",
        );
    }

    #[test]
    fn plain_float_neither_shifts_nor_joins_the_stack() {
        let mut mgr = FloatManager::new();
        let first = open_float(&mut mgr, stack_config(Anchor::NE, true));
        let plain = open_float(&mut mgr, stack_config(Anchor::NE, false));
        let second = open_float(&mut mgr, stack_config(Anchor::NE, true));

        assert_eq!(
            rows_by_id(&mgr),
            vec![(first, 1), (plain, 1), (second, 6)],
            "{EXPECT_PLAIN_UNSTACKED}",
        );
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
    fn unfocused_float_does_not_make_a_focused_split_modal() {
        let mut mgr = FloatManager::new();
        let (_float_events, _float_commands) = open_with_lines(&mut mgr, &[PASTE_TEXT]);
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let split = FloatConfig {
            split: Split::Below,
            ..make_config()
        };
        mgr.open(make_buf(&[PASTE_TEXT]), split, true, event_tx, cmd_rx);

        assert!(Overlay::is_open(&mgr), "{EXPECT_OPEN}");
        assert!(!mgr.is_modal(), "{EXPECT_NOT_MODAL}");
    }

    #[test]
    fn focused_float_stays_modal_over_an_unfocused_split() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let split = FloatConfig {
            split: Split::Below,
            ..make_config()
        };
        mgr.open(make_buf(&[PASTE_TEXT]), split, true, event_tx, cmd_rx);
        let (_float_events, _float_commands) = open_with_lines(&mut mgr, &[PASTE_TEXT]);

        assert!(mgr.is_modal(), "{EXPECT_MODAL}");
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
    fn set_cursor_command() {
        let mut mgr = FloatManager::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a", "b", "c", "d", "e"]);

        cmd_tx.send(WinCommand::SetCursor(3)).unwrap();
        let _ = mgr.tick();
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
        let _ = mgr.tick();

        assert_eq!(mgr.windows[0].config.title, "Updated");
        assert_eq!(mgr.windows[0].config.zindex, 99);
    }

    #[test]
    fn needs_input_tracks_window_lifecycle_and_config() {
        let mut mgr = FloatManager::new();
        assert!(!mgr.needs_input());

        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a"]);
        assert!(!mgr.needs_input());

        cmd_tx
            .send(WinCommand::SetConfig(FloatConfigPatch {
                needs_input: Some(true),
                ..FloatConfigPatch::default()
            }))
            .unwrap();
        let _ = mgr.tick();
        assert!(mgr.needs_input());

        cmd_tx.send(WinCommand::Close).unwrap();
        let _ = mgr.tick();
        assert!(!mgr.needs_input());
    }

    #[test]
    fn close_command_from_lua() {
        let mut mgr = FloatManager::new();
        let (event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a"]);

        cmd_tx.send(WinCommand::Close).unwrap();
        let _ = mgr.tick();
        assert!(!mgr.is_open(), "{}", EXPECT_CLOSED);
        assert!(event_rx.drain().any(|e| matches!(e, WinEvent::Close)));
    }

    #[test]
    fn key_forwarded_to_lua() {
        let mut mgr = FloatManager::new();
        let (event_rx, _cmd_tx) = open_with_lines(&mut mgr, &["line1"]);

        let handled = mgr.handle_focused_key(press("a"));
        assert!(handled, "true when a window has focus");

        let evt = event_rx.drain().find(|e| matches!(e, WinEvent::Key { .. }));
        assert!(evt.is_some(), "key forwarded to lua");
    }

    #[test]
    fn handle_key_returns_false_when_empty() {
        let mut mgr = FloatManager::new();
        assert!(
            !mgr.handle_focused_key(press("a")),
            "handle_focused_key should return false with no windows"
        );
        assert!(
            !mgr.handle_claimed_key(press("a")),
            "handle_claimed_key should return false with no windows"
        );
    }

    /// One frame, which is what arms a claim: a window takes only the keys it
    /// declared *and* painted for.
    fn paint(mgr: &mut FloatManager) {
        let area = Rect::new(0, 0, 80, 40);
        render_into(mgr, area, |m, f| {
            m.view(f, area, NO_CARET);
        });
    }

    fn key(lhs: &str) -> Key {
        Key::parse(lhs).expect(EXPECT_NAMEABLE)
    }

    /// The terminal event {lhs} names, which is what the app hands the
    /// manager.
    fn press(lhs: &str) -> KeyEvent {
        key(lhs).into()
    }

    /// An unfocused window that declared {claims}, as the completion popup
    /// opens one: the user goes on typing into the chat input under it. The
    /// command end comes back because dropping it closes the window.
    fn open_claiming(mgr: &mut FloatManager, claims: &[&str], zindex: u16) -> WinChannels {
        let (event_tx, cmd_rx, event_rx, cmd_tx) = make_channels();
        let config = FloatConfig {
            zindex,
            keys: claims.iter().copied().map(key).collect(),
            ..FloatConfig::default()
        };
        mgr.open(make_buf(&["x"]), config, false, event_tx, cmd_rx);
        paint(mgr);
        (event_rx, cmd_tx)
    }

    fn took_a_key(events: &flume::Receiver<WinEvent>) -> bool {
        events.drain().any(|e| matches!(e, WinEvent::Key { .. }))
    }

    /// The one gap the layer machinery existed to close: an unfocused window
    /// has to be able to take the keys its footer advertises, and the key must
    /// not also reach whatever is underneath.
    #[test]
    fn an_unfocused_window_takes_the_keys_it_claimed() {
        let mut mgr = FloatManager::new();
        let (events, _cmd_tx) = open_claiming(&mut mgr, &["<Tab>"], 50);

        assert!(mgr.handle_claimed_key(press("<Tab>")));
        assert!(took_a_key(&events), "{CLAIM_NOT_DELIVERED}");
    }

    /// Everything else goes on to the chat input, which is where the user is
    /// typing while the popup is up.
    #[test]
    fn an_unfocused_window_leaves_every_other_key_alone() {
        let mut mgr = FloatManager::new();
        let (events, _cmd_tx) = open_claiming(&mut mgr, &["<Tab>"], 50);

        assert!(!mgr.handle_claimed_key(press("a")));
        assert!(!took_a_key(&events), "{CLAIM_LEAKED}");
    }

    /// Modifiers are compared exactly, the way a keymap binding is, so a
    /// window claiming `<C-n>` never answers a bare `n` the user typed.
    #[test]
    fn a_claim_answers_only_its_own_modifiers() {
        let mut mgr = FloatManager::new();
        let (events, _cmd_tx) = open_claiming(&mut mgr, &["<C-n>"], 50);

        assert!(!mgr.handle_claimed_key(press("n")));
        assert!(!took_a_key(&events), "{CLAIM_LEAKED}");
    }

    /// Two popups claiming one key is the ordinary stacking question, and the
    /// answer is the one the user is looking at.
    #[test]
    fn the_topmost_claiming_window_takes_the_key() {
        let mut mgr = FloatManager::new();
        let (under, _under_tx) = open_claiming(&mut mgr, &["<Esc>"], 10);
        let (over, _over_tx) = open_claiming(&mut mgr, &["<Esc>"], 90);

        assert!(mgr.handle_claimed_key(press("<Esc>")));
        assert!(took_a_key(&over), "{CLAIM_NOT_DELIVERED}");
        assert!(!took_a_key(&under), "{CLAIM_LEAKED}");
    }

    /// Raising a popup over another is one `set_config` away, and z-order
    /// decides the claim as well as the paint order. Without the re-sort the
    /// window drawn in front watched the one underneath go on answering its
    /// key.
    #[test]
    fn raising_a_window_moves_the_claim_with_it() {
        let mut mgr = FloatManager::new();
        let (under, under_tx) = open_claiming(&mut mgr, &["<CR>"], 10);
        let (over, _over_tx) = open_claiming(&mut mgr, &["<CR>"], 90);

        under_tx
            .send(WinCommand::SetConfig(FloatConfigPatch {
                zindex: Some(99),
                ..FloatConfigPatch::default()
            }))
            .unwrap();

        assert!(mgr.handle_claimed_key(press("<CR>")));
        assert!(took_a_key(&under), "{CLAIM_NOT_DELIVERED}");
        assert!(!took_a_key(&over), "{CLAIM_LEAKED}");
    }

    /// The sort is stable, so a patch that only levels two windows leaves the
    /// one opened later in front, which is where the user has been seeing it.
    #[test]
    fn levelling_the_zindex_keeps_open_order() {
        let mut mgr = FloatManager::new();
        let (under, under_tx) = open_claiming(&mut mgr, &["<Tab>"], 10);
        let (over, _over_tx) = open_claiming(&mut mgr, &["<Tab>"], 90);

        under_tx
            .send(WinCommand::SetConfig(FloatConfigPatch {
                zindex: Some(90),
                ..FloatConfigPatch::default()
            }))
            .unwrap();

        assert!(mgr.handle_claimed_key(press("<Tab>")));
        assert!(took_a_key(&over), "{CLAIM_NOT_DELIVERED}");
        assert!(!took_a_key(&under), "{CLAIM_LEAKED}");
    }

    /// A terminal that speaks the kitty protocol reports Shift+Tab as
    /// `Tab + SHIFT` and every other one sends `CSI Z`, i.e. `BackTab`. One
    /// claim has to answer both, or a popup's binding works on half the
    /// terminals in the world.
    #[test_case(KeyCode::Tab, KeyModifiers::SHIFT ; "kitty_reports_tab_with_shift")]
    #[test_case(KeyCode::BackTab, KeyModifiers::NONE ; "everything_else_sends_csi_z")]
    fn a_shift_tab_claim_answers_either_way_the_terminal_spells_it(
        code: KeyCode,
        modifiers: KeyModifiers,
    ) {
        let mut mgr = FloatManager::new();
        let (events, _cmd_tx) = open_claiming(&mut mgr, &["<S-Tab>"], 50);

        assert!(mgr.handle_claimed_key(KeyEvent::new(code, modifiers)));
        assert!(took_a_key(&events), "{CLAIM_NOT_DELIVERED}");
    }

    #[test]
    fn a_focused_window_spends_a_key_no_notation_names() {
        let mut mgr = FloatManager::new();
        let (events, _cmd_tx) = open_with_lines(&mut mgr, &["x"]);

        assert!(
            mgr.handle_focused_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SUPER)),
            "{EXPECT_FOCUS_OWNS_KEY}"
        );
        assert!(!took_a_key(&events), "{EXPECT_NOTHING_SENT}");
    }

    /// No claim can name the press, so an unfocused window lets it through to
    /// the host, which is the one that knows what `Super` means.
    #[test]
    fn a_claim_never_takes_a_key_no_notation_names() {
        let mut mgr = FloatManager::new();
        let (events, _cmd_tx) = open_claiming(&mut mgr, &["<CR>"], 50);

        assert!(!mgr.handle_claimed_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SUPER)));
        assert!(!took_a_key(&events), "{CLAIM_LEAKED}");
    }

    /// A plugin compares `ev.key` against the notation it claimed, so the
    /// event has to carry exactly that string.
    #[test_case("<C-n>" ; "ctrl_letter")]
    #[test_case("<Space>" ; "space")]
    #[test_case("<S-Tab>" ; "shift_tab")]
    #[test_case("a" ; "plain_char")]
    fn a_delivered_key_carries_its_canonical_notation(lhs: &str) {
        let mut mgr = FloatManager::new();
        let (events, _cmd_tx) = open_claiming(&mut mgr, &[lhs], 50);

        assert!(mgr.handle_claimed_key(press(lhs)));
        let delivered = events
            .drain()
            .find_map(|e| match e {
                WinEvent::Key { key } => Some(key.notation()),
                _ => None,
            })
            .expect(CLAIM_NOT_DELIVERED);
        assert_eq!(delivered, lhs);
    }

    /// What bounds a claim, and the whole reason there is nothing to release:
    /// the list lives on the window and goes out with it.
    #[test]
    fn a_claim_dies_with_the_window() {
        let mut mgr = FloatManager::new();
        let (events, _cmd_tx) = open_claiming(&mut mgr, &["<Tab>"], 50);

        mgr.close_all();

        assert!(!mgr.handle_claimed_key(press("<Tab>")));
        assert!(!took_a_key(&events), "{CLAIM_LEAKED}");
    }

    /// The close runs on the Lua thread, so between it and the next tick the
    /// window is still in the list. A key claimed there would be handed to a
    /// loop that has stopped reading and lost, which is why dispatch drains
    /// the commands first.
    #[test]
    fn a_window_closing_this_instant_claims_nothing() {
        let mut mgr = FloatManager::new();
        let (events, cmd_tx) = open_claiming(&mut mgr, &["<CR>"], 50);

        cmd_tx.send(WinCommand::Close).unwrap();

        assert!(!mgr.handle_claimed_key(press("<CR>")));
        assert!(!took_a_key(&events), "{CLAIM_LEAKED}");
        assert!(!mgr.is_open(), "{EXPECT_CLOSED}");
    }

    /// A window opened hidden is not on screen yet, so the keys it advertises
    /// are advertised to nobody.
    #[test]
    fn a_hidden_window_claims_nothing() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, event_rx, _cmd_tx) = make_channels();
        let config = FloatConfig {
            visible: false,
            keys: vec![key("<Tab>")],
            ..FloatConfig::default()
        };
        mgr.open(make_buf(&["x"]), config, false, event_tx, cmd_rx);
        paint(&mut mgr);

        assert!(!mgr.handle_claimed_key(press("<Tab>")));
        assert!(!took_a_key(&event_rx), "{CLAIM_LEAKED}");
    }

    /// The other half of the same rule: a window the plugin hides is not drawn
    /// either. While it was, `win:hide()` left a popup on screen advertising
    /// `Esc close` in its footer with the Esc falling through to the chat input
    /// underneath.
    #[test]
    fn hiding_a_window_takes_it_off_the_screen_and_out_of_the_claim() {
        let mut mgr = FloatManager::new();
        let (events, cmd_tx) = open_claiming(&mut mgr, &["<Esc>"], 50);
        assert!(mgr.windows[0].on_screen, "{EXPECT_PAINTED}");

        cmd_tx.send(WinCommand::SetVisible(false)).unwrap();
        let _ = mgr.tick();
        let area = Rect::new(0, 0, 80, 40);
        render_into(&mut mgr, area, |m, f| {
            assert_eq!(
                m.view(f, area, NO_CARET),
                Rect::default(),
                "{EXPECT_HIDDEN_UNPAINTED}"
            );
        });

        assert!(!mgr.windows[0].on_screen, "{EXPECT_HIDDEN_UNPAINTED}");
        assert!(!mgr.handle_claimed_key(press("<Esc>")));
        assert!(!took_a_key(&events), "{CLAIM_LEAKED}");
    }

    /// `visible` is the plugin's own flag and says nothing about geometry. A
    /// window sized to nothing paints nothing, so it can advertise nothing,
    /// and a claim it kept would take `<CR>` and `<Esc>` from the user for the
    /// rest of the run with no footer anywhere to say where they went.
    #[test]
    fn a_window_sized_to_nothing_claims_nothing() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, event_rx, _cmd_tx) = make_channels();
        let config = FloatConfig {
            width: Dimension::Abs(0),
            height: Dimension::Abs(0),
            keys: vec![key("<CR>")],
            ..FloatConfig::default()
        };
        mgr.open(make_buf(&["x"]), config, false, event_tx, cmd_rx);
        paint(&mut mgr);

        assert!(!mgr.handle_claimed_key(press("<CR>")));
        assert!(!took_a_key(&event_rx), "{CLAIM_LEAKED}");
    }

    /// A claim is armed by the frame that painted it, so a window opened
    /// between two frames holds nothing yet: until the user can see it, the
    /// key still belongs to whatever was already on screen.
    #[test]
    fn a_window_that_has_not_painted_yet_claims_nothing() {
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, event_rx, _cmd_tx) = make_channels();
        let config = FloatConfig {
            keys: vec![key("<Tab>")],
            ..FloatConfig::default()
        };
        mgr.open(make_buf(&["x"]), config, false, event_tx, cmd_rx);

        assert!(!mgr.handle_claimed_key(press("<Tab>")));
        assert!(!took_a_key(&event_rx), "{CLAIM_LEAKED}");

        paint(&mut mgr);

        assert!(mgr.handle_claimed_key(press("<Tab>")));
        assert!(took_a_key(&event_rx), "{CLAIM_NOT_DELIVERED}");
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
        let _ = mgr.tick();
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
        let _ = mgr.tick();
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
        let _ = mgr.tick();

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
        let _ = mgr.tick();

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
        let _ = mgr.tick();
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
        let _ = mgr.tick();

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
        let _ = mgr.tick();
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
        let _ = mgr.tick();
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

        mgr.handle_focused_key(press("x"));

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

        let _ = mgr.tick();

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
        let _ = mgr.tick();
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
        let _ = mgr.tick();
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
        let _ = mgr.tick();

        assert_eq!(
            mgr.windows[0].cached_lines.len(),
            3,
            "all appends since last read must be visible after one tick",
        );
    }

    /// Plugin output lands in the buffer behind the UI's back, so an append is
    /// only ever seen because `tick` reports it. Exactly once, though: this
    /// poller runs for every session, so a manager that keeps reporting dirty
    /// with nothing to show would keep the whole loop awake forever.
    #[test]
    fn buffer_append_owes_exactly_one_frame() {
        let mut mgr = FloatManager::new();
        assert_eq!(mgr.tick(), Dirty::NO, "{QUIET}");

        let (event_tx, cmd_rx, _event_rx, _cmd_tx) = make_channels();
        let buf = make_buf(&["initial"]);
        mgr.open(buf.clone(), make_config(), true, event_tx, cmd_rx);
        assert_eq!(mgr.tick(), Dirty::NO, "{QUIET}");

        buf.append(make_line("second"));
        assert_eq!(mgr.tick(), Dirty::YES, "{OWED}");
        assert_eq!(mgr.tick(), Dirty::NO, "{QUIET}");
    }

    /// Every command changes what is drawn, and `Close` reports through a
    /// different branch than the rest because it breaks out of the drain loop.
    #[test_case(WinCommand::SetCursor(2) ; "set_cursor")]
    #[test_case(WinCommand::SetVisible(false) ; "set_visible")]
    #[test_case(WinCommand::Close ; "close")]
    fn window_command_owes_exactly_one_frame(cmd: WinCommand) {
        let mut mgr = FloatManager::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a", "b", "c"]);

        cmd_tx.send(cmd).unwrap();
        assert_eq!(mgr.tick(), Dirty::YES, "{OWED}");
        assert_eq!(mgr.tick(), Dirty::NO, "{QUIET}");
    }

    /// A plugin that goes away drops its sender and the window goes with it.
    /// Nothing else wakes the loop, so that removal is only painted if this
    /// tick reports it.
    #[test]
    fn disconnected_cmd_channel_closes_the_window_and_owes_one_frame() {
        let mut mgr = FloatManager::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a"]);
        assert!(mgr.is_open(), "{EXPECT_OPEN}");

        drop(cmd_tx);
        assert_eq!(mgr.tick(), Dirty::YES, "{OWED}");
        assert!(!mgr.is_open(), "{EXPECT_CLOSED}");
        assert_eq!(mgr.tick(), Dirty::NO, "{QUIET}");
    }

    /// A hidden window is not painted, and the cadence is what notices it
    /// asking to come back: [`FloatManager::tick`] is the only thing that
    /// drains the commands, so going idle over a hidden window would leave its
    /// `SetVisible(true)` unread and the window hidden for good.
    #[test]
    fn cadence_spins_while_any_window_is_open_visible_or_not() {
        let mut mgr = FloatManager::new();
        assert_eq!(mgr.cadence(), Cadence::IDLE);

        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a"]);
        assert_eq!(mgr.cadence(), Cadence::SPINNER);

        cmd_tx.send(WinCommand::SetVisible(false)).unwrap();
        let _ = mgr.tick();
        assert!(!mgr.windows[0].visible);
        assert_eq!(
            mgr.cadence(),
            Cadence::SPINNER,
            "an invisible window still counts as open, so the loop keeps draining it"
        );

        mgr.close_all();
        assert_eq!(mgr.cadence(), Cadence::IDLE);
    }

    #[test]
    fn close_before_set_cursor_in_same_tick_is_safe() {
        let mut mgr = FloatManager::new();
        let (_event_rx, cmd_tx) = open_with_lines(&mut mgr, &["a", "b", "c"]);

        cmd_tx.send(WinCommand::Close).unwrap();
        cmd_tx.send(WinCommand::SetCursor(2)).unwrap();
        let _ = mgr.tick();

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
        let _ = mgr.tick();

        assert!(
            !mgr.handle_focused_key(press("a")),
            "no windows remain, so handle_focused_key must return false",
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
            opened_focused: true,
            painting: false,
            on_screen: false,
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

    fn open_split(mgr: &mut FloatManager, dir: Split, extent: u16, focus: bool) -> WinChannels {
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

    type WinChannels = (flume::Receiver<WinEvent>, flume::Sender<WinCommand>);

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
            let u = m.view(f, area, NO_CARET);
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
        mgr.open(make_buf(&["a"]), FloatConfig::default(), true, tx1, rx1);
        let survivor = mgr.windows[0].id;

        let _ = open_split(&mut mgr, Split::Below, 5, true);

        mgr.remove_windows(|w| w.config.split == Split::Below);
        assert_eq!(mgr.focused_id, Some(survivor), "{EXPECT_FOCUS_RECOVERS}");
    }

    const EXPECT_NO_PROMOTION: &str =
        "a window opened unfocused must never be handed focus, or it swallows every key";

    /// The completion popup is up while the user types into the chat input
    /// underneath. Handing it the focus a closing modal gave up would turn it
    /// into a key sink: it is handed every key, looks up the few it knows, and
    /// drops the rest, so typing stops arriving with nothing to show why.
    #[test]
    fn a_window_opened_unfocused_is_never_handed_focus() {
        let mut mgr = FloatManager::new();
        let (events, _cmd_tx) = open_claiming(&mut mgr, &["<Tab>"], 50);
        let (tx, rx, _erx, _ctx) = make_channels();
        mgr.open(make_buf(&["modal"]), make_config(), true, tx, rx);
        let modal = mgr.windows.last().expect(EXPECT_OPEN).id;

        mgr.remove_windows(|w| w.id == modal);

        assert_eq!(mgr.focused_id, None, "{EXPECT_NO_PROMOTION}");
        assert!(!mgr.handle_focused_key(press("a")));
        assert!(!took_a_key(&events), "{CLAIM_LEAKED}");
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
            let u = m.view(f, area, NO_CARET);
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

    const EXPECT_NO_ROOM: &str = "a hidden window must ask for no room";
    const EXPECT_NO_PAINT: &str = "a hidden window must paint nothing";
    const EXPECT_NO_CLAIM: &str = "a hidden window must claim no keys";
    const HIDDEN_EXTENT: u16 = 6;

    /// Draws one frame in the same order the app does: splits, then panels,
    /// then floats. Returns how many splits and panels asked for room.
    fn draw_one_frame(mgr: &mut FloatManager, area: Rect) -> usize {
        let split_reqs = mgr.split_reqs(area);
        let splits = crate::components::split_layout::carve(area, &split_reqs);
        let panels = mgr.panel_reqs();
        let room_asked = split_reqs.len() + panels.len();

        render_into(mgr, area, |m, f| {
            for dir in Split::ALL {
                if let Some(rect) = splits.rect(dir) {
                    m.view_split(f, dir, rect);
                }
            }
            let mut y = splits.inner.y;
            for (idx, h) in panels {
                m.view_panel(f, idx, Rect::new(splits.inner.x, y, splits.inner.width, h));
                y += h;
            }
            m.view(f, splits.inner, NO_CARET);
        });

        room_asked
    }

    /// The claim rides on the paint: a window only gets keys once a frame has
    /// drawn it, so showing it again has to bring back room, paint and keys
    /// together.
    #[test_case(Split::None ; "float")]
    #[test_case(Split::Above ; "split_above")]
    #[test_case(Split::Below ; "split_below")]
    #[test_case(Split::Left ; "split_left")]
    #[test_case(Split::Right ; "split_right")]
    #[test_case(Split::Panel ; "panel")]
    fn a_hidden_window_of_any_kind_is_out_of_the_layout(split: Split) {
        let area = Rect::new(0, 0, 80, 40);
        let mut mgr = FloatManager::new();
        let (event_tx, cmd_rx, events, cmd_tx) = make_channels();
        let config = FloatConfig {
            width: Dimension::Abs(HIDDEN_EXTENT),
            height: Dimension::Abs(HIDDEN_EXTENT),
            border: Border::None,
            split,
            visible: false,
            keys: vec![key("<Tab>")],
            ..FloatConfig::default()
        };
        mgr.open(make_buf(&["x"]), config, false, event_tx, cmd_rx);

        assert_eq!(draw_one_frame(&mut mgr, area), 0, "{EXPECT_NO_ROOM}");
        assert!(!mgr.windows[0].on_screen, "{EXPECT_NO_PAINT}");
        assert!(!mgr.handle_claimed_key(press("<Tab>")), "{EXPECT_NO_CLAIM}");
        assert!(!took_a_key(&events), "{EXPECT_NO_CLAIM}");

        cmd_tx.send(WinCommand::SetVisible(true)).unwrap();
        let _ = mgr.tick();

        assert_eq!(
            draw_one_frame(&mut mgr, area) > 0,
            split != Split::None,
            "showing it asks for room again, except a float which never does"
        );
        assert!(mgr.windows[0].on_screen, "showing it paints it again");
        assert!(
            mgr.handle_claimed_key(press("<Tab>")),
            "and its claim is back"
        );
        assert!(took_a_key(&events), "{CLAIM_NOT_DELIVERED}");
    }

    #[test]
    fn panel_reqs_returns_visible_panels_sorted_by_order() {
        let mut mgr = FloatManager::new();
        let (tx1, rx1, _, _) = make_channels();
        let (tx2, rx2, _, _) = make_channels();

        let cfg1 = FloatConfig {
            split: Split::Panel,
            height: Dimension::Abs(5),
            order: 20,
            ..FloatConfig::default()
        };
        let cfg2 = FloatConfig {
            split: Split::Panel,
            height: Dimension::Abs(3),
            order: 10,
            ..FloatConfig::default()
        };

        mgr.open(make_buf(&["a"]), cfg1, false, tx1, rx1);
        mgr.open(make_buf(&["b"]), cfg2, false, tx2, rx2);

        let reqs = mgr.panel_reqs();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].1, 3, "order=10 should come first");
        assert_eq!(reqs[1].1, 5, "order=20 should come second");
    }

    #[test]
    fn panel_window_not_evicted_on_second_open() {
        let mut mgr = FloatManager::new();
        let (tx1, rx1, _, _) = make_channels();
        let (tx2, rx2, _, _) = make_channels();

        let cfg = FloatConfig {
            split: Split::Panel,
            height: Dimension::Abs(3),
            ..FloatConfig::default()
        };

        mgr.open(make_buf(&["a"]), cfg.clone(), false, tx1, rx1);
        mgr.open(make_buf(&["b"]), cfg, false, tx2, rx2);

        assert_eq!(mgr.panel_reqs().len(), 2);
    }

    #[test]
    fn hidden_panel_excluded_from_reqs() {
        let mut mgr = FloatManager::new();
        let (tx, _rx, _, _) = make_channels();
        let (cmd_tx, cmd_rx) = flume::bounded::<WinCommand>(8);

        let cfg = FloatConfig {
            split: Split::Panel,
            height: Dimension::Abs(5),
            ..FloatConfig::default()
        };

        mgr.open(make_buf(&["a"]), cfg, false, tx, cmd_rx);
        assert_eq!(mgr.panel_reqs().len(), 1);

        cmd_tx.send(WinCommand::SetVisible(false)).unwrap();
        let _ = mgr.tick();
        assert_eq!(mgr.panel_reqs().len(), 0);

        cmd_tx.send(WinCommand::SetVisible(true)).unwrap();
        let _ = mgr.tick();
        assert_eq!(mgr.panel_reqs().len(), 1);
    }

    #[test]
    fn focus_fallback_skips_panel_windows() {
        let mut mgr = FloatManager::new();

        let (tx_panel, rx_panel, _, _cmd_tx_panel) = make_channels();
        let panel_cfg = FloatConfig {
            split: Split::Panel,
            height: Dimension::Abs(5),
            ..FloatConfig::default()
        };
        mgr.open(make_buf(&["panel"]), panel_cfg, false, tx_panel, rx_panel);

        let (tx_modal, rx_modal, _, cmd_tx_modal) = make_channels();
        mgr.open(
            make_buf(&["modal"]),
            make_config(),
            true,
            tx_modal,
            rx_modal,
        );

        assert_eq!(mgr.focused_id, Some(1));

        cmd_tx_modal.send(WinCommand::Close).unwrap();
        let _ = mgr.tick();

        assert_eq!(
            mgr.focused_id, None,
            "focus must not fall back to a panel window"
        );
        assert_eq!(mgr.windows.len(), 1, "panel window must survive");
    }
}
