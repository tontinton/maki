use std::sync::atomic::Ordering;

use crate::components::Overlay;
#[cfg(test)]
use crate::components::keybindings::KeybindContext;
use crate::components::queue_panel;
use crate::components::split_layout::{MIN_CHAT_ROWS, SplitLayout, carve};
use crate::components::status_bar::{StatusBarContext, UsageStats};
use crate::selection::{self, SelectableZone, SelectionZone};
use crate::theme;
use maki_lua::Split;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};

use super::{App, Mode, Status};

struct ViewLayout {
    msg_area: Rect,
    bottom_area: Rect,
    status_area: Rect,
    queue_area: Rect,
    panel_windows: Vec<(usize, Rect)>,
    input_area: Rect,
    splits: SplitLayout,
}

impl App {
    pub fn view(&mut self, frame: &mut Frame) {
        self.status_bar.clear_expired_hint();

        let in_plan = self.state.mode == Mode::Plan;
        let form_visible =
            self.permission_prompt.is_open() || (in_plan && self.plan_form.is_visible());
        let layout = self.compute_layout(frame.area(), form_visible);
        let render_chat = self.resolve_render_chat();

        self.render_background(frame);
        self.render_messages(frame, &layout, render_chat);
        self.render_bottom_panel(frame, &layout);
        self.render_splits(frame, &layout);
        let mut overlay_rect = self.render_picker_overlays(frame, &layout);
        self.render_status_bar(frame, layout.status_area, render_chat);
        overlay_rect = self.render_top_modals(frame, overlay_rect);
        self.register_zones(&layout, overlay_rect);
        self.apply_selection(frame, render_chat);
    }

    fn compute_layout(&self, area: Rect, form_visible: bool) -> ViewLayout {
        let permission_open = self.permission_prompt.is_open();

        // Carve the full-width status bar first so the split carving below only
        // ever deals with the content region above it.
        let [content, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

        // The permission prompt owns the bottom area, so drop any `below` split
        // here at the source. That keeps "prompt wins bottom" in one filter
        // instead of needing a fix-up further down.
        let reqs: Vec<_> = self
            .float_mgr
            .split_reqs(content)
            .into_iter()
            .filter(|r| !(permission_open && r.split == Split::Below))
            .collect();
        let splits = carve(content, &reqs);
        let inner = splits.inner;

        let below_active = splits.rect(Split::Below).is_some();
        let bottom_takeover = form_visible || below_active;
        let max_bottom = inner.height.saturating_sub(MIN_CHAT_ROWS);
        let bottom_height = if permission_open {
            self.permission_prompt.height(inner.width).min(max_bottom)
        } else if below_active {
            0
        } else if form_visible {
            self.plan_form.height().min(max_bottom)
        } else if self.is_main_chat() {
            let panel_h: u16 = self.float_mgr.panel_reqs().iter().map(|(_, h)| *h).sum();
            queue_panel::height(self.queue.panel_len())
                + panel_h
                + self.input_box.height(inner.width)
        } else {
            let panel_h: u16 = self.float_mgr.panel_reqs().iter().map(|(_, h)| *h).sum();
            if panel_h > 0 { panel_h + 1 } else { 1 }
        };

        // The `below` split lives outside `inner` (drawn by render_splits), so
        // the bottom panel only ever splits the chat region.
        let [msg_area, bottom_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(bottom_height)]).areas(inner);

        let panel_reqs = if bottom_takeover {
            Vec::new()
        } else {
            self.float_mgr.panel_reqs()
        };

        let (queue_height, _input_height) = if bottom_takeover {
            (0, 0)
        } else {
            let queue_height = queue_panel::height(self.queue.panel_len());
            let panel_h: u16 = panel_reqs.iter().map(|(_, h)| *h).sum();
            let input_height = bottom_area.height.saturating_sub(queue_height + panel_h);
            (queue_height, input_height)
        };

        let mut constraints = vec![Constraint::Length(queue_height)];
        for &(_, h) in &panel_reqs {
            constraints.push(Constraint::Length(h));
        }
        constraints.push(Constraint::Min(1));

        let areas = Layout::vertical(constraints).split(bottom_area);
        let queue_area = areas[0];
        let panel_windows: Vec<(usize, Rect)> = panel_reqs
            .iter()
            .enumerate()
            .map(|(i, &(idx, _))| (idx, areas[1 + i]))
            .collect();
        let input_area = areas[areas.len() - 1];

        ViewLayout {
            msg_area,
            bottom_area,
            status_area,
            queue_area,
            panel_windows,
            input_area,
            splits,
        }
    }

    fn resolve_render_chat(&self) -> usize {
        if self.task_picker.is_open() {
            self.task_picker
                .selected_index()
                .unwrap_or(self.active_chat)
        } else {
            self.active_chat
        }
    }

    fn render_background(&self, frame: &mut Frame) {
        let bg =
            Block::default().style(ratatui::style::Style::new().bg(theme::current().background));
        bg.render(frame.area(), frame.buffer_mut());
    }

