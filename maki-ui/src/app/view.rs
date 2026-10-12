use std::sync::atomic::Ordering;

use crate::components::Overlay;
use crate::components::input::{BORDER_ROWS, Placeholder};
#[cfg(test)]
use crate::components::keybindings::KeybindContext;
use crate::components::queue_panel;
use crate::components::split_layout::{MIN_CHAT_ROWS, carve};
use crate::components::status_bar::{StatusBarContext, UsageStats};
use crate::components::usage_modal::UsageModalContext;
use crate::selection::{self, SelectableZone, SelectionZone, ZoneRegistry};
use crate::theme;
#[cfg(test)]
use maki_lua::Split;
use maki_providers::RequestOptions;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};

use super::{App, Mode, Status};

struct ViewLayout {
    msg_area: Rect,
    bottom_area: Rect,
    status_area: Rect,
    queue_area: Rect,
    /// Bottom docks, in the input column, as `(window index, rect)`.
    bottom_windows: Vec<(usize, Rect)>,
    /// Edge docks carved off the content, as `(window index, rect)`.
    dock_windows: Vec<(usize, Rect)>,
    input_area: Rect,
    bottom_takeover: bool,
}

/// Every dock drawn this frame, keyed by direction, for tests to assert on.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DockRects(Vec<(Split, Rect)>);

#[cfg(test)]
impl DockRects {
    pub(super) fn rect(&self, split: Split) -> Option<Rect> {
        self.0.iter().find(|(s, _)| *s == split).map(|(_, r)| *r)
    }
}

impl App {
    fn prompt_open(&self) -> bool {
        self.pack_review.is_open() || self.permission_prompt.is_open()
    }

    pub(super) fn form_visible(&self) -> bool {
        self.prompt_open() || self.plan_form_active()
    }

    /// Whether the chat input is the widget the user's keys reach. It decides
    /// the block cursor and the terminal cursor, never where the caret was
    /// drawn.
    fn input_has_keyboard(&self) -> bool {
        !self.any_overlay_open()
    }

    /// Whether a plugin writing to the chat input would land in a box the
    /// user can see: nothing covering it, no form in the bottom panel, and a
    /// {area} tall enough to leave the box a text row. An overlay counts even when it only takes the keyboard, because
    /// the user is reading it and not the draft.
    ///
    /// Worked out from state on every ask rather than recorded while
    /// painting: a whole batch of wakes is handled between two frames, so a
    /// permission prompt and a plugin's edit can arrive in the same one and
    /// the edit has to meet the prompt that is already open.
    pub(crate) fn input_live(&self, area: Rect) -> bool {
        self.chat_accepts_input()
            && !self.any_overlay_open()
            && !self.plan_form_active()
            && self.compute_layout(area).input_area.height > BORDER_ROWS
    }

    /// Returns the cell the terminal cursor belongs on, the cell the input box
    /// reversed. An overlay owning the keyboard leaves none, so there is
    /// nothing to park an IME on.
    ///
    /// Where the caret was drawn is a separate question, and a caret-anchored
    /// float is placed from that: the caret outlives an overlay taking focus,
    /// or a window on it would jump to the middle of the screen the moment it
    /// started reading keys.
    pub fn view(&mut self, frame: &mut Frame) -> Option<Position> {
        let layout = self.compute_layout(frame.area());
        let render_chat = self.active_chat;

        self.render_background(frame);
        self.render_messages(frame, &layout, render_chat);
        let caret = self.render_bottom_panel(frame, &layout);
        self.render_splits(frame, &layout);
        let mut overlay_rect = self.render_picker_overlays(frame, &layout);
        self.render_status_bar(frame, layout.status_area, render_chat);
        overlay_rect = self.render_top_modals(frame, overlay_rect, caret);
        self.register_zones(&layout, overlay_rect);
        self.apply_selection(frame, render_chat);
        caret.filter(|_| self.input_has_keyboard())
    }

