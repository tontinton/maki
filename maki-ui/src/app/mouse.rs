use std::time::{Duration, Instant};

use crate::selection::{self, ContentRegion, EdgeScroll, SelectableZone, Selection, SelectionZone};
#[cfg(target_os = "linux")]
use arboard::{LinuxClipboardKind, SetExtLinux};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

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
                    let scroll = self.scroll_offset(zone.zone);
                    self.selection_state = Some(crate::selection::SelectionState {
                        sel: Selection::start(
                            event.row,
                            event.column,
                            zone.area,
                            zone.zone,
                            scroll,
                        ),
                        copy_on_release: false,
                        edge_scroll: None,
                        last_drag_col: event.column,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_drag(event.row, event.column);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(ref mut state) = self.selection_state {
                    state.edge_scroll = None;
                    if !state.sel.is_empty() {
                        state.copy_on_release = true;
                    } else {
                        let zone = state.sel.zone;
                        self.selection_state = None;
                        if zone == SelectionZone::Messages {
                            let area = self.msg_area();
                            self.chats[self.active_chat].toggle_expansion_at(event.row, area);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_drag(&mut self, row: u16, col: u16) {
        let Some(ref mut state) = self.selection_state else {
            return;
        };
        let zone = state.sel.zone;
        let area = state.sel.area;
        state.last_drag_col = col;

        let at_top = row <= area.y;
        let at_bottom = row + 1 >= area.bottom();

        if at_top || at_bottom {
            let dir = if at_top {
                EDGE_SCROLL_LINES
            } else {
                -EDGE_SCROLL_LINES
            };
            let first_edge_hit = state.edge_scroll.is_none();
            if let Some(ref mut es) = state.edge_scroll {
                es.dir = dir;
            } else {
                state.edge_scroll = Some(EdgeScroll {
                    dir,
                    last_tick: Instant::now(),
                });
            }
            if first_edge_hit {
                self.scroll_zone(zone, dir);
            }
            self.update_selection_to_edge(zone, col);
        } else {
            let state = self.selection_state.as_mut().expect("checked above");
            state.edge_scroll = None;
            let scroll = self.scroll_offset(zone);
            let state = self.selection_state.as_mut().expect("checked above");
            state.sel.update(row, col, scroll);
        }
    }

    fn update_selection_to_edge(&mut self, zone: SelectionZone, col: u16) {
        let scroll = self.scroll_offset(zone);
        let state = self.selection_state.as_mut().expect("caller ensures Some");
        let edge_row = if state.edge_scroll.as_ref().is_some_and(|es| es.dir > 0) {
            state.sel.area.y
        } else {
            state.sel.area.bottom().saturating_sub(1)
        };
        state.sel.update(edge_row, col, scroll);
    }

    pub fn tick_edge_scroll(&mut self) {
        let Some(ref mut state) = self.selection_state else {
            return;
        };
        let Some(ref mut es) = state.edge_scroll else {
            return;
        };
        if es.last_tick.elapsed() < EDGE_SCROLL_INTERVAL {
            return;
        }
        let dir = es.dir;
        let zone = state.sel.zone;
        let last_drag_col = state.last_drag_col;
        es.last_tick = Instant::now();

        self.scroll_zone(zone, dir);
        self.update_selection_to_edge(zone, last_drag_col);
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
            SelectionZone::StatusBar | SelectionZone::Overlay => {
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

        if !text.is_empty() {
            match &mut self.clipboard {
                Some(cb) => {
                    #[cfg(target_os = "linux")]
                    let result = cb
                        .set()
                        .clipboard(LinuxClipboardKind::Primary)
                        .text(&text)
                        .and_then(|()| {
                            cb.set().clipboard(LinuxClipboardKind::Clipboard).text(text)
                        });

                    #[cfg(not(target_os = "linux"))]
                    let result = cb.set_text(&text);

                    match result {
                        Ok(()) => self.status_bar.flash("Copied selection".into()),
                        Err(e) => self.status_bar.flash(format!("Copy failed: {e}")),
                    }
                }
                None => self.status_bar.flash("Copy failed: no clipboard".into()),
            }
        }
        self.selection_state = None;
    }

    pub(super) fn zone_at(&self, row: u16, col: u16) -> Option<SelectableZone> {
        selection::zone_at(&self.zones, row, col)
    }

    pub(super) fn scroll_offset(&self, zone: SelectionZone) -> u32 {
        match zone {
            SelectionZone::Messages => self.chats[self.active_chat].scroll_top() as u32,
            SelectionZone::Input => self.input_box.scroll_y() as u32,
            SelectionZone::StatusBar | SelectionZone::Overlay => 0,
        }
    }

    pub(super) fn scroll_zone(&mut self, zone: SelectionZone, delta: i32) {
        match zone {
            SelectionZone::Messages => self.chats[self.active_chat].scroll(delta),
            SelectionZone::Input => self.input_box.scroll(delta),
            SelectionZone::StatusBar | SelectionZone::Overlay => {}
        }
    }

    pub(super) fn msg_area(&self) -> Rect {
        self.zones[SelectionZone::Messages.idx()]
            .map(|z| z.highlight_area)
            .unwrap_or_default()
    }
}