    fn render_messages(&mut self, frame: &mut Frame, layout: &ViewLayout, render_chat: usize) {
        let accent = self.effective_mode_color();
        self.chats[render_chat].set_accent(accent);
        self.chats[render_chat].view(frame, layout.msg_area, self.selection_state.is_some());
    }

    fn render_bottom_panel(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        let in_plan = self.state.mode == Mode::Plan;
        if self.permission_prompt.is_open() {
            self.permission_prompt.view(frame, layout.bottom_area);
        } else if !self.is_main_chat() {
            let panel_reqs = self.float_mgr.panel_reqs();
            let panel_h: u16 = panel_reqs.iter().map(|(_, h)| *h).sum();
            let (panel_areas, sep_area) = if panel_h > 0 {
                let [panels, s] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
                    .areas(layout.bottom_area);
                let constraints: Vec<_> = panel_reqs
                    .iter()
                    .map(|&(_, h)| Constraint::Length(h))
                    .collect();
                let sub = Layout::vertical(constraints).split(panels);
                let areas: Vec<(usize, Rect)> = panel_reqs
                    .iter()
                    .enumerate()
                    .map(|(i, &(idx, _))| (idx, sub[i]))
                    .collect();
                (Some(areas), s)
            } else {
                (None, layout.bottom_area)
            };
            if let Some(areas) = panel_areas {
                for (idx, rect) in areas {
                    self.float_mgr.view_panel(frame, idx, rect);
                }
            }
            let sep = Block::default()
                .borders(Borders::TOP)
                .border_style(self.separator_style());
            frame.render_widget(sep, sep_area);
        } else if in_plan && self.plan_form.is_visible() {
            self.plan_form.view(frame, layout.bottom_area);
        } else if layout.bottom_area.height > 0 {
            let queue_entries = self.queue.panel_entries();
            queue_panel::view(frame, layout.queue_area, &queue_entries, self.queue.focus());
            for &(idx, rect) in &layout.panel_windows {
                self.float_mgr.view_panel(frame, idx, rect);
            }
            let streaming = self.status == Status::Streaming;
            let panel_hint = in_plan
                .then(|| self.plan_form.hint_line())
                .flatten()
                .or_else(|| self.lua_hint_line());
            self.input_box.view(
                frame,
                layout.input_area,
                streaming,
                self.separator_style(),
                !self.any_overlay_open(),
                panel_hint,
            );
            self.command_palette.view(frame, layout.input_area);
        }
    }

    fn render_splits(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        for dir in Split::ALL {
            if let Some(rect) = layout.splits.rect(dir) {
                self.float_mgr.view_split(frame, dir, rect);
            }
        }
    }

    fn render_picker_overlays(&mut self, frame: &mut Frame, layout: &ViewLayout) -> Rect {
        let mut overlay_rect = Rect::default();
        let full = frame.area();

        if self.search_modal.is_open() {
            overlay_rect = self.search_modal.view(frame, layout.msg_area);
        }

        if self.task_picker.is_open() {
            overlay_rect = self.task_picker.view(frame, full);
        }

        if self.file_picker.is_open() {
            if let Some(flash) = self.file_picker.tick() {
                self.status_bar.flash(flash);
            }
            overlay_rect = self.file_picker.view(frame, full);
        }

        if self.session_picker.is_open() {
            self.session_picker.tick();
            overlay_rect = self.session_picker.view(frame, full);
            if let Some(flash) = self.session_picker.take_flash() {
                self.status_bar.flash(flash);
            }
        }

        macro_rules! render_if_open {
            ($overlay:expr) => {
                if $overlay.is_open() {
                    overlay_rect = $overlay.view(frame, full);
                }
            };
        }

        render_if_open!(self.rewind_picker);
        render_if_open!(self.theme_picker);
        render_if_open!(self.model_picker);
        render_if_open!(self.mcp_picker);

        overlay_rect
    }

    fn render_top_modals(&mut self, frame: &mut Frame, mut overlay_rect: Rect) -> Rect {
        let full = frame.area();
        let r = self.btw_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        let r = self.help_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        let r = self.float_mgr.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        overlay_rect
    }

    fn render_status_bar(&mut self, frame: &mut Frame, status_area: Rect, render_chat: usize) {
        let chat = &self.chats[render_chat];
        let chat_name = (self.chats.len() > 1).then_some(chat.name.as_str());
        let (mode_label, mode_style) = self.mode_label();
        let ctx = StatusBarContext {
            status: &self.status,
            mode_label,
            mode_style,
            model_id: chat
                .model_id
                .as_deref()
                .unwrap_or(&self.state.session.model),
            stats: UsageStats {
                usage: &chat.token_usage,
                global_usage: &self.state.token_usage,
                context_size: chat.context_size,
                pricing: &self.state.model.pricing,
                context_window: self.state.model.context_window,
                show_global: self.chats.len() > 1,
            },
            auto_scroll: chat.auto_scroll(),
            chat_name,
            retry_info: self.retry_info.as_ref(),
            thinking_label: self.state.thinking.status_label(),
            fast: self.state.fast,
            restoring: self.restoring.load(Ordering::Relaxed),
        };
        self.status_bar.view(frame, status_area, &ctx);
    }