    fn compute_layout(&self, area: Rect) -> ViewLayout {
        // Carve the full-width status bar first so the split carving below only
        // ever deals with the content region above it.
        let [content, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

        // Edge docks (above/left/right) carve off the content. Bottom docks
        // live in the input column below, so they never reach `carve`.
        let dock_reqs = self.float_mgr.dock_reqs(content);
        let reqs: Vec<_> = dock_reqs.iter().map(|(_, req)| *req).collect();
        let splits = carve(content, &reqs);
        let inner = splits.inner;
        let dock_windows: Vec<(usize, Rect)> = dock_reqs
            .iter()
            .zip(splits.rects())
            .filter_map(|((idx, _), rect)| rect.map(|rect| (*idx, rect)))
            .collect();

        let hides_input = self.float_mgr.hides_input();
        let bottom_takeover = self.form_visible() || hides_input;
        let max_bottom = inner.height.saturating_sub(MIN_CHAT_ROWS);

        let bottom_reqs = if self.form_visible() {
            Vec::new()
        } else {
            self.float_mgr.bottom_reqs()
        };
        let bottom_h: u16 = bottom_reqs.iter().map(|(_, h)| *h).sum();
        let queue_height = if self.form_visible() || hides_input || !self.chat_accepts_input() {
            0
        } else {
            queue_panel::height(self.active_queue_len())
        };
        let input_shown = self.chat_accepts_input() && !hides_input && !self.form_visible();
        let input_h = if input_shown {
            self.input_box.height(inner.width)
        } else {
            0
        };

        let bottom_height = if self.permission_prompt.is_open() {
            self.permission_prompt.height(inner.width).min(max_bottom)
        } else if self.pack_review.is_open() {
            self.pack_review.height(inner.width).min(max_bottom)
        } else if self.plan_form_active() {
            self.plan_form.height(max_bottom).min(max_bottom)
        } else if self.chat_accepts_input() {
            (queue_height + bottom_h + input_h).min(max_bottom)
        } else if bottom_h > 0 {
            bottom_h + 1
        } else {
            1
        };

        // The bottom column is `[queue, bottom docks..., input]`. A dock that
        // hides the input drops that last row, so it reaches the screen edge.
        let [msg_area, bottom_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(bottom_height)]).areas(inner);

        let mut constraints = vec![Constraint::Length(queue_height)];
        for &(_, h) in &bottom_reqs {
            constraints.push(Constraint::Length(h));
        }
        // The tail row is the input box, or a one-row separator when the chat
        // takes no input (a finished subagent transcript).
        let tail = if input_shown {
            Constraint::Min(1)
        } else if !self.chat_accepts_input() && !bottom_reqs.is_empty() {
            Constraint::Length(1)
        } else {
            Constraint::Length(0)
        };
        constraints.push(tail);

        let areas = Layout::vertical(constraints).split(bottom_area);
        let queue_area = areas[0];
        let bottom_windows: Vec<(usize, Rect)> = bottom_reqs
            .iter()
            .enumerate()
            .map(|(n, &(idx, _))| (idx, areas[1 + n]))
            .collect();
        let input_area = areas[areas.len() - 1];

        ViewLayout {
            msg_area,
            bottom_area,
            status_area,
            queue_area,
            bottom_windows,
            dock_windows,
            input_area,
            bottom_takeover,
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
        let images_visible = !self.any_overlay_open();
        self.chats[render_chat].view(
            frame,
            layout.msg_area,
            self.selection_state.is_some(),
            images_visible,
            self.command_palette.lua_commands(),
        );
    }

    /// Returns the cell the input box drew its caret in, when it drew one at
    /// all: a prompt, a form, a `below` split or a finished subagent's chat
    /// all take the box off screen, and a box scrolled off its own viewport
    /// draws without a caret. A running subagent's chat keeps the box, so a
    /// message can be queued for that subagent while watching it.
    fn render_bottom_panel(&mut self, frame: &mut Frame, layout: &ViewLayout) -> Option<Position> {
        if self.permission_prompt.is_open() {
            self.permission_prompt.view(frame, layout.bottom_area);
        } else if self.pack_review.is_open() {
            self.pack_review.view(frame, layout.bottom_area);
        } else if !self.chat_accepts_input() {
            for &(idx, rect) in &layout.bottom_windows {
                self.float_mgr.view_dock(frame, idx, rect);
            }
            let sep_area = if layout.bottom_windows.is_empty() {
                layout.bottom_area
            } else {
                layout.input_area
            };
            let sep = Block::default()
                .borders(Borders::TOP)
                .border_style(self.separator_style());
            frame.render_widget(sep, sep_area);
        } else if self.plan_form_active() {
            self.plan_form.view(frame, layout.bottom_area);
        } else if layout.bottom_area.height > 0 {
            let queue_entries = self.active_queue_entries();
            let focus = self.queue.focus().filter(|_| self.is_main_chat());
            queue_panel::view(frame, layout.queue_area, &queue_entries, focus);
            for &(idx, rect) in &layout.bottom_windows {
                self.float_mgr.view_dock(frame, idx, rect);
            }
            if self.float_mgr.hides_input() {
                return None;
            }
            let placeholder = if !self.is_main_chat() {
                Placeholder::SubagentQueue
            } else if self.status == Status::Streaming {
                Placeholder::Queue
            } else if self.state.session.messages().is_empty() {
                Placeholder::Suggestion
            } else {
                Placeholder::Blank
            };
            let panel_hint = (self.state.mode == Mode::Plan)
                .then(|| self.plan_form.hint_line())
                .flatten()
                .or_else(|| self.lua_hint_line());
            let caret = self.input_box.view(
                frame,
                layout.input_area,
                placeholder,
                self.separator_style(),
                self.input_has_keyboard(),
                panel_hint,
            );
            self.command_palette
                .view(frame, layout.input_area, &self.input_box.buffer);
            return caret;
        }
        None
    }

    fn render_splits(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        for &(idx, rect) in &layout.dock_windows {
            self.float_mgr.view_dock(frame, idx, rect);
        }
    }

    fn render_picker_overlays(&mut self, frame: &mut Frame, layout: &ViewLayout) -> Rect {
        let mut overlay_rect = Rect::default();
        let full = frame.area();

        if self.search_modal.is_open() {
            overlay_rect = self.search_modal.view(frame, layout.msg_area);
        }

        if self.file_picker.is_open() {
            overlay_rect = self.file_picker.view(frame, full);
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
        render_if_open!(self.login_picker);
        render_if_open!(self.mcp_picker);

        overlay_rect
    }

    /// {caret} is the cell `render_bottom_panel` just drew the input caret in,
    /// so a float anchored to it is placed against the frame being painted.
    fn render_top_modals(
        &mut self,
        frame: &mut Frame,
        mut overlay_rect: Rect,
        caret: Option<Position>,
    ) -> Rect {
        let full = frame.area();
        let r = self.btw_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        let r = self.help_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        if self.usage_modal.is_open() {
            let ctx = UsageModalContext {
                total: &self.state.token_usage,
                total_cost: self.state.cost,
                total_list_cost: self.state.subsidised_list_cost,
                by_model: self.state.session.usage_by_model(),
                model: &self.state.model,
                fast: self.state.fast,
                clock_format: self.ui_config.clock_format,
            };
            let r = self.usage_modal.view(frame, full, &ctx);
            if r.width > 0 {
                overlay_rect = r;
            }
        }
        let r = self.float_mgr.view(frame, full, caret);
        if r.width > 0 {
            overlay_rect = r;
        }
        let r = self.alert_modal.view(frame, full);
        if r.width > 0 {
            overlay_rect = r;
        }
        overlay_rect
    }

    fn render_status_bar(&mut self, frame: &mut Frame, status_area: Rect, render_chat: usize) {
        let chat = &self.chats[render_chat];
        let chat_name = (self.chats.len() > 1).then_some(chat.name.as_str());
        let opts = chat.opts.unwrap_or(RequestOptions {
            thinking: self.state.thinking,
            fast: self.state.fast,
        });
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
                global_cost: self.state.cost,
                context_size: chat.context_size,
                cost: chat.cost,
                list_cost: chat.list_cost,
                subsidy_source: self.state.model.subsidised_by.clone(),
                context_window: self.state.model.context_window,
                show_global: self.chats.len() > 1,
            },
            auto_scroll: chat.auto_scroll(),
            chat_name,
            retry_info: self.retry_info.as_ref(),
            thinking_label: opts.thinking.status_label(),
            fast: opts.fast,
            workflow: self.state.workflow,
            restricted: self.trust_question.is_some(),
            yolo: self.permissions.is_yolo(),
            restoring: self.restoring.load(Ordering::Relaxed),
        };
        self.status_bar.view(frame, status_area, &ctx);
    }

