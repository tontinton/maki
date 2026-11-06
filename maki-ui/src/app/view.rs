use crate::components::Overlay;
use crate::components::input::BORDER_ROWS;
use crate::components::queue_panel;
use crate::components::scrollbar::Thumbs;
use crate::components::split_layout::{MIN_CHAT_ROWS, SplitLayout, carve};
use crate::components::status_bar::{StatusBarContext, UsageStats};
use crate::components::usage_modal::UsageModalContext;
use crate::selection::{self, SelectableZone, SelectionZone, ZoneRegistry};
use crate::theme;
use maki_lua::{CursorStyle, Split};
use maki_providers::RequestOptions;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};

use super::{App, KeyboardOwner, Mode};

/// A tab without an input box, a finished task's or a moved call's, has
/// only this hint to say how to get out and where to stop it.
pub(super) const NO_INPUT_TAB_HINT: &str = " esc back · ctrl+x tasks ";

struct ViewLayout {
    msg_area: Rect,
    bottom_area: Rect,
    status_area: Rect,
    queue_area: Rect,
    panel_windows: Vec<(usize, Rect)>,
    input_area: Rect,
    splits: SplitLayout,
    bottom_takeover: bool,
}

impl App {
    fn prompt_open(&self) -> bool {
        self.pack_review.is_open() || self.permission_prompt.is_open()
    }

    pub(super) fn form_visible(&self) -> bool {
        self.prompt_open() || self.plan_form_active()
    }

    /// The caret of {owner}, the holder of the keyboard: the input box's
    /// {input}, the permission prompt's {prompt}, the focused window's own, or
    /// the {picker} one for a picker. None for an owner with no text field.
    fn keyboard_caret(
        &self,
        owner: KeyboardOwner,
        input: Option<Position>,
        prompt: Option<Position>,
        picker: Option<Position>,
    ) -> Option<Position> {
        match owner {
            KeyboardOwner::Chat => input,
            KeyboardOwner::Permission => prompt,
            KeyboardOwner::FocusedWindow => self.float_mgr.focused_caret(),
            KeyboardOwner::Search
            | KeyboardOwner::FilePicker
            | KeyboardOwner::RewindPicker
            | KeyboardOwner::ThemePicker
            | KeyboardOwner::ModelPicker
            | KeyboardOwner::LoginPicker
            | KeyboardOwner::McpPicker => picker,
            KeyboardOwner::Alert
            | KeyboardOwner::PackReview
            | KeyboardOwner::PlanForm
            | KeyboardOwner::Help
            | KeyboardOwner::Usage
            | KeyboardOwner::Btw
            | KeyboardOwner::Queue => None,
        }
    }

    /// Whether a plugin writing to the chat input would land in a box the
    /// user can see: nothing covering it, no form in the bottom panel, and a
    /// {area} tall enough to leave the box a text row. An overlay counts
    /// even when it only takes the keyboard, because the user is reading it
    /// and not the draft.
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

    /// Returns the cell the terminal cursor belongs on, so an IME draws its
    /// preedit there: the input box's caret, or while an overlay owns the
    /// keyboard, the caret of the field its keys reach. None when that field
    /// wraps, is clipped, or the overlay has no field.
    ///
    /// Where the caret was drawn is a separate question, and a caret-anchored
    /// float is placed from that: the caret outlives an overlay taking focus,
    /// or a window on it would jump to the middle of the screen the moment it
    /// started reading keys.
    pub fn view(&mut self, frame: &mut Frame) -> Option<Position> {
        let layout = self.compute_layout(frame.area());
        let render_chat = self.active_chat;
        let owner = self.keyboard_owner();

        let ((overlay_rect, caret, prompt_caret, picker_caret), thumbs) = Thumbs::record(|| {
            self.render_background(frame);
            self.render_messages(frame, &layout, render_chat);
            let (caret, prompt_caret) = self.render_bottom_panel(frame, &layout);
            self.render_splits(frame, &layout);
            let (overlay_rect, picker_caret) = self.render_picker_overlays(frame, &layout, owner);
            self.render_status_bar(frame, layout.status_area, render_chat);
            let overlay_rect = self.render_top_modals(frame, overlay_rect, caret);
            (overlay_rect, caret, prompt_caret, picker_caret)
        });
        self.register_zones(&layout, overlay_rect);
        self.apply_selection(frame, render_chat, &thumbs);
        self.keyboard_caret(owner, caret, prompt_caret, picker_caret)
    }

    /// Whether the terminal draws the caret. Only the chat input stopped
    /// painting its own; every other field still paints one.
    pub(crate) fn shows_terminal_cursor(&self) -> bool {
        self.keyboard_owner() == KeyboardOwner::Chat
    }

