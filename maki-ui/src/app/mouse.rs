use std::time::{Duration, Instant};

use crate::clipboard::CopyResult;
use crate::components::scrollbar;
use crate::selection::{
    self, ContentRegion, DocPos, EdgeScroll, RowPos, ScreenSelection, Selection, SelectionState,
    SelectionZone,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::repaint::Dirty;

use super::App;

pub(super) const EDGE_SCROLL_LINES: i32 = 1;
pub(super) const EDGE_SCROLL_INTERVAL: Duration = Duration::from_millis(25);

impl App {
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(zone) = self.zone_at(event.row, event.column) {
                    if self.has_modal_overlay() && zone.zone != SelectionZone::Overlay {
                        return;
                    }
                    // Move the cursor to the click position in the input area.
                    if zone.zone == SelectionZone::Input {
                        let focused = !self.any_overlay_open();
                        self.input_box
                            .handle_click(zone.area, event.row, event.column, focused);
                    }
                    let pos = self.doc_pos(zone.zone, zone.area, event.row, event.column);
                    self.selection_state = Some(SelectionState::Dragging {
                        sel: Selection::start(pos, zone.area, zone.zone),
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

    pub(super) fn handle_scroll(&mut self, column: u16, row: u16, delta: i32) {
        let drag_zone = match self.selection_state {
            Some(SelectionState::Dragging { ref sel, .. }) => Some(sel.zone),
            _ => None,
        };
        match self.scroll_at(column, row, delta) {
            Some(zone) if drag_zone == Some(zone) => self.drag_selection_to(row, column),
            _ => self.clear_selection_unless_pending_copy(),
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
            self.update_selection_to_edge(col);
        } else {
            if let Some(SelectionState::Dragging { edge_scroll, .. }) = &mut self.selection_state {
                *edge_scroll = None;
            }
            self.drag_selection_to(row, col);
        }
    }

    /// Moves the drag cursor, always in the zone and area the drag started in.
    /// Reading the document position needs `&self` while writing it needs
    /// `&mut self`, hence the two matches.
    fn drag_selection_to(&mut self, row: u16, col: u16) {
        let Some(SelectionState::Dragging { ref sel, .. }) = self.selection_state else {
            return;
        };
        let pos = self.doc_pos(sel.zone, sel.area, row, col);
        if let Some(SelectionState::Dragging { sel, .. }) = &mut self.selection_state {
            sel.update(pos);
        }
    }

    fn update_selection_to_edge(&mut self, col: u16) {
        let Some(SelectionState::Dragging {
            ref sel,
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
        self.drag_selection_to(edge_row, col);
    }

    pub fn tick_edge_scroll(&mut self) -> Dirty {
        let (dir, zone, col) = match self.selection_state {
            Some(SelectionState::Dragging {
                ref sel,
                ref mut edge_scroll,
                last_drag_col,
            }) => {
                let Some(es) = edge_scroll else {
                    return Dirty::NO;
                };
                if es.last_tick.elapsed() < EDGE_SCROLL_INTERVAL {
                    return Dirty::NO;
                }
                let dir = es.dir;
                es.last_tick = Instant::now();
                (dir, sel.zone, last_drag_col)
            }
            _ => return Dirty::NO,
        };

        self.scroll_zone(zone, dir);
        self.update_selection_to_edge(col);
        Dirty::YES
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
                let Some(screen_sel) = self.screen_selection(sel, render_chat) else {
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
                let Some(screen_sel) = self.screen_selection(sel, render_chat) else {
                    self.selection_state = None;
                    return;
                };
                let mut area = sel.area;
                if scrollbar::is_enabled() {
                    area.width = area.width.saturating_sub(1);
                }
                let regions = [ContentRegion {
                    area,
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

    /// The one place a screen position becomes a document position. The
    /// transcript's document is segments, every other zone's is a flat list of
    /// rows starting at that zone's own scroll offset.
    pub(super) fn doc_pos(&self, zone: SelectionZone, area: Rect, row: u16, col: u16) -> DocPos {
        let rel = selection::row_in_area(row, area);
        let col = selection::clamp_col(col, area);
        match zone {
            SelectionZone::Messages => self.chats[self.active_chat].doc_pos_at(rel, col),
            SelectionZone::Input => {
                DocPos::flat(self.input_box.scroll_y().saturating_add(rel), col)
            }
            SelectionZone::Overlay => DocPos::flat(rel, col),
        }
    }

    pub(super) fn screen_selection(&self, sel: &Selection, chat: usize) -> Option<ScreenSelection> {
        match sel.zone {
            SelectionZone::Messages => sel.to_screen(|pos| self.chats[chat].project_row(pos)),
            SelectionZone::Input => {
                sel.to_screen(|pos| RowPos::flat(pos, sel.area, self.input_box.scroll_y()))
            }
            SelectionZone::Overlay => sel.to_screen(|pos| RowPos::flat(pos, sel.area, 0)),
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