    fn register_zones(&mut self, layout: &ViewLayout, overlay_rect: Rect) {
        // Push order = z-order. zone_at() walks in reverse, so later entries win.
        self.zones = ZoneRegistry::new();

        self.zones.push(SelectableZone {
            area: layout.msg_area,
            zone: SelectionZone::Messages,
        });

        if layout.input_area.height > 0 && !layout.bottom_takeover && self.chat_accepts_input() {
            let input_inner = Rect::new(
                layout.input_area.x,
                layout.input_area.y + 1,
                layout.input_area.width,
                layout.input_area.height.saturating_sub(2),
            );
            self.zones.push(SelectableZone {
                area: input_inner,
                zone: SelectionZone::Input,
            });
        }

        self.zones.push_overlay(layout.status_area);

        if self.form_visible() || !self.chat_accepts_input() {
            self.zones.push_overlay(layout.bottom_area);
        }

        for &(_, rect) in &layout.bottom_windows {
            self.zones.push_overlay(selection::inset_border(rect));
        }

        if layout.queue_area.height > 0 && !layout.bottom_takeover {
            self.zones.push_overlay(layout.queue_area);
        }

        for &(_, rect) in &layout.dock_windows {
            self.zones.push_overlay(selection::inset_border(rect));
        }

        if overlay_rect.width > 0 {
            self.zones
                .push_overlay(selection::inset_border(overlay_rect));
        }

        // Overlay zone was removed (e.g. dialog closed), drop the dangling selection
        if let Some(ref state) = self.selection_state
            && state.sel().zone == SelectionZone::Overlay
            && self.zones.find_area(state.sel().area).is_none()
        {
            self.selection_state = None;
        }
    }