    /// The shape a plugin set with `maki.ui.set_cursor_shape`, None if no
    /// plugin ever did.
    pub(crate) fn cursor_style(&self) -> Option<CursorStyle> {
        self.keymap_reader.cursor_style()
    }

    fn compute_layout(&self, area: Rect) -> ViewLayout {
        let prompt_open = self.prompt_open();

        // Carve the full-width status bar first so the split carving below only
        // ever deals with the content region above it.
        let [content, status_area] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

        // A confirmation prompt owns the bottom area, so drop any `below` split
        // here at the source instead of fixing it up further down.
        let reqs: Vec<_> = self
            .float_mgr
            .split_reqs(content)
            .into_iter()
            .filter(|r| !(prompt_open && r.split == Split::Below))
            .collect();
        let splits = carve(content, &reqs);
        let inner = splits.inner;

        let below_active = splits.rect(Split::Below).is_some();
        let bottom_takeover = self.form_visible() || below_active;
        let max_bottom = inner.height.saturating_sub(MIN_CHAT_ROWS);
        let bottom_height = if self.permission_prompt.is_open() {
            self.permission_prompt.height(inner.width).min(max_bottom)
        } else if self.pack_review.is_open() {
            self.pack_review.height(inner.width).min(max_bottom)
        } else if below_active {
            0
        } else if self.form_visible() {
            self.plan_form.height(max_bottom).min(max_bottom)
        } else if self.chat_accepts_input() {
            let panel_h: u16 = self.float_mgr.panel_reqs().iter().map(|(_, h)| *h).sum();
            queue_panel::height(self.active_queue_len())
                + panel_h
                + self.input_box.height(inner.width).min(max_bottom)
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

        let queue_height = if bottom_takeover {
            0
        } else {
            queue_panel::height(self.active_queue_len())
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

    /// Returns the cell the input box drew its caret in and the cell the
    /// permission prompt drew its deny field's in, when they drew one at all.
    /// A prompt, a form, a `below` split or a chat that takes no messages all
    /// take the box off screen, and a box scrolled off its own viewport draws
    /// without a caret. A running subagent's chat keeps the box, so a message
    /// can be queued for that subagent while watching it.
    fn render_bottom_panel(
        &mut self,
        frame: &mut Frame,
        layout: &ViewLayout,
    ) -> (Option<Position>, Option<Position>) {
        if self.permission_prompt.is_open() {
            let prompt_caret = self.permission_prompt.view(frame, layout.bottom_area);
            return (None, prompt_caret);
        } else if self.pack_review.is_open() {
            self.pack_review.view(frame, layout.bottom_area);
        } else if !self.chat_accepts_input() {
            let panel_reqs = self.float_mgr.panel_reqs();
            let panel_h: u16 = panel_reqs.iter().map(|(_, h)| *h).sum();
            let sep_area = if panel_h > 0 {
                let [panels, sep] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
                    .areas(layout.bottom_area);
                let constraints: Vec<_> = panel_reqs
                    .iter()
                    .map(|&(_, h)| Constraint::Length(h))
                    .collect();
                let areas = Layout::vertical(constraints).split(panels);
                for (i, &(idx, _)) in panel_reqs.iter().enumerate() {
                    self.float_mgr.view_panel(frame, idx, areas[i]);
                }
                sep
            } else {
                layout.bottom_area
            };
            let hint = Line::from(Span::styled(NO_INPUT_TAB_HINT, theme::current().tool_dim));
            let sep = Block::default()
                .borders(Borders::TOP)
                .border_style(self.separator_style())
                .title_top(hint.right_aligned());
            frame.render_widget(sep, sep_area);
        } else if self.plan_form_active() {
            self.plan_form.view(frame, layout.bottom_area);
        } else if layout.bottom_area.height > 0 {
            let queue_entries = self.active_queue_entries();
            let focus = self.queue.focus().filter(|_| self.is_main_chat());
            queue_panel::view(frame, layout.queue_area, &queue_entries, focus);
            for &(idx, rect) in &layout.panel_windows {
                self.float_mgr.view_panel(frame, idx, rect);
            }
            let placeholder = self.input_placeholder();
            let panel_hint = (self.state.mode == Mode::Plan)
                .then(|| self.plan_form.hint_line())
                .flatten()
                .or_else(|| self.lua_hint_line());
            let caret = self.input_box.view(
                frame,
                layout.input_area,
                placeholder,
                self.separator_style(),
                panel_hint,
            );
            self.command_palette
                .view(frame, layout.input_area, &self.input_box.buffer);
            return (caret, None);
        }
        (None, None)
    }

    fn render_splits(&mut self, frame: &mut Frame, layout: &ViewLayout) {
        for dir in Split::ALL {
            if let Some(rect) = layout.splits.rect(dir) {
                self.float_mgr.view_split(frame, dir, rect);
            }
        }
    }

    /// A later picker is drawn over an earlier one. {overlay_rect} is the
    /// topmost picker's, and the caret is the one of the picker that is
    /// {owner}, None when {owner} is not a picker.
    fn render_picker_overlays(
        &mut self,
        frame: &mut Frame,
        layout: &ViewLayout,
        owner: KeyboardOwner,
    ) -> (Rect, Option<Position>) {
        let mut overlay_rect = Rect::default();
        let mut picker_caret = None;
        let full = frame.area();
        let mut record = |picker: KeyboardOwner, (rect, caret): (Rect, Option<Position>)| {
            overlay_rect = rect;
            if picker == owner {
                picker_caret = caret;
            }
        };

        if self.search_modal.is_open() {
            record(
                KeyboardOwner::Search,
                self.search_modal.view(frame, layout.msg_area),
            );
        }
        if self.file_picker.is_open() {
            record(
                KeyboardOwner::FilePicker,
                self.file_picker.view(frame, full),
            );
        }
        if self.rewind_picker.is_open() {
            record(
                KeyboardOwner::RewindPicker,
                self.rewind_picker.view(frame, full),
            );
        }
        if self.theme_picker.is_open() {
            record(
                KeyboardOwner::ThemePicker,
                self.theme_picker.view(frame, full),
            );
        }
        if self.model_picker.is_open() {
            record(
                KeyboardOwner::ModelPicker,
                self.model_picker.view(frame, full),
            );
        }
        if self.login_picker.is_open() {
            record(
                KeyboardOwner::LoginPicker,
                self.login_picker.view(frame, full),
            );
        }
        if self.mcp_picker.is_open() {
            record(KeyboardOwner::McpPicker, self.mcp_picker.view(frame, full));
        }

        (overlay_rect, picker_caret)
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
        let r = self.help_modal.view(frame, full, &self.keymap_reader);
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
        // Only the session's model is at hand here, and a subagent may run on
        // another, so a subagent's options are shown as stored.
        let opts = chat.opts.unwrap_or(RequestOptions {
            thinking: self.state.thinking.shown(&self.state.model),
            fast: self.state.fast,
        });
        let (mode_label, mode_style) = self.mode_label();
        let ctx = StatusBarContext {
            status: &self.status,
            motion: self.status_motion(),
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
            thinking_label: opts.thinking.status_label(),
            fast: opts.fast,
            workflow: self.state.workflow,
            restricted: self.trust.question().is_some(),
            yolo: self.permissions.is_yolo(),
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

        for &(_, rect) in &layout.panel_windows {
            self.zones.push_overlay(selection::inset_border(rect));
        }

        if layout.queue_area.height > 0 && !layout.bottom_takeover {
            self.zones.push_overlay(layout.queue_area);
        }

        for dir in Split::ALL {
            if let Some(rect) = layout.splits.rect(dir) {
                self.zones.push_overlay(selection::inset_border(rect));
            }
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

    fn apply_selection(&mut self, frame: &mut Frame, render_chat: usize, thumbs: &Thumbs) {
        let Some(ref state) = self.selection_state else {
            return;
        };

        let sel = state.sel();
        if let Some(screen_sel) = self.screen_selection(sel, render_chat) {
            selection::apply_highlight(
                frame.buffer_mut(),
                sel.highlight_area(),
                &screen_sel,
                thumbs,
            );
        }
        if state.is_pending_copy() {
            let sel = *sel;
            self.copy_selection(frame.buffer_mut(), &sel, render_chat, thumbs);
        }
    }

    /// Layout geometry for tests: `(msg_area, bottom_area, status_area,
    /// input_area, splits)`.
    #[cfg(test)]
    pub(super) fn layout_geometry(&self, area: Rect) -> (Rect, Rect, Rect, Rect, SplitLayout) {
        let layout = self.compute_layout(area);
        (
            layout.msg_area,
            layout.bottom_area,
            layout.status_area,
            layout.input_area,
            layout.splits,
        )
    }

    fn lua_hint_line(&self) -> Option<Line<'static>> {
        let spans: Vec<_> = self
            .hints
            .get()?
            .visible_in(self.state.session.id)
            .flat_map(|entry| &entry.spans)
            .map(|(text, style_name)| Span::styled(text.clone(), theme::style_by_name(style_name)))
            .collect();
        (!spans.is_empty()).then(|| Line::from(spans))
    }
}
