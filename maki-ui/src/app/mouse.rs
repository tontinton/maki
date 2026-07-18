use std::time::{Duration, Instant};

use crate::clipboard::CopyResult;
use crate::components::scrollbar;
use crate::selection::{self, ContentRegion, EdgeScroll, Selection, SelectionState, SelectionZone};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::App;

pub(super) const EDGE_SCROLL_LINES: i32 = 1;
pub(super) const EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(25);

pub(crate) struct ScrollbarDrag {
    zone: SelectionZone,
    content_len: u16,
    viewport_height: u16,
    thumb_len: u16,
    grab_offset: i32,
    track_y_start: u16,
}

impl App {
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) {
        if self.handle_scrollbar_mouse(&event) {
            return;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(zone) = self.zone_at(event.row, event.column) {
                    if self.has_modal_overlay() && zone.zone != SelectionZone::Overlay {
                        return;
                    }
                    let scroll = self.scroll_offset(zone.zone);
                    self.selection_state = Some(SelectionState::Dragging {
                        sel: Selection::start(
                            event.row,
                            event.column,
                            zone.area,
                            zone.zone,
                            scroll,
                        ),
                        edge_scroll: None,
                        last_drag_col: event.column,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_drag(event.row, event.column);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(SelectionState::Dragging { sel, .. }) = self.selection_state {
                    if !sel.is_empty() {
                        self.selection_state = Some(SelectionState::PendingCopy { sel });
                    } else {
                        let zone = sel.zone;
                        self.selection_state = None;
                        if zone == SelectionZone::Messages {
                            let area = self.msg_area();
                            self.chats[self.active_chat].handle_click(event.row, area);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_scrollbar_mouse(&mut self, event: &MouseEvent) -> bool {
        if !scrollbar::is_enabled() {
            return false;
        }
        if self.has_modal_overlay() {
            self.scrollbar_drag = None;
            return false;
        }

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(zone) = self.zone_at(event.row, event.column) else {
                    return false;
                };
                let Some(info) = zone.scroll_info else {
                    return false;
                };
                if event.column != zone.area.right().saturating_sub(1) {
                    return false;
                }
                let Some((thumb_start, thumb_end)) =
                    scrollbar::vertical_thumb_bounds(info.content_len, zone.area.height, info.position)
                else {
                    return false;
                };

                let row_rel = event.row.saturating_sub(zone.area.y);
                let thumb_len = thumb_end.saturating_sub(thumb_start);
                self.selection_state = None;

                if row_rel >= thumb_start && row_rel < thumb_end {
                    self.scrollbar_drag = Some(ScrollbarDrag {
                        zone: zone.zone,
                        content_len: info.content_len,
                        viewport_height: zone.area.height,
                        thumb_len,
                        grab_offset: row_rel as i32 - thumb_start as i32,
                        track_y_start: zone.area.y,
                    });
                } else {
                    let page = zone.area.height as i32;
                    let delta = if row_rel < thumb_start { page } else { -page };
                    self.scroll_zone(zone.zone, delta);
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (zone, content_len, viewport_height, thumb_len, grab_offset, track_y_start) = {
                    let Some(ref drag) = self.scrollbar_drag else {
                        return false;
                    };
                    (
                        drag.zone,
                        drag.content_len,
                        drag.viewport_height,
                        drag.thumb_len,
                        drag.grab_offset,
                        drag.track_y_start,
                    )
                };
                let row_rel = event.row.saturating_sub(track_y_start) as i32;
                let max_thumb_start = (viewport_height as i32 - thumb_len as i32).max(0);
                let new_thumb_start = (row_rel - grab_offset).clamp(0, max_thumb_start) as u16;
                let position =
                    scrollbar::position_for_thumb_row(content_len, viewport_height, thumb_len, new_thumb_start);
                self.set_scroll_zone(zone, position);
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.scrollbar_drag.is_some() => {
                self.scrollbar_drag = None;
                true
            }
            MouseEventKind::Up(MouseButton::Left) => false,
            _ => false,
        }
    }

    fn set_scroll_zone(&mut self, zone: SelectionZone, position: u16) {
        match zone {
            SelectionZone::Messages => self.chats[self.active_chat].set_scroll_top(position),
            SelectionZone::Input => self.input_box.set_scroll_y(position),
            SelectionZone::Overlay => {}
        }
    }

    fn handle_drag(&mut self, row: u16, col: u16) {
        let (zone, area) = match self.selection_state {
            Some(SelectionState::Dragging {
                ref sel,
                ref mut last_drag_col,
                ..
            }) => {
                *last_drag_col = col;
                (sel.zone, sel.area)
            }
            _ => return,
        };

        let at_top = row <= area.y;
        let at_bottom = row + 1 >= area.bottom();

        if at_top || at_bottom {
            let dir = if at_top {
                EDGE_SCROLL_LINES
            } else {
                -EDGE_SCROLL_LINES
            };
            let first_edge_hit = if let Some(SelectionState::Dragging { edge_scroll, .. }) =
                &mut self.selection_state
            {
                let first = edge_scroll.is_none();
                match edge_scroll {
                    Some(es) => es.dir = dir,
                    None => {
                        *edge_scroll = Some(EdgeScroll {
                            dir,
                            last_tick: Instant::now(),
                        });
                    }
                }
                first
            } else {
                false
            };
            if first_edge_hit {
                self.scroll_zone(zone, dir);
            }
            self.update_selection_to_edge(zone, col);
        } else {
            if let Some(SelectionState::Dragging { edge_scroll, .. }) = &mut self.selection_state {
                *edge_scroll = None;
            }
            let scroll = self.scroll_offset(zone);
            if let Some(SelectionState::Dragging { sel, .. }) = &mut self.selection_state {
                sel.update(row, col, scroll);
            }
        }
    }

    fn update_selection_to_edge(&mut self, zone: SelectionZone, col: u16) {
        let scroll = self.scroll_offset(zone);
        let Some(SelectionState::Dragging {
            ref mut sel,
            ref edge_scroll,
            ..
        }) = self.selection_state
        else {
            return;
        };
        let edge_row = if edge_scroll.as_ref().is_some_and(|es| es.dir > 0) {
            sel.area.y
        } else {
            sel.area.bottom().saturating_sub(1)
        };
        sel.update(edge_row, col, scroll);
    }

    pub fn tick_edge_scroll(&mut self) {
        let (dir, zone, col) = match self.selection_state {
            Some(SelectionState::Dragging {
                ref sel,
                ref mut edge_scroll,
                last_drag_col,
            }) => {
                let Some(es) = edge_scroll else {
                    return;
                };
                if es.last_tick.elapsed() < EDGE_SCROLL_INTERVAL {
                    return;
                }
                let dir = es.dir;
                es.last_tick = Instant::now();
                (dir, sel.zone, last_drag_col)
            }
            _ => return,
        };

        self.scroll_zone(zone, dir);
        self.update_selection_to_edge(zone, col);
    }

    pub(super) fn copy_selection(
        &mut self,
        buf: &mut ratatui::buffer::Buffer,
        sel: &Selection,
        render_chat: usize,
    ) {
        let text = match sel.zone {
            SelectionZone::Messages => {
                let msg_area = self.msg_area();
                self.chats[render_chat].extract_selection_text(sel, msg_area)
            }
            SelectionZone::Input => {
                let scroll = self.scroll_offset(sel.zone);
                let Some(screen_sel) = sel.to_screen(scroll) else {
                    self.selection_state = None;
                    return;
                };
                let copy_text = self.input_box.copy_text();
                let input_area = sel.area;
                let line_breaks = self.input_box.line_breaks(input_area.width);
                let regions = [ContentRegion {
                    area: input_area,
                    raw_text: &copy_text,
                    line_breaks,
                }];
                selection::extract_selected_text(buf, &screen_sel, &regions)
            }
            SelectionZone::Overlay => {
                let scroll = self.scroll_offset(sel.zone);
                let Some(screen_sel) = sel.to_screen(scroll) else {
                    self.selection_state = None;
                    return;
                };
                let regions = [ContentRegion {
                    area: sel.area,
                    ..Default::default()
                }];
                selection::extract_selected_text(buf, &screen_sel, &regions)
            }
        };

        match self.clipboard.copy_text(&text) {
            Ok(CopyResult::Noop) => {}
            Ok(CopyResult::Copied) => self.status_bar.flash("Copied selection".into()),
            Err(e) => self.status_bar.flash(format!("Copy failed: {e}")),
        }
        self.selection_state = None;
    }

    pub(super) fn zone_at(&self, row: u16, col: u16) -> Option<selection::SelectableZone> {
        self.zones.zone_at(row, col)
    }

    pub(super) fn scroll_offset(&self, zone: SelectionZone) -> u32 {
        match zone {
            SelectionZone::Messages => self.chats[self.active_chat].scroll_top() as u32,
            SelectionZone::Input => self.input_box.scroll_y() as u32,
            SelectionZone::Overlay => 0,
        }
    }

    pub(super) fn scroll_zone(&mut self, zone: SelectionZone, delta: i32) {
        match zone {
            SelectionZone::Messages => self.chats[self.active_chat].scroll(delta),
            SelectionZone::Input => self.input_box.scroll(delta),
            SelectionZone::Overlay => {}
        }
    }

    pub(super) fn msg_area(&self) -> Rect {
        self.zones
            .find(SelectionZone::Messages)
            .map(|z| {
                let a = z.area;
                Rect::new(a.x, a.y, a.width.saturating_sub(1), a.height)
            })
            .unwrap_or_default()
    }
}