    fn apply_selection(&mut self, frame: &mut Frame, render_chat: usize) {
        let Some(ref state) = self.selection_state else {
            return;
        };

        let sel = state.sel();
        if let Some(screen_sel) = self.screen_selection(sel, render_chat) {
            selection::apply_highlight(frame.buffer_mut(), sel.highlight_area(), &screen_sel);
        }
        if state.is_pending_copy() {
            let sel = *sel;
            self.copy_selection(frame.buffer_mut(), &sel, render_chat);
        }
    }

    /// Layout geometry for tests: `(msg_area, bottom_area, status_area,
    /// input_area, docks)`.
    #[cfg(test)]
    pub(super) fn layout_geometry(&self, area: Rect) -> (Rect, Rect, Rect, Rect, DockRects) {
        let layout = self.compute_layout(area);
        let docks = layout
            .dock_windows
            .iter()
            .chain(layout.bottom_windows.iter())
            .map(|&(idx, rect)| (self.float_mgr.split_of(idx), rect))
            .collect();
        (
            layout.msg_area,
            layout.bottom_area,
            layout.status_area,
            layout.input_area,
            DockRects(docks),
        )
    }

    fn lua_hint_line(&self) -> Option<Line<'static>> {
        let snap = self.hints.get()?;
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
    pub(super) fn active_keybind_contexts(&mut self) -> Vec<KeybindContext> {
        let mut contexts = vec![KeybindContext::General];
        if self.pack_review.is_open() || self.plan_form_active() {
            contexts.push(KeybindContext::FormInput);
        } else if self.queue.focus().is_some() {
            contexts.push(KeybindContext::QueueFocus);
        } else if self.rewind_picker.is_open() {
            contexts.push(KeybindContext::RewindPicker);
        } else if self.theme_picker.is_open() {
            contexts.push(KeybindContext::ThemePicker);
        } else if self.model_picker.is_open() {
            contexts.push(KeybindContext::ModelPicker);
        } else if self.command_palette.is_active(&self.input_box.buffer) {
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