    fn register_zones(&mut self, layout: &ViewLayout, overlay_rect: Rect) {
        self.zones[SelectionZone::Messages.idx()] = Some(SelectableZone {
            area: layout.msg_area,
            highlight_area: Rect::new(
                layout.msg_area.x,
                layout.msg_area.y,
                layout.msg_area.width.saturating_sub(1),
                layout.msg_area.height,
            ),
            zone: SelectionZone::Messages,
        });

        if layout.input_area.height > 0 {
            let input_inner = Rect::new(
                layout.input_area.x,
                layout.input_area.y + 1,
                layout.input_area.width,
                layout.input_area.height.saturating_sub(2),
            );
            self.zones[SelectionZone::Input.idx()] = Some(SelectableZone {
                area: input_inner,
                highlight_area: input_inner,
                zone: SelectionZone::Input,
            });
        } else {
            self.zones[SelectionZone::Input.idx()] = None;
        }

        self.zones[SelectionZone::StatusBar.idx()] = Some(SelectableZone {
            area: layout.status_area,
            highlight_area: layout.status_area,
            zone: SelectionZone::StatusBar,
        });

        if overlay_rect.width > 0 {
            let inner = selection::inset_border(overlay_rect);
            self.zones[SelectionZone::Overlay.idx()] = Some(SelectableZone {
                area: inner,
                highlight_area: inner,
                zone: SelectionZone::Overlay,
            });
        } else {
            self.zones[SelectionZone::Overlay.idx()] = None;
            if self
                .selection_state
                .as_ref()
                .is_some_and(|s| s.sel().zone == SelectionZone::Overlay)
            {
                self.selection_state = None;
            }
        }
    }

    fn apply_selection(&mut self, frame: &mut Frame, render_chat: usize) {
        let Some(ref state) = self.selection_state else {
            return;
        };

        let sel = state.sel();
        let zone = sel.zone;
        let scroll = self.scroll_offset(zone);
        if let Some(screen_sel) = sel.to_screen(scroll) {
            let highlight_area = self.zones[zone.idx()]
                .map(|z| z.highlight_area)
                .unwrap_or_default();
            selection::apply_highlight(frame.buffer_mut(), highlight_area, &screen_sel);
        }
        if state.is_pending_copy() {
            let sel = *sel;
            self.copy_selection(frame.buffer_mut(), &sel, render_chat);
        }
    }

    /// Layout geometry for tests: `(msg_area, bottom_area, status_area,
    /// input_area, splits)`.
    #[cfg(test)]
    pub(super) fn layout_geometry(&self, area: Rect) -> (Rect, Rect, Rect, Rect, SplitLayout) {
        let in_plan = self.state.mode == Mode::Plan;
        let form_visible =
            self.permission_prompt.is_open() || (in_plan && self.plan_form.is_visible());
        let layout = self.compute_layout(area, form_visible);
        (
            layout.msg_area,
            layout.bottom_area,
            layout.status_area,
            layout.input_area,
            layout.splits,
        )
    }

    fn lua_hint_line(&self) -> Option<Line<'static>> {
        let snap = self.hint_reader.load();
        if snap.entries.is_empty() {
            return None;
        }
        let mut spans = Vec::new();
        for (_, pairs) in &snap.entries {
            for (text, style_name) in pairs {
                let style = theme::style_by_name(style_name);
                spans.push(Span::styled(text.clone(), style));
            }
        }
        Some(Line::from(spans))
    }

    #[cfg(test)]
    pub(super) fn active_keybind_contexts(&self) -> Vec<KeybindContext> {
        let mut contexts = vec![KeybindContext::General];
        if self.state.mode == Mode::Plan && self.plan_form.is_visible() {
            contexts.push(KeybindContext::FormInput);
        } else if self.queue.focus().is_some() {
            contexts.push(KeybindContext::QueueFocus);
        } else if self.session_picker.is_open() {
            contexts.push(KeybindContext::SessionPicker);
        } else if self.rewind_picker.is_open() {
            contexts.push(KeybindContext::RewindPicker);
        } else if self.task_picker.is_open() {
            contexts.push(KeybindContext::TaskPicker);
        } else if self.theme_picker.is_open() {
            contexts.push(KeybindContext::ThemePicker);
        } else if self.model_picker.is_open() {
            contexts.push(KeybindContext::ModelPicker);
        } else if self.command_palette.is_active() {
            contexts.push(KeybindContext::CommandPalette);
        } else if self.search_modal.is_open() {
            contexts.push(KeybindContext::Search);
        } else if self.file_picker.is_open() {
            contexts.push(KeybindContext::FilePicker);
        } else {
            if self.status == Status::Streaming {
                contexts.push(KeybindContext::Streaming);
            }
            contexts.push(KeybindContext::Editing);
        }
        contexts
    }
}
