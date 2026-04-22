mod render;
mod segment;
mod selection;
#[cfg(test)]
mod tests;

use self::render::RenderCursor;
use self::segment::{Segment, SegmentCache, wrapped_line_count};
use self::selection::parse_batch_inner_id;

use super::render_hints::RenderHintsRegistry;
use super::tool_display::{
    ToolLines, append_annotation, append_right_info, assistant_style, build_batch_entry_lines,
    build_instructions_lines, build_tool_lines, done_style, error_style, format_timestamp_now,
    output_limits_from_hints, thinking_style, tool_output_annotation, truncate_to_header,
    user_style,
};
use super::{
    DisplayMessage, DisplayRole, ToolRole, ToolStatus, apply_scroll_delta, code_view::SectionFlags,
};
use crate::animation::spinner_str;
use crate::components::keybindings::key;
use crate::markdown::{hr_line, plain_lines, text_to_lines, truncate_output};
use crate::render_worker::RenderWorker;
use crate::selection::Selection;
use crate::splash::{ColorTransition, Splash};
use crate::theme;
use maki_config::{ToolOutputLines, UiConfig};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use super::scrollbar::render_vertical_scrollbar;
use super::streaming_content::StreamingContent;
use maki_agent::tools::native_static_name;
use maki_agent::{
    BatchToolEntry, BatchToolStatus, BufferSnapshot, InstructionBlock, NO_FILES_FOUND,
    RawRenderHints, ToolDoneEvent, ToolOutput, ToolStartEvent,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

pub struct MessagesPanel {
    messages: Vec<DisplayMessage>,
    streaming_thinking: StreamingContent,
    streaming_text: StreamingContent,
    started_at: Instant,
    scroll_top: u16,
    auto_scroll: bool,
    viewport_height: u16,
    viewport_width: u16,
    cache: SegmentCache,
    last_total_lines: u16,
    hl_worker: RenderWorker,
    theme_generation: u64,
    highlight_segment: Option<usize>,
    idle_splash: Splash,
    accent: ColorTransition,
    expanded_tools: HashMap<String, SectionFlags>,
    tool_output_lines: ToolOutputLines,
    render_hints: RenderHintsRegistry,
}

impl MessagesPanel {
    pub fn new(ui_config: UiConfig) -> Self {
        let thinking = thinking_style();
        let assistant = assistant_style();
        let ms = ui_config.typewriter_ms_per_char;
        Self {
            messages: Vec::new(),
            streaming_thinking: StreamingContent::new(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
                ms,
            ),
            streaming_text: StreamingContent::new(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
                ms,
            ),
            started_at: Instant::now(),
            scroll_top: u16::MAX,
            auto_scroll: true,
            viewport_height: 24,
            viewport_width: 80,
            cache: SegmentCache::new(),
            last_total_lines: 0,
            hl_worker: RenderWorker::new(),
            theme_generation: theme::generation(),
            highlight_segment: None,
            idle_splash: Splash::new(ui_config.splash_animation),
            accent: ColorTransition::new(theme::current().mode_build),
            expanded_tools: HashMap::new(),
            tool_output_lines: ui_config.tool_output_lines,
            render_hints: RenderHintsRegistry::new(),
        }
    }

    pub fn push(&mut self, msg: DisplayMessage) {
        self.messages.push(msg);
    }

    pub fn load_messages(&mut self, msgs: Vec<DisplayMessage>) {
        self.messages = msgs;
        self.cache.clear();
        self.expanded_tools.clear();
        self.highlight_segment = None;
    }

    pub fn register_plugin_hints(&mut self, tools: Vec<(Arc<str>, RawRenderHints)>) {
        for (name, raw) in tools {
            self.render_hints.register(name, &raw);
        }
    }

    pub fn thinking_delta(&mut self, text: &str) {
        self.streaming_thinking.push(text);
    }

    pub fn text_delta(&mut self, text: &str) {
        self.flush_thinking();
        self.streaming_text.push(text);
    }

    pub fn tool_pending(&mut self, id: String, name: &str) {
        let Some(name) = native_static_name(name) else {
            return;
        };
        self.flush();
        let role = DisplayRole::Tool(Box::new(ToolRole {
            id,
            status: ToolStatus::InProgress,
            name: Arc::from(name),
        }));
        let mut msg = DisplayMessage::new(role, String::new());
        msg.timestamp = Some(format_timestamp_now());
        self.messages.push(msg);
    }

    pub fn tool_start(&mut self, event: ToolStartEvent) {
        if let Some(msg) = self.find_tool_msg_mut(&event.id) {
            if let DisplayRole::Tool(t) = &mut msg.role {
                t.name = Arc::clone(&event.tool);
            }
            msg.text = event.summary;
            msg.tool_input = event.input.map(Arc::new);
            msg.tool_output = event.output.map(Arc::new);
            msg.annotation = event.annotation;
            self.rebuild_tool_segment(&event.id);
            return;
        }
        self.flush();
        let mut msg = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: event.id,
                status: ToolStatus::InProgress,
                name: Arc::clone(&event.tool),
            })),
            event.summary,
        );
        msg.tool_input = event.input.map(Arc::new);
        msg.tool_output = event.output.map(Arc::new);
        msg.annotation = event.annotation;
        msg.timestamp = Some(format_timestamp_now());
        self.messages.push(msg);
    }

    pub fn tool_output(&mut self, tool_id: &str, content: &str) {
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let tool_name = msg.role.tool_name().unwrap_or("");
        let hints = self.render_hints.get(tool_name);
        let limits = output_limits_from_hints(tool_name, hints, &self.tool_output_lines);
        truncate_to_header(&mut msg.text);
        let truncated = truncate_output(content, limits.max_lines, limits.keep);
        msg.truncated_lines = truncated.skipped;
        msg.text.push('\n');
        msg.text.push_str(&truncated.kept);
        msg.live_output = Some(content.to_owned());
        self.rebuild_tool_segment(tool_id);
    }

    pub fn tool_done(&mut self, event: ToolDoneEvent) {
        let Some(msg) = self
            .messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == event.id))
        else {
            return;
        };
        if let DisplayRole::Tool(t) = &mut msg.role {
            t.status = if event.is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Success
            };
        }
        truncate_to_header(&mut msg.text);
        let hints = self.render_hints.get(&event.tool);
        let done_annotation = tool_output_annotation(&event.output, hints.always_annotate);
        if let Some(suffix) = &done_annotation {
            append_annotation(&mut msg.annotation, suffix);
        }

        match &event.output {
            ToolOutput::Plain(text) | ToolOutput::ReadDir { text, .. }
                if !hints.skip_done_truncation && msg.render_snapshot.is_none() =>
            {
                let limits = output_limits_from_hints(&event.tool, hints, &self.tool_output_lines);
                let tr = truncate_output(text, limits.max_lines, limits.keep);
                msg.truncated_lines = tr.skipped;
                if !tr.kept.is_empty() {
                    msg.text = format!("{}\n{}", msg.text, tr.kept);
                }
            }
            ToolOutput::QuestionAnswers(pairs) => {
                let n = pairs.len();
                msg.text = format!("{n} question{} answered", if n == 1 { "" } else { "s" });
            }
            output @ ToolOutput::GlobResult { .. } => {
                if output.is_empty_result() {
                    msg.text = format!("{}\n{NO_FILES_FOUND}", msg.text);
                } else {
                    let display = output.as_display_text();
                    let limits =
                        output_limits_from_hints(&event.tool, hints, &self.tool_output_lines);
                    let tr = truncate_output(&display, limits.max_lines, limits.keep);
                    msg.truncated_lines = tr.skipped;
                    msg.text = format!("{}\n{}", msg.text, tr.kept);
                }
            }
            ToolOutput::GrepResult { entries } if entries.is_empty() => {
                msg.text = format!("{}\n{NO_FILES_FOUND}", msg.text);
            }
            ToolOutput::Batch { entries, .. } => {
                let failed = entries
                    .iter()
                    .filter(|e| e.status == BatchToolStatus::Error)
                    .count();
                if failed > 0 {
                    let total = entries.len();
                    msg.text = format!("{}/{total} tools succeeded", total - failed);
                }
            }
            _ => {}
        }
        if let ToolOutput::Batch {
            entries: new_entries,
            text,
        } = &event.output
            && let Some(arc) = &mut msg.tool_output
            && let ToolOutput::Batch {
                entries: existing,
                text: existing_text,
            } = Arc::make_mut(arc)
        {
            for (existing, new) in existing.iter_mut().zip(new_entries) {
                existing.status = new.status;
                existing.output = new.output.clone();
            }
            *existing_text = text.clone();
        } else {
            msg.tool_output = Some(Arc::new(event.output));
        }
        msg.live_output = None;
        self.rebuild_tool_segment(&event.id);
    }

    pub fn batch_progress(
        &mut self,
        batch_id: &str,
        index: usize,
        status: BatchToolStatus,
        output: Option<ToolOutput>,
        summary: Option<&str>,
    ) {
        let Some(msg) = self.find_tool_msg_mut(batch_id) else {
            return;
        };
        if let Some(arc) = &mut msg.tool_output
            && let ToolOutput::Batch { entries, .. } = Arc::make_mut(arc)
            && let Some(entry) = entries.get_mut(index)
        {
            entry.status = status;
            if output.is_some() {
                entry.output = output;
            }
            if let Some(s) = summary {
                entry.summary = s.to_owned();
            }
        }
        self.rebuild_tool_segment(batch_id);
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.update_tool(
            tool_id,
            |msg| msg.text = summary.to_owned(),
            |entry| entry.summary = summary.to_owned(),
        );
    }

    pub fn update_tool_model(&mut self, tool_id: &str, model: &str) {
        self.update_tool(
            tool_id,
            |msg| append_annotation(&mut msg.annotation, model),
            |entry| append_annotation(&mut entry.annotation, model),
        );
    }

    pub fn tool_snapshot(&mut self, tool_id: &str, snapshot: BufferSnapshot) {
        if let Some(msg) = self.find_tool_msg_mut(tool_id) {
            msg.render_snapshot = Some(snapshot);
            self.rebuild_tool_segment(tool_id);
        }
    }

    pub fn tool_annotation(&mut self, tool_id: &str, annotation: &str) {
        if let Some(msg) = self.find_tool_msg_mut(tool_id) {
            append_annotation(&mut msg.annotation, annotation);
            self.rebuild_tool_segment(tool_id);
        }
    }

    pub fn set_turn_usage_on_last_tool(&mut self, usage: String) {
        let Some(idx) = self
            .messages
            .iter()
            .rposition(|m| matches!(m.role, DisplayRole::Tool(_)))
        else {
            return;
        };
        self.messages[idx].turn_usage = Some(usage);
        let DisplayRole::Tool(t) = &self.messages[idx].role else {
            unreachable!()
        };
        let id = t.id.clone();
        self.rebuild_tool_segment(&id);
    }

    fn upsert_instruction_segment(
        &mut self,
        parent_id: &str,
        blocks: &[InstructionBlock],
        parent_idx: usize,
        msg_index: Option<usize>,
    ) {
        if blocks.is_empty() {
            return;
        }
        let inst_id = segment::instruction_id(parent_id);
        let batch_index = parse_batch_inner_id(parent_id).map(|(_, idx)| idx + 1);
        let exp = self
            .expanded_tools
            .get(&inst_id)
            .copied()
            .unwrap_or_default();
        let tl = build_instructions_lines(blocks, self.viewport_width, exp.output, batch_index);

        if let Some(seg_idx) = self.cache.find_by_tool_id(&inst_id) {
            let seg = self.cache.get_mut(seg_idx).unwrap();
            seg.search_text = tl.search_text.clone();
            seg.update_with_reuse(tl, &self.hl_worker);
        } else {
            let mut seg = Segment::with_tool(inst_id, msg_index);
            seg.search_text = tl.search_text.clone();
            seg.apply_highlight(tl, &self.hl_worker);
            if batch_index.is_some() {
                self.cache.insert(parent_idx + 1, seg);
            } else {
                self.cache.insert(parent_idx + 1, Segment::spacer());
                self.cache.insert(parent_idx + 2, seg);
            }
        }
    }

    fn update_tool(
        &mut self,
        tool_id: &str,
        update_msg: impl FnOnce(&mut DisplayMessage),
        update_entry: impl FnOnce(&mut BatchToolEntry),
    ) {
        let rebuild_id;
        if let Some((batch_id, idx)) = parse_batch_inner_id(tool_id) {
            let Some(msg) = self.find_tool_msg_mut(batch_id) else {
                return;
            };
            if let Some(arc) = &mut msg.tool_output
                && let ToolOutput::Batch { entries, .. } = Arc::make_mut(arc)
                && let Some(entry) = entries.get_mut(idx)
            {
                update_entry(entry);
            }
            rebuild_id = batch_id.to_owned();
        } else {
            let Some(msg) = self.find_tool_msg_mut(tool_id) else {
                return;
            };
            update_msg(msg);
            rebuild_id = tool_id.to_owned();
        }
        self.rebuild_tool_segment(&rebuild_id);
    }

    pub fn stream_reset(&mut self) {
        self.streaming_thinking.clear();
        self.streaming_text.clear();
        self.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: String) {
        let ids: Vec<(String, Arc<str>)> = self
            .messages
            .iter()
            .filter_map(|m| {
                if let DisplayRole::Tool(t) = &m.role
                    && t.status == ToolStatus::InProgress
                {
                    Some((t.id.clone(), Arc::clone(&t.name)))
                } else {
                    None
                }
            })
            .collect();
        for (id, tool) in ids {
            self.tool_done(ToolDoneEvent {
                id,
                tool,
                output: ToolOutput::Plain(message.clone()),
                is_error: true,
            });
        }
    }

    pub fn cancel_in_progress(&mut self) {
        let affected_ids: Vec<String> = self
            .messages
            .iter_mut()
            .filter_map(|msg| {
                if let DisplayRole::Tool(t) = &mut msg.role
                    && t.status == ToolStatus::InProgress
                {
                    t.status = ToolStatus::Error;
                    if let Some(arc) = &mut msg.tool_output
                        && let ToolOutput::Batch { entries, .. } = Arc::make_mut(arc)
                    {
                        for entry in entries.iter_mut() {
                            if entry.status == BatchToolStatus::InProgress
                                || entry.status == BatchToolStatus::Pending
                            {
                                entry.status = BatchToolStatus::Error;
                            }
                        }
                    }
                    Some(t.id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in &affected_ids {
            self.rebuild_tool_segment(id);
        }
    }

    pub fn in_progress_count(&self) -> usize {
        self.messages
            .iter()
            .filter(
                |m| matches!(&m.role, DisplayRole::Tool(t) if t.status == ToolStatus::InProgress),
            )
            .count()
    }

    #[cfg(test)]
    pub fn toggle_expansion(&mut self, tool_id: &str) -> bool {
        let Some(seg) = self
            .cache
            .segments()
            .iter()
            .find(|s| s.tool_id.as_deref() == Some(tool_id))
        else {
            return false;
        };
        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let tool_id = tool_id.to_owned();
        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        entry.script = !entry.script;
        entry.output = !entry.output;
        self.rebuild_expanded_tool(&tool_id);
        true
    }

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    #[cfg(test)]
    pub fn last_message_text(&self) -> &str {
        self.messages.last().map(|m| m.text.as_str()).unwrap_or("")
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages.last().is_some_and(|m| m.plan_path.is_some())
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages.last().map(|m| &m.role)
    }

    pub fn flush(&mut self) {
        self.flush_thinking();
        if !self.streaming_text.is_empty() {
            self.messages.push(DisplayMessage::new(
                DisplayRole::Assistant,
                self.streaming_text.take_all(),
            ));
        }
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll_top = apply_scroll_delta(self.scroll_top, delta).min(self.max_scroll());
        self.auto_scroll = false;
    }

    pub fn auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_top = 0;
        self.auto_scroll = false;
    }

    pub fn enable_auto_scroll(&mut self) {
        self.auto_scroll = true;
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        let width = self.viewport_width;
        let offset = self
            .cache
            .segments()
            .iter()
            .take(segment_index)
            .map(|s| s.height(width) as u32)
            .sum::<u32>()
            .min(u16::MAX as u32) as u16;
        self.scroll_top = offset.min(self.max_scroll());
        self.auto_scroll = false;
    }

    pub fn restore_scroll(&mut self, scroll_top: u16, auto_scroll: bool) {
        self.scroll_top = scroll_top;
        self.auto_scroll = auto_scroll;
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.highlight_segment = idx;
    }

    pub fn half_page(&self) -> i32 {
        self.viewport_height as i32 / 2
    }

    pub fn set_accent(&mut self, color: ratatui::style::Color) {
        self.accent.set(color);
    }

    pub fn toggle_expansion_at(&mut self, row: u16, area: Rect) -> bool {
        if area.height == 0 {
            return false;
        }
        let doc_row = (row.saturating_sub(area.y)) as u32 + self.scroll_top as u32;
        let width = self.viewport_width;
        let Some((_, seg, seg_start)) = self.cache.segment_at_row(doc_row, width) else {
            return false;
        };
        let Some(tool_id) = seg.tool_id.as_deref() else {
            return false;
        };
        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        if !seg.truncation.any() && !exp.any() {
            return false;
        }
        let click_line = (doc_row - seg_start) as usize;
        let tool_id = tool_id.to_owned();
        let truncation = seg.truncation;
        let separator_line = seg.separator_line;

        let entry = self.expanded_tools.entry(tool_id.clone()).or_default();
        match separator_line {
            Some(sep) if click_line < sep => {
                if truncation.script || entry.script {
                    entry.script = !entry.script;
                }
            }
            _ => {
                if truncation.output || entry.output {
                    entry.output = !entry.output;
                }
            }
        }
        self.rebuild_expanded_tool(&tool_id);
        true
    }

    fn rebuild_expanded_tool(&mut self, tool_id: &str) {
        if segment::is_instruction_segment(tool_id) {
            if let Some(parent_id) = segment::instruction_parent(tool_id)
                && let Some(parent_idx) = self.cache.find_by_tool_id(parent_id)
                && let Some(blocks) = self.get_instructions_for_tool(parent_id)
            {
                self.upsert_instruction_segment(parent_id, &blocks, parent_idx, None);
            }
        } else {
            let rebuild_id =
                parse_batch_inner_id(tool_id).map_or(tool_id, |(batch_id, _)| batch_id);
            self.rebuild_tool_segment(rebuild_id);
        }
    }

    fn get_instructions_for_tool(&self, tool_id: &str) -> Option<Vec<InstructionBlock>> {
        let output = if let Some((batch_id, idx)) = parse_batch_inner_id(tool_id) {
            let msg = self
                .messages
                .iter()
                .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == batch_id))?;
            match msg.tool_output.as_deref()? {
                ToolOutput::Batch { entries, .. } => entries.get(idx)?.output.as_ref()?,
                _ => return None,
            }
        } else {
            let msg = self
                .messages
                .iter()
                .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))?;
            msg.tool_output.as_deref()?
        };
        output.owned_instructions()
    }

    pub fn is_animating(&self) -> bool {
        self.in_progress_count() > 0
            || self.streaming_thinking.is_animating()
            || self.streaming_text.is_animating()
            || self.show_idle_splash()
            || self.accent.is_animating()
    }

    fn show_idle_splash(&self) -> bool {
        self.messages.is_empty()
            && self.streaming_thinking.is_empty()
            && self.streaming_text.is_empty()
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect, has_selection: bool) {
        self.viewport_height = area.height;

        if self.show_idle_splash() {
            let accent = self.accent.resolve();
            self.idle_splash.render(area, frame.buffer_mut(), accent);
            return;
        }

        let width = area.width.saturating_sub(1);
        let theme_gen = theme::generation();
        if self.viewport_width != width || self.theme_generation != theme_gen {
            self.viewport_width = width;
            self.theme_generation = theme_gen;
            self.cache.invalidate_from_msg_count();
            let thinking = thinking_style();
            let assistant = assistant_style();
            self.streaming_thinking.set_style(
                thinking.prefix,
                thinking.text_style,
                thinking.prefix_style,
            );
            self.streaming_text.set_style(
                assistant.prefix,
                assistant.text_style,
                assistant.prefix_style,
            );
        }
        self.drain_highlights();
        self.rebuild_line_cache();
        if self.in_progress_count() > 0 {
            self.update_spinners();
        }

        let cached_count = self.cache.len();
        let spacer_lines: [Line<'static>; 1] = [Line::default()];
        let mut streaming_heights: Vec<u16> = Vec::new();
        for sc in [&mut self.streaming_thinking, &mut self.streaming_text] {
            if sc.is_empty() {
                continue;
            }
            let lines = sc.render_lines(width);
            if cached_count > 0 || !streaming_heights.is_empty() {
                streaming_heights.push(1);
            }
            streaming_heights.push(wrapped_line_count(lines, width));
        }

        let cached_height = self.cache.total_height(width);
        let streaming_sum: u32 = streaming_heights.iter().map(|&h| h as u32).sum();
        let total_lines: u16 = (cached_height + streaming_sum).min(u16::MAX as u32) as u16;
        self.last_total_lines = total_lines;
        let max_scroll = total_lines.saturating_sub(self.viewport_height);
        self.scroll_top = self.scroll_top.min(max_scroll);
        if !has_selection {
            if self.scroll_top >= max_scroll {
                self.auto_scroll = true;
            }
            if self.auto_scroll {
                self.scroll_top = max_scroll;
            }
        }

        let viewport = Rect::new(area.x, area.y, width, area.height);
        let mut cursor = RenderCursor::new(self.scroll_top, viewport);

        for (i, seg) in self.cache.segments().iter().enumerate() {
            if cursor.past_bottom() {
                break;
            }
            let h = seg.height(width);
            let highlight = self.highlight_segment == Some(i);
            let style = seg.tool_id.as_ref().map(|_| theme::current().tool_bg);
            cursor.render(seg.lines(), h, style, highlight, frame);
        }

        let mut height_idx = 0usize;
        for sc in [&self.streaming_thinking, &self.streaming_text] {
            if sc.is_empty() || height_idx >= streaming_heights.len() || cursor.past_bottom() {
                continue;
            }
            if cached_count > 0 || height_idx > 0 {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                cursor.render(&spacer_lines, h, None, false, frame);
            }
            if height_idx < streaming_heights.len() {
                let h = streaming_heights[height_idx];
                height_idx += 1;
                cursor.render(sc.cached_lines(), h, None, false, frame);
            }
        }

        if total_lines > area.height {
            render_vertical_scrollbar(frame, area, total_lines, self.scroll_top);
        }
    }

    fn max_scroll(&self) -> u16 {
        self.last_total_lines.saturating_sub(self.viewport_height)
    }

    pub fn scroll_top(&self) -> u16 {
        self.scroll_top
    }

    pub fn segment_heights(&self) -> Vec<u16> {
        let width = self.viewport_width;
        self.cache
            .segments()
            .iter()
            .map(|s| s.height(width))
            .collect()
    }

    pub fn segment_search_texts(&self) -> Vec<&str> {
        self.cache.search_texts()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        selection::extract_selection_text(&self.cache, self.viewport_width, sel, msg_area)
    }

    fn find_tool_msg_mut(&mut self, tool_id: &str) -> Option<&mut DisplayMessage> {
        self.messages
            .iter_mut()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
    }

    fn build_tool_segment_lines(
        msg: &DisplayMessage,
        status: ToolStatus,
        started_at: Instant,
        width: u16,
        exp: SectionFlags,
        tool_output_lines: &ToolOutputLines,
        registry: &RenderHintsRegistry,
    ) -> ToolLines {
        let mut tl = build_tool_lines(
            msg,
            status,
            started_at,
            width,
            exp,
            tool_output_lines,
            registry,
        );
        if let Some(ts) = &msg.timestamp
            && !tl.lines.is_empty()
        {
            append_right_info(&mut tl.lines[0], msg.turn_usage.as_deref(), Some(ts), width);
        }
        tl
    }

    fn flush_thinking(&mut self) {
        if !self.streaming_thinking.is_empty() {
            self.messages.push(DisplayMessage::new(
                DisplayRole::Thinking,
                self.streaming_thinking.take_all(),
            ));
        }
    }

    fn update_spinners(&mut self) {
        let spinner_span = Span::styled(
            spinner_str(self.started_at.elapsed().as_millis()),
            theme::current().spinner,
        );
        for seg in self.cache.segments_mut() {
            let is_child = seg
                .tool_id
                .as_deref()
                .is_some_and(segment::is_child_segment);
            for &line_idx in &seg.spinner_lines.clone() {
                let span_idx = if line_idx == 0 && !is_child { 0 } else { 1 };
                seg.update_spinner(line_idx, span_idx, spinner_span.clone());
            }
        }
    }

    fn drain_highlights(&mut self) {
        while let Some(result) = self.hl_worker.try_recv() {
            if let Some(seg) = self
                .cache
                .segments_mut()
                .iter_mut()
                .find(|s| s.matches_pending_highlight(result.id))
            {
                seg.apply_highlight_result(result.lines);
            }
        }
    }

    fn rebuild_tool_segment(&mut self, tool_id: &str) {
        let Some(msg) = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let DisplayRole::Tool(t) = &msg.role else {
            unreachable!()
        };
        let status = t.status;
        let Some(seg_idx) = self.cache.find_by_tool_id(tool_id) else {
            return;
        };

        let exp = self
            .expanded_tools
            .get(tool_id)
            .copied()
            .unwrap_or_default();
        let tl = Self::build_tool_segment_lines(
            msg,
            status,
            self.started_at,
            self.viewport_width,
            exp,
            &self.tool_output_lines,
            &self.render_hints,
        );

        let instructions = msg
            .tool_output
            .as_deref()
            .and_then(|o| o.owned_instructions());

        let seg = self.cache.get_mut(seg_idx).unwrap();
        seg.search_text = tl.search_text.clone();
        seg.update_with_reuse(tl, &self.hl_worker);

        self.build_and_upsert_batch_children(seg_idx, tool_id);

        if let Some(blocks) = instructions {
            self.upsert_instruction_segment(tool_id, &blocks, seg_idx, None);
        }
    }

    fn build_and_upsert_batch_children(&mut self, parent_idx: usize, tool_id: &str) {
        let Some(msg) = self
            .messages
            .iter()
            .rfind(|m| matches!(&m.role, DisplayRole::Tool(t) if t.id == tool_id))
        else {
            return;
        };
        let Some(ToolOutput::Batch { entries, .. }) = msg.tool_output.as_deref() else {
            return;
        };
        let children: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(j, entry)| {
                let child_id = format!("{tool_id}__{j}");
                let child_exp = self
                    .expanded_tools
                    .get(&child_id)
                    .copied()
                    .unwrap_or_default();
                let tl = build_batch_entry_lines(
                    entry,
                    j,
                    self.started_at,
                    self.viewport_width,
                    child_exp,
                    &self.tool_output_lines,
                    &self.render_hints,
                );
                let search = tl.search_text.clone();
                let instructions = entry.output.as_ref().and_then(|o| o.owned_instructions());
                (child_id, search, tl, instructions)
            })
            .collect();
        let child_prefix = format!("{tool_id}__");
        let msg_index = self.cache.get(parent_idx).and_then(|s| s.msg_index);
        for (child_id, search, tl, instructions) in children {
            let child_seg_idx = if let Some(cseg_idx) = self.cache.find_by_tool_id(&child_id) {
                let cseg = self.cache.get_mut(cseg_idx).unwrap();
                cseg.search_text = search;
                cseg.update_with_reuse(tl, &self.hl_worker);
                cseg_idx
            } else {
                let mut seg = Segment::with_tool(child_id.clone(), msg_index);
                seg.search_text = search;
                seg.apply_highlight(tl, &self.hl_worker);
                let insert_pos = self
                    .cache
                    .segments()
                    .iter()
                    .rposition(|s| {
                        s.tool_id
                            .as_deref()
                            .is_some_and(|id| id == tool_id || id.starts_with(&child_prefix))
                    })
                    .map_or(parent_idx + 1, |p| p + 1);
                self.cache.insert(insert_pos, seg);
                insert_pos
            };
            if let Some(blocks) = instructions {
                self.upsert_instruction_segment(&child_id, &blocks, child_seg_idx, msg_index);
            }
        }
    }

    fn rebuild_line_cache(&mut self) {
        if !self.cache.needs_rebuild(self.messages.len()) {
            return;
        }
        for i in self.cache.msg_count()..self.messages.len() {
            let msg = &self.messages[i];

            if let DisplayRole::Tool(t) = &msg.role {
                let exp = self.expanded_tools.get(&t.id).copied().unwrap_or_default();
                let status = t.status;
                let tl = Self::build_tool_segment_lines(
                    msg,
                    status,
                    self.started_at,
                    self.viewport_width,
                    exp,
                    &self.tool_output_lines,
                    &self.render_hints,
                );
                let id = t.id.clone();
                let search_text = tl.search_text.clone();
                self.cache.push_spacer_if_needed();
                let mut seg = Segment::with_tool(id.clone(), Some(i));
                seg.search_text = search_text;
                seg.apply_highlight(tl, &self.hl_worker);
                self.cache.push(seg);

                if let Some(ToolOutput::Batch { entries, .. }) = msg.tool_output.as_deref() {
                    let inst_data: Vec<_> = entries
                        .iter()
                        .enumerate()
                        .map(|(j, entry)| {
                            let child_id = format!("{id}__{j}");
                            let child_exp = self
                                .expanded_tools
                                .get(&child_id)
                                .copied()
                                .unwrap_or_default();
                            let tl = build_batch_entry_lines(
                                entry,
                                j,
                                self.started_at,
                                self.viewport_width,
                                child_exp,
                                &self.tool_output_lines,
                                &self.render_hints,
                            );
                            let blocks = entry.output.as_ref().and_then(|o| o.owned_instructions());
                            (child_id, tl, blocks)
                        })
                        .collect();
                    for (child_id, tl, blocks) in inst_data {
                        let mut seg = Segment::with_tool(child_id.clone(), Some(i));
                        seg.search_text = tl.search_text.clone();
                        seg.apply_highlight(tl, &self.hl_worker);
                        self.cache.push(seg);
                        if let Some(blocks) = blocks {
                            let last_idx = self.cache.len().saturating_sub(1);
                            self.upsert_instruction_segment(&child_id, &blocks, last_idx, Some(i));
                        }
                    }
                } else {
                    let blocks = msg
                        .tool_output
                        .as_deref()
                        .and_then(|o| o.owned_instructions());
                    if let Some(blocks) = blocks {
                        let last_idx = self.cache.len().saturating_sub(1);
                        self.upsert_instruction_segment(&id, &blocks, last_idx, Some(i));
                    }
                }
            } else {
                let style = match &msg.role {
                    DisplayRole::User => user_style(),
                    DisplayRole::Assistant => assistant_style(),
                    DisplayRole::Thinking => thinking_style(),
                    DisplayRole::Error => error_style(),
                    DisplayRole::Done => done_style(),
                    DisplayRole::Tool(_) => unreachable!(),
                };
                let prefix = if msg.plan_path.is_some() {
                    ""
                } else {
                    style.prefix
                };
                let mut lines = if style.use_markdown {
                    text_to_lines(
                        &msg.text,
                        prefix,
                        style.text_style,
                        style.prefix_style,
                        None,
                        self.viewport_width,
                    )
                } else {
                    plain_lines(&msg.text, prefix, style.text_style, style.prefix_style)
                };
                if let Some(pp) = &msg.plan_path {
                    if !msg.text.is_empty() {
                        let rule = hr_line(self.viewport_width, theme::current().plan_rule);
                        lines.insert(0, rule.clone());
                        lines.push(rule);
                    } else {
                        lines.clear();
                    }
                    if !msg.text.is_empty() {
                        lines.push(Line::from(""));
                    }
                    lines.push(Line::from(Span::styled(
                        pp.to_owned(),
                        theme::current().plan_path,
                    )));
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{} to open in editor ($VISUAL / $EDITOR)",
                            key::OPEN_EDITOR.label
                        ),
                        theme::current().tool_dim,
                    )));
                }

                let search_text = format!("{prefix}{}", msg.text);
                self.cache.push_spacer_if_needed();
                self.cache
                    .push(Segment::with_lines(lines, search_text, Some(i)));
            }
        }
        self.cache.mark_built(self.messages.len());
    }
}
