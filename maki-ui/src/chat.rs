//! Rebuilds display messages from stored sessions. Tool outputs get syntax
//! highlighted, missing outputs fall back to plain text from `ToolResult`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::app::tasks::{TaskClock, TaskOutcome, TaskParent, TaskStatus};
use crate::components::input::Draft;
use crate::components::messages::{ClickOutcome, MessagesPanel, PromptProgress, ScrollPos};
use crate::components::tool_display::append_annotation;
use crate::components::{
    BackgroundBadge, BackgroundOutcome, DisplayMessage, DisplayRole, ToolRole, ToolStatus,
};
use crate::markdown::truncate_output;

use crate::selection::{DocPos, RowPos, Selection};
use maki_agent::tools::{MAIN_TASK_ID, ToolInvocation, ToolRegistry, WRITE_TOOL_NAME};
use maki_agent::types::ALLOWED_BY;
use maki_agent::{
    AgentEvent, BufferSnapshot, DisplayItem, SharedBuf, SteerKind, SubagentInbox, TextOutput,
    ToolDoneEvent, ToolOutput, ToolStartEvent, display_order, observation_detail,
};
use maki_config::{ToolKey, ToolOutputLines, UiConfig};
use maki_lua::{ChatItem, ChatItemStatus, LuaCommandInfo, WinView};

use maki_providers::{ContentBlock, ImageSource, Message, RequestOptions, Role};
use maki_storage::id::MakiId;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::repaint::{Cadence, Dirty};

pub(crate) const DONE_TEXT: &str = "Done!";
pub(crate) const ERROR_TEXT: &str = "Error";
pub(crate) const CANCELLED_TEXT: &str = "Cancelled";
pub(crate) const LOST_TEXT: &str = "Lost";
/// One notice per streak: a wedged model can spend twenty nudges, and twenty
/// identical bubbles bury the conversation they are about.
const NUDGE_TEXT: &str = "Model stalled after tool calls, nudging...";
const REWRITTEN_PREFIX: &str = "A plugin rewrote this message. The model got:";
const DROPPED_PREFIX: &str = "A plugin kept this message from the model:";
const CONTINUED_PREFIX: &str = "A plugin kept the agent going:";
pub(crate) const INBOX_DROPPED_SUFFIX: &str =
    " queued message(s) dropped: the subagent finished first";

pub enum ChatEventResult {
    Continue,
    Done,
    QueueItemConsumed {
        text: String,
        images: Vec<ImageSource>,
    },
    Error(String),
    PermissionRequest {
        id: String,
        tool: ToolKey,
        scopes: Vec<String>,
        reason: Option<String>,
    },
    AuthRequired,
}

/// A call this chat's agent started that has not reported done.
pub(crate) struct RunningCall {
    pub(crate) id: String,
    pub(crate) tool: Arc<str>,
}

pub struct Chat {
    pub name: String,
    /// The plugin that opened this task with `maki.agent.session`. A tool task
    /// has none: it lives for one turn of the main chat and is swept with it,
    /// while a plugin task answers only to its plugin and to Esc in its tab.
    pub(crate) plugin: Option<Arc<str>>,
    /// A plugin task between runs: its run ended, its session did not. Kept
    /// by [`Self::track_run`] from the runs' own events.
    pub(crate) idle: bool,
    /// Its run waits on expired credentials for the user's Enter. Any later
    /// event of the run, or the chat's end, lifts it, so it never goes stale.
    pub(crate) auth_parked: bool,
    /// Shows a call moved off its turn. The call is out until its final
    /// result, whatever its runs do in between, so it never reads idle.
    pub(crate) backs_moved_call: bool,
    /// The moved call `/tasks` lists this task under.
    pub(crate) parent: Option<TaskParent>,
    pub(crate) clock: Option<TaskClock>,
    pub cost: Option<f64>,
    /// This chat's slice of the session's list-price reference cost; mirrors
    /// `cost`, but only ever set for subsidised models.
    pub list_cost: Option<f64>,
    pub context_size: u32,
    pub model_id: Option<String>,
    /// A subagent's own settings; `None` on the main chat, which reads the
    /// session's.
    pub opts: Option<RequestOptions>,
    /// A running subagent's inbox for messages typed in its chat. Taken when
    /// the chat finishes, since nothing would drain it after that.
    pub(crate) inbox: Option<Arc<SubagentInbox>>,
    /// Where a subagent's permission answers go. It lives until the chat
    /// finishes, not the turn, so a subagent moved off its turn can still be
    /// answered.
    pub(crate) answer_tx: Option<flume::Sender<String>>,
    /// Parked while another chat is in front, see `App::set_active_chat`.
    pub(crate) draft: Draft,
    pending_turn_usage: Option<String>,
    messages_panel: MessagesPanel,
    /// The ending and the index of the bubble announcing it, so a later, better
    /// informed outcome can fix that bubble instead of appending a second one.
    finish: Option<(TaskOutcome, usize)>,
    /// Candidates only: a row can stop running without a `ToolDone` (a cancel,
    /// a retry), so [`Self::running_calls`] checks each against its row.
    running_calls: Vec<RunningCall>,
}

impl Chat {
    /// A chat belongs to one session for life: every path that changes
    /// `App::state.session` rebuilds the chats after the swap. `task_id` is
    /// `None` for the main chat, the subagent's `tool_use_id` otherwise.
    pub fn new(
        session_id: MakiId,
        task_id: Option<&str>,
        name: String,
        ui_config: UiConfig,
        lua_event_handle: maki_lua::EventHandle,
    ) -> Self {
        let mut messages_panel = MessagesPanel::new(ui_config, lua_event_handle);
        messages_panel.set_chat(session_id, task_id.map(Arc::from));
        Self {
            name,
            plugin: None,
            idle: false,
            auth_parked: false,
            backs_moved_call: false,
            parent: None,
            clock: None,
            cost: None,
            list_cost: None,
            context_size: 0,
            model_id: None,
            opts: None,
            inbox: None,
            answer_tx: None,
            draft: Draft::default(),
            pending_turn_usage: None,
            messages_panel,
            finish: None,
            running_calls: Vec::new(),
        }
    }

    /// The handle `maki.task` addresses a task by, see `app::tasks`.
    pub(crate) fn task_id(&self) -> Option<&Arc<str>> {
        self.messages_panel.task_id()
    }

    pub(crate) fn request_restores(&self, items: Vec<maki_lua::RestoreItem>) {
        self.messages_panel.request_restores(items);
    }

    pub(crate) fn task_id_or_main(&self) -> Arc<str> {
        self.task_id()
            .map_or_else(|| Arc::from(MAIN_TASK_ID), Arc::clone)
    }

    pub(crate) fn task_status(&self) -> TaskStatus {
        match self.finish {
            Some((outcome, _)) => outcome.into(),
            None if self.idle && !self.backs_moved_call => TaskStatus::Idle,
            None => TaskStatus::Working,
        }
    }

    /// A plugin task that still answers to its plugin. Its session lives in
    /// the Lua host, so neither a rewind nor a reload of the chrome ends it.
    pub(crate) fn is_live_plugin_task_of(&self, session: MakiId) -> bool {
        self.plugin.is_some()
            && !self.is_finished()
            && self.messages_panel.session_id() == Some(session)
    }

    /// A run is working from any event of its own to its `Done` or `Error`.
    /// A run woken by a moved call's result opens without a prompt, so no
    /// single event marks the start. What trails a run, its transcript, its
    /// close and frozen renders, says nothing either way. Only a plugin task
    /// goes idle, but any run's own event means it no longer waits on auth.
    pub(crate) fn track_run(&mut self, event: &AgentEvent) {
        let idle = match event {
            AgentEvent::Done { .. } | AgentEvent::Error { .. } => true,
            AgentEvent::SubagentHistory { .. }
            | AgentEvent::StreamClosed
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. } => return,
            _ => false,
        };
        self.auth_parked = false;
        if self.plugin.is_some() {
            self.idle = idle;
        }
    }

    pub fn set_pending_turn_usage(&mut self, usage: String) {
        self.pending_turn_usage = Some(usage);
    }

    pub(crate) fn set_restore_channel(&mut self, event_tx: Option<maki_agent::EventSender>) {
        self.messages_panel.set_restore_channel(event_tx);
    }

    pub fn handle_event(&mut self, event: AgentEvent, plan_path: Option<&Path>) -> ChatEventResult {
        match event {
            AgentEvent::ThinkingDelta { text } => {
                self.messages_panel.clear_prompt_progress();
                self.messages_panel.thinking_delta(&text);
            }
            AgentEvent::TextDelta { text } => {
                self.messages_panel.clear_prompt_progress();
                self.messages_panel.text_delta(&text);
            }
            AgentEvent::ToolPending { id, name } => self.messages_panel.tool_pending(id, &name),
            AgentEvent::ToolStart(e) => {
                if !self.running_calls.iter().any(|call| call.id == e.id) {
                    self.running_calls.push(RunningCall {
                        id: e.id.clone(),
                        tool: Arc::clone(&e.tool),
                    });
                }
                self.messages_panel.tool_start(*e);
            }
            AgentEvent::ToolOutput { id, content } => {
                self.messages_panel.tool_output(&id, &content)
            }
            AgentEvent::ToolDone(e) => {
                let plan_write = plan_path.filter(|pp| e.wrote_to(pp));
                let is_full_write = &*e.tool == WRITE_TOOL_NAME;
                self.running_calls.retain(|call| call.id != e.id);
                self.messages_panel.tool_done(*e);
                if let Some(pp) = plan_write {
                    let content = if is_full_write {
                        std::fs::read_to_string(pp).unwrap_or_default()
                    } else {
                        String::new()
                    };
                    self.messages_panel
                        .push(DisplayMessage::plan(content, pp.display().to_string()));
                }
            }
            AgentEvent::TurnComplete(turn) => {
                for block in turn.message.content {
                    if let ContentBlock::Image { source } = block {
                        self.messages_panel.flush();
                        self.messages_panel.push(DisplayMessage::with_images(
                            DisplayRole::Assistant,
                            String::new(),
                            vec![source],
                        ));
                    }
                }
            }
            AgentEvent::ToolResultsSubmitted { .. } => {
                if let Some(usage) = self.pending_turn_usage.take() {
                    self.messages_panel.set_turn_usage_on_last_tool(usage);
                }
            }
            AgentEvent::AutoCompacting { .. } => {
                self.messages_panel.flush();
                self.messages_panel.push(DisplayMessage::new(
                    DisplayRole::Assistant,
                    "Auto-compacting conversation...".into(),
                ));
            }
            AgentEvent::CompactionDone { .. } => {
                self.messages_panel.flush();
            }
            AgentEvent::QueueItemConsumed { text, images } => {
                return ChatEventResult::QueueItemConsumed { text, images };
            }
            AgentEvent::QueueDrained => {}
            AgentEvent::ToolMovable { id, movable } => {
                self.messages_panel.set_tool_movable(&id, movable);
            }
            // Parts belong to `/tasks`, which the App keeps, never to a transcript.
            AgentEvent::ToolParts { .. } => {}
            AgentEvent::Retry { .. } => unreachable!("handled before handle_event"),
            AgentEvent::Done { .. } => {
                self.messages_panel.flush();
                return ChatEventResult::Done;
            }
            AgentEvent::Error { message, .. } => {
                self.messages_panel.flush();
                return ChatEventResult::Error(message);
            }
            AgentEvent::PermissionRequest {
                id,
                tool,
                scopes,
                reason,
            } => {
                return ChatEventResult::PermissionRequest {
                    id,
                    tool,
                    scopes,
                    reason,
                };
            }
            AgentEvent::Steered { kind, text } => {
                let prefix = match kind {
                    SteerKind::MessageRewritten => REWRITTEN_PREFIX,
                    SteerKind::MessageDropped => DROPPED_PREFIX,
                    SteerKind::Continued => CONTINUED_PREFIX,
                };
                self.messages_panel.flush();
                self.messages_panel.push(DisplayMessage::new(
                    DisplayRole::Assistant,
                    format!("{prefix}\n\n{text}"),
                ));
            }
            AgentEvent::AllowedByPlugin { id, plugin } => self
                .messages_panel
                .annotate_tool(&id, &format!("{ALLOWED_BY} {plugin}")),
            AgentEvent::AuthRequired => {
                return ChatEventResult::AuthRequired;
            }
            AgentEvent::ToolSnapshot {
                id,
                snapshot,
                theme_gen,
            } => {
                self.messages_panel.tool_snapshot(&id, snapshot, theme_gen);
            }
            AgentEvent::ToolHeaderSnapshot {
                id,
                snapshot,
                theme_gen,
            } => {
                self.messages_panel
                    .tool_header_snapshot(&id, snapshot, theme_gen);
            }
            AgentEvent::Nudge => {
                self.messages_panel.flush();
                if self.messages_panel.last_message_text() != NUDGE_TEXT {
                    self.messages_panel.push(DisplayMessage::new(
                        DisplayRole::Assistant,
                        NUDGE_TEXT.into(),
                    ));
                }
            }
            AgentEvent::Notice { text } => {
                self.messages_panel.flush();
                self.messages_panel.push(notice(text, None));
            }
            AgentEvent::ObservationLine { text, detail, end } => {
                self.messages_panel.flush();
                self.messages_panel.push(task_notice(
                    text,
                    detail,
                    end.map(BackgroundOutcome::from),
                ));
            }
            AgentEvent::SubagentHistory { .. } | AgentEvent::StreamClosed => {}
            AgentEvent::LiveToolBuf { id, body } => {
                self.messages_panel.register_live_buf(id, body);
            }
            AgentEvent::PromptProgress {
                processed,
                total,
                cache,
            } => {
                self.messages_panel
                    .set_prompt_progress((processed < total).then_some(PromptProgress {
                        processed,
                        total,
                        cache,
                    }));
            }
        }
        ChatEventResult::Continue
    }

    pub fn scroll(&mut self, delta: i32) {
        self.messages_panel.scroll(delta);
    }

    pub fn scroll_to_row(&mut self, doc_row: u32) {
        self.messages_panel.scroll_to_row(doc_row);
    }

    pub fn half_page(&self) -> i32 {
        self.messages_panel.half_page()
    }

    pub fn page(&self) -> i32 {
        self.messages_panel.page()
    }

    pub fn win_view(&self) -> WinView {
        self.messages_panel.win_view()
    }

    pub fn auto_scroll(&self) -> bool {
        self.messages_panel.auto_scroll()
    }

    pub fn scroll_to_top(&mut self) {
        self.messages_panel.scroll_to_top();
    }

    pub fn enable_auto_scroll(&mut self) {
        self.messages_panel.enable_auto_scroll();
    }

    pub fn scroll_to_segment(&mut self, segment_index: usize) {
        self.messages_panel.scroll_to_segment(segment_index);
    }

    pub fn restore_scroll(&mut self, scroll: ScrollPos, auto_scroll: bool) {
        self.messages_panel.restore_scroll(scroll, auto_scroll);
    }

    pub fn set_highlight_segment(&mut self, idx: Option<usize>) {
        self.messages_panel.set_highlight_segment(idx);
    }

    pub fn set_accent(&mut self, color: Color) {
        self.messages_panel.set_accent(color);
    }

    pub fn tick(&mut self) -> Dirty {
        self.messages_panel.tick()
    }

    pub fn cadence(&self) -> Cadence {
        self.messages_panel.cadence()
    }

    pub fn view(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        has_selection: bool,
        images_visible: bool,
        lua_commands: &[LuaCommandInfo],
    ) {
        self.messages_panel
            .view(frame, area, has_selection, images_visible, lua_commands);
    }

    pub fn scroll_pos(&self) -> ScrollPos {
        self.messages_panel.scroll_pos()
    }

    pub fn doc_pos_at(&self, rel_row: u16, col: u16) -> DocPos {
        self.messages_panel.doc_pos_at(rel_row, col)
    }

    pub fn project_row(&self, pos: DocPos) -> RowPos {
        self.messages_panel.project_row(pos)
    }

    pub fn segment_search_texts(&self) -> Vec<String> {
        self.messages_panel.segment_search_texts()
    }

    pub fn extract_selection_text(&self, sel: &Selection, msg_area: Rect) -> String {
        self.messages_panel.extract_selection_text(sel, msg_area)
    }

    pub fn handle_click(&mut self, row: u16, col: u16, area: Rect) -> ClickOutcome {
        self.messages_panel.handle_click(row, col, area)
    }

    pub fn toggle_click(&mut self, row: u16, area: Rect) -> ClickOutcome {
        self.messages_panel.toggle_click(row, area)
    }

    pub fn tool_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.messages_panel
            .tool_snapshot(tool_id, snapshot, theme_gen);
    }

    pub fn tool_header_snapshot(
        &mut self,
        tool_id: &str,
        snapshot: BufferSnapshot,
        theme_gen: Option<u64>,
    ) {
        self.messages_panel
            .tool_header_snapshot(tool_id, snapshot, theme_gen);
    }

    pub fn stream_reset(&mut self) {
        self.messages_panel.stream_reset();
    }

    pub fn flush(&mut self) {
        self.messages_panel.flush();
    }

    pub fn cancel_in_progress(&mut self) {
        self.messages_panel.cancel_in_progress();
    }

    pub fn fail_in_progress_with_message(&mut self, message: String) {
        self.messages_panel.fail_in_progress_with_message(message);
    }

    pub fn fail_in_progress_except(&mut self, message: String, excluded: &HashSet<String>) {
        self.messages_panel
            .fail_in_progress_except(message, excluded);
    }

    pub fn push(&mut self, msg: DisplayMessage) {
        self.messages_panel.push(msg);
    }

    /// Ends the task before its work says so, after a stop or a loss. Rows
    /// still in progress would never hear back, so they are cancelled here.
    pub(crate) fn end_early(&mut self, outcome: TaskOutcome, text: &str) {
        self.flush();
        self.cancel_in_progress();
        self.mark_finished(outcome, text);
    }

    /// Like [`Self::mark_finished`], but the latest word wins. A part's tab
    /// only mirrors reports, and a report that lands after its call ended
    /// knows better than the call did.
    pub(crate) fn end_as(&mut self, outcome: TaskOutcome, text: &str) {
        match self.finish {
            Some((previous, _)) if previous == outcome => {}
            Some((_, bubble)) => {
                self.finish = Some((outcome, bubble));
                self.messages_panel
                    .replace(bubble, DisplayMessage::new(outcome.role(), text.into()));
            }
            None => self.mark_finished(outcome, text),
        }
    }

    pub(crate) fn finish_outcome(&self) -> Option<TaskOutcome> {
        self.finish.map(|(outcome, _)| outcome)
    }

    /// The call this tab is a part of, see [`TaskParent::Part`].
    pub(crate) fn part_of(&self) -> Option<&Arc<str>> {
        match &self.parent {
            Some(TaskParent::Part(call)) => Some(call),
            _ => None,
        }
    }

    /// Ends the transcript with the bubble [`TaskOutcome::role`] picks. A chat
    /// only ever grows one ending, but a caller who knows more than the one who
    /// got here first rewrites it in place. See [`TaskOutcome::refines`].
    pub(crate) fn mark_finished(&mut self, outcome: TaskOutcome, text: &str) {
        if let Some((previous, bubble)) = self.finish {
            if outcome.refines(previous) {
                self.finish = Some((outcome, bubble));
                self.messages_panel
                    .replace(bubble, DisplayMessage::new(outcome.role(), text.into()));
            }
            return;
        }
        self.messages_panel.flush();
        self.answer_tx = None;
        self.auth_parked = false;
        if let Some(clock) = &mut self.clock {
            clock.stop();
        }
        let undelivered = self.inbox.take().map_or(0, |inbox| inbox.len());
        if undelivered > 0 {
            self.messages_panel.push(DisplayMessage::new(
                DisplayRole::Error,
                format!("{undelivered}{INBOX_DROPPED_SUFFIX}"),
            ));
        }
        let bubble = self
            .messages_panel
            .push(DisplayMessage::new(outcome.role(), text.into()));
        self.finish = Some((outcome, bubble));
    }

    /// Takes back an ending the turn wrote for a task that outlived it. Its
    /// bubble stays as the record of that Esc. The next event naming the task
    /// brings back the senders the ending dropped.
    pub(crate) fn reopen(&mut self) {
        self.finish = None;
        if let Some(clock) = &mut self.clock {
            clock.resume();
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finish.is_some()
    }

    pub fn update_tool_summary(&mut self, tool_id: &str, summary: &str) {
        self.messages_panel.update_tool_summary(tool_id, summary);
    }

    pub fn update_tool_model(&mut self, tool_id: &str, model: &str) {
        self.messages_panel.annotate_tool(tool_id, model);
    }

    /// A plugin-owned transcript item: `running` creates or re-titles a
    /// spinner item, `done` / `failed` closes it. Missing items close quietly:
    /// a plugin may update a chat item after the transcript was rebuilt.
    pub(crate) fn apply_chat_item(&mut self, item: &ChatItem) {
        match item.status {
            ChatItemStatus::Running => {
                self.messages_panel.tool_start(ToolStartEvent {
                    id: item.id.to_string(),
                    tool: Arc::clone(&item.label),
                    summary: item.title.clone(),
                    render_header: None,
                    annotation: None,
                    input: None,
                    raw_input: None,
                    output: None,
                });
            }
            ChatItemStatus::Done | ChatItemStatus::Failed => {
                if let Some(detail) = &item.detail {
                    self.messages_panel.update_tool_summary(&item.id, detail);
                }
                self.messages_panel.tool_done(ToolDoneEvent {
                    id: item.id.to_string(),
                    tool: Arc::clone(&item.label),
                    output: Arc::new(ToolOutput::Plain(TextOutput::from(String::new()))),
                    is_error: item.status == ChatItemStatus::Failed,
                    annotation: None,
                    written_path: None,
                    call: None,
                });
            }
        }
    }

    pub fn set_tool_turn_usage(&mut self, tool_id: &str, usage: String) {
        self.messages_panel.set_tool_turn_usage(tool_id, usage);
    }

    /// The calls still running now, oldest first.
    pub(crate) fn running_calls(&mut self) -> &[RunningCall] {
        let panel = &self.messages_panel;
        self.running_calls.retain(|call| {
            panel.tool_message(&call.id).is_some_and(|msg| {
                matches!(&msg.role, DisplayRole::Tool(t) if t.status == ToolStatus::InProgress)
            })
        });
        &self.running_calls
    }

    pub(crate) fn tool_message(&self, tool_id: &str) -> Option<DisplayMessage> {
        self.messages_panel.tool_message(tool_id)
    }

    pub(crate) fn live_buf(&self, tool_id: &str) -> Option<Arc<SharedBuf>> {
        self.messages_panel.live_buf(tool_id)
    }

    pub(crate) fn register_live_buf(&mut self, tool_id: String, buf: Arc<SharedBuf>) {
        self.messages_panel.register_live_buf(tool_id, buf);
    }

    pub(crate) fn settle_tool(&mut self, tool_id: &str, status: ToolStatus) {
        self.messages_panel.settle_tool(tool_id, status);
    }

    pub(crate) fn set_tool_background(&mut self, tool_id: &str, badge: BackgroundBadge) {
        self.messages_panel.set_tool_background(tool_id, badge);
    }

    pub(crate) fn set_background_outcome(&mut self, tool_id: &str, outcome: BackgroundOutcome) {
        self.messages_panel.set_background_outcome(tool_id, outcome);
    }

    pub(crate) fn has_background_badge(&self, tool_id: &str) -> bool {
        self.messages_panel.has_background_badge(tool_id)
    }

    pub(crate) fn background_badges(&self) -> Vec<BackgroundBadge> {
        self.messages_panel.background_badges()
    }

    pub fn load_messages(&mut self, msgs: Vec<DisplayMessage>) {
        self.running_calls.clear();
        self.messages_panel.load_messages(msgs);
    }

    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.messages_panel
            .push(DisplayMessage::new(DisplayRole::User, text.into()));
    }

    /// Flush, push, and re-pin scroll in one shot to avoid
    /// the one-frame hop where the bubble briefly lands in the wrong row.
    pub fn show_user_message(&mut self, text: impl Into<String>, images: Vec<ImageSource>) {
        self.flush();
        self.messages_panel.push(DisplayMessage::with_images(
            DisplayRole::User,
            text.into(),
            images,
        ));
        self.enable_auto_scroll();
    }

    pub fn shell_tool_start(&mut self, event: ToolStartEvent) {
        self.messages_panel.tool_start(event);
    }

    pub fn shell_tool_output(&mut self, id: &str, content: &str) {
        self.messages_panel.tool_output(id, content);
    }

    pub fn shell_tool_done(&mut self, event: ToolDoneEvent) {
        self.messages_panel.tool_done(event);
    }

    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.messages_panel.message_count()
    }

    #[cfg(test)]
    pub fn message_at(&self, index: usize) -> Option<&DisplayMessage> {
        self.messages_panel.message_at(index)
    }

    #[cfg(test)]
    pub fn in_progress_count(&self) -> usize {
        self.messages_panel.in_progress_count()
    }

    #[cfg(test)]
    pub fn last_message_text(&self) -> &str {
        self.messages_panel.last_message_text()
    }

    #[cfg(test)]
    pub fn last_message_is_plan(&self) -> bool {
        self.messages_panel.last_message_is_plan()
    }

    #[cfg(test)]
    pub fn last_message_role(&self) -> Option<&DisplayRole> {
        self.messages_panel.last_message_role()
    }

    #[cfg(test)]
    pub fn streaming_text_is_empty(&self) -> bool {
        self.messages_panel.streaming_text_is_empty()
    }

    #[cfg(test)]
    pub fn streaming_thinking_is_empty(&self) -> bool {
        self.messages_panel.streaming_thinking_is_empty()
    }

    #[cfg(test)]
    pub fn tool_turn_usage(&self, tool_id: &str) -> Option<&str> {
        self.messages_panel.tool_turn_usage(tool_id)
    }
}

pub fn history_to_display(
    messages: &[Message],
    tool_outputs: &HashMap<String, Arc<ToolOutput>>,
    tool_output_lines: &ToolOutputLines,
) -> (Vec<DisplayMessage>, Vec<maki_lua::RestoreItem>) {
    let results = build_tool_results_map(messages);
    let mut display = Vec::new();
    let mut restore_items: Vec<maki_lua::RestoreItem> = Vec::new();
    for item in display_order(messages) {
        let msg = match item {
            DisplayItem::Message(msg) => msg,
            DisplayItem::ObservationLine { index, text } => {
                let message = &messages[index];
                display.push(task_notice(
                    text.to_owned(),
                    observation_detail(message).map(str::to_owned),
                    message.settles.as_ref().map(|settled| settled.end.into()),
                ));
                continue;
            }
        };
        if msg.is_context_update() || msg.is_observation() {
            continue;
        }
        match msg.role {
            Role::User => {
                // An empty `display_text` marks a message the transcript never
                // shows, so its attachments must not sneak in either.
                if msg.display_text.as_deref() == Some("") {
                    continue;
                }
                let images = user_images(msg);
                let text = msg.user_text();
                if text.is_some() || !images.is_empty() {
                    display.push(DisplayMessage::with_images(
                        DisplayRole::User,
                        text.unwrap_or_default().to_owned(),
                        images,
                    ));
                }
            }
            Role::Assistant => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            display.push(DisplayMessage::new(DisplayRole::Assistant, text.clone()));
                        }
                        ContentBlock::Image { source } => {
                            display.push(DisplayMessage::with_images(
                                DisplayRole::Assistant,
                                String::new(),
                                vec![source.clone()],
                            ));
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                            display
                                .push(DisplayMessage::new(DisplayRole::Thinking, thinking.clone()));
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            let static_name = name.as_str();
                            let reg = ToolRegistry::global();
                            let tool_call: Option<Box<dyn ToolInvocation>> =
                                reg.get(name).and_then(|entry| entry.try_parse(input));
                            let summary = reg.resolve_header(name, input);
                            let (status, result_text) = results
                                .get(id.as_str())
                                .map(|(err, text)| {
                                    let s = if *err {
                                        ToolStatus::Error
                                    } else {
                                        ToolStatus::Success
                                    };
                                    (s, Some(&**text))
                                })
                                .unwrap_or((ToolStatus::Success, None));
                            let stored = tool_outputs.get(id.as_str());
                            let (text, truncated_lines, tool_output, mut annotation) =
                                build_loaded_tool(
                                    static_name,
                                    &summary,
                                    stored.cloned(),
                                    result_text,
                                    tool_output_lines,
                                );
                            if let Some(ta) =
                                tool_call.as_deref().and_then(|tc| tc.start_annotation())
                            {
                                append_annotation(&mut annotation, &ta);
                            }
                            let output = stored
                                .map(|o| o.as_text())
                                .or_else(|| result_text.map(str::to_owned))
                                .unwrap_or_default();
                            let state = stored.and_then(|o| o.state().cloned());
                            // Structured outputs (e.g. Diff) render natively
                            // in Rust with full fidelity; a Lua restore
                            // snapshot would replace that with a poorer text
                            // view.
                            let rust_rendered =
                                stored.is_some_and(|o| o.structured_display_text().is_some());
                            if !rust_rendered {
                                restore_items.push(maki_lua::RestoreItem {
                                    tool: Arc::from(static_name),
                                    tool_use_id: id.clone(),
                                    output,
                                    input: input.clone(),
                                    is_error: status == ToolStatus::Error,
                                    tool_output_lines: *tool_output_lines,
                                    theme_gen: None,
                                    clicks: Vec::new(),
                                    state,
                                    session_id: None,
                                    task_id: None,
                                    reason: maki_lua::RestoreReason::Load,
                                });
                            }
                            display.push(DisplayMessage {
                                role: DisplayRole::Tool(Box::new(ToolRole {
                                    id: id.clone(),
                                    status,
                                    name: static_name.into(),
                                })),
                                text,
                                images: Vec::new(),
                                tool_input: None,
                                tool_raw_input: Some(Arc::new(input.clone())),
                                tool_output,
                                live_output: None,
                                annotation,
                                plan_path: None,
                                timestamp: None,
                                turn_usage: None,
                                truncated_lines,
                                render_snapshot: None,
                                render_header: None,
                                snapshot_theme_gen: 0,
                                thinking_expanded: false,
                                movable: false,
                                background: None,
                                detail: None,
                                detail_expanded: false,
                                task_end: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    (display, restore_items)
}

fn notice(text: String, detail: Option<String>) -> DisplayMessage {
    task_notice(text, detail, None)
}

fn task_notice(
    text: String,
    detail: Option<String>,
    task_end: Option<BackgroundOutcome>,
) -> DisplayMessage {
    DisplayMessage {
        detail,
        task_end,
        ..DisplayMessage::new(DisplayRole::Notice, text)
    }
}

/// `ToolResult` is gone after session load, so we rebuild from
/// whatever the `DisplayMessage` kept. A rerender: the transcript was
/// already replayed once, this asks for one call's buf again.
pub(crate) fn restore_item_for(
    msg: &DisplayMessage,
    tool_output_lines: maki_config::ToolOutputLines,
    theme_gen: u64,
) -> Option<maki_lua::RestoreItem> {
    let DisplayRole::Tool(role) = &msg.role else {
        return None;
    };
    let input = msg.tool_raw_input.as_deref()?;
    let stored = msg.tool_output.as_deref()?;
    if stored.structured_display_text().is_some() {
        return None;
    }
    let output = stored.as_text();
    let state = stored.state().cloned();
    Some(maki_lua::RestoreItem {
        tool: role.name.clone(),
        tool_use_id: role.id.clone(),
        output,
        input: input.clone(),
        is_error: role.status == ToolStatus::Error,
        tool_output_lines,
        theme_gen: Some(theme_gen),
        clicks: Vec::new(),
        state,
        session_id: None,
        task_id: None,
        reason: maki_lua::RestoreReason::Rerender,
    })
}

/// Mirrors the live `tool_done` path so restored sessions
/// look the same as streamed ones.
fn build_loaded_tool(
    tool: &str,
    summary: &str,
    reconstructed: Option<Arc<ToolOutput>>,
    result_text: Option<&str>,
    tool_output_lines: &ToolOutputLines,
) -> (String, usize, Option<Arc<ToolOutput>>, Option<String>) {
    match reconstructed {
        Some(output) => {
            let annotation = output.annotation();
            (summary.to_owned(), 0, Some(output), annotation)
        }
        None => {
            let result = result_text.unwrap_or("");
            let annotation = if !result.is_empty() {
                ToolOutput::Plain(result.into()).annotation()
            } else {
                None
            };
            if result.is_empty() {
                (summary.to_owned(), 0, None, annotation)
            } else {
                let tr = truncate_output(result, tool_output_lines.get(tool));
                (
                    format!("{}\n{}", summary, tr.kept),
                    tr.skipped,
                    None,
                    annotation,
                )
            }
        }
    }
}

/// Images the user attached. A tool result also rides in a user message, and
/// its images already render under the tool itself, so those stay out.
fn user_images(msg: &Message) -> Vec<ImageSource> {
    if msg
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
    {
        return Vec::new();
    }
    msg.content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { source } => Some(source.clone()),
            _ => None,
        })
        .collect()
}

fn build_tool_results_map(messages: &[Message]) -> HashMap<&str, (bool, &str)> {
    let mut map = HashMap::new();
    for msg in messages {
        if !matches!(msg.role, Role::User) || msg.is_from_host() {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } = block
            {
                map.insert(tool_use_id.as_str(), (*is_error, content.as_str()));
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::IMAGE_PLACEHOLDER;
    use maki_agent::{
        AgentEvent, RunTotals, ToolDoneEvent, ToolOutput, ToolStartEvent, TurnCompleteEvent,
    };
    use maki_config::UiConfig;
    use maki_providers::{ImageMediaType, TaskEnd, TaskSettled};
    use test_case::test_case;

    const IMAGE_DATA: &str = "dGVzdA==";
    const EXPECTED_QUEUE_DELIVERY: &str = "a consumed queue item must carry its images";

    fn image() -> ImageSource {
        ImageSource::new(ImageMediaType::Png, Arc::from(IMAGE_DATA))
    }

    #[test_case(USER_TEXT, USER_TEXT ; "captioned")]
    #[test_case("", IMAGE_PLACEHOLDER ; "image_only")]
    fn history_delivers_images_without_synthetic_or_tool_attachments(text: &str, expected: &str) {
        let mut hidden = Message::synthetic(USER_TEXT.into());
        hidden.content.push(ContentBlock::Image { source: image() });
        let mut observation = Message::observation(USER_TEXT.into());
        observation
            .content
            .push(ContentBlock::Image { source: image() });
        let tool_result = Message {
            role: Role::User,
            content: vec![
                ContentBlock::tool_result(TASK_ID, USER_TEXT, false),
                ContentBlock::Image { source: image() },
            ],
            ..Default::default()
        };
        let assistant = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Image { source: image() }],
            ..Default::default()
        };
        let (display, _) = history_to_display(
            &[
                Message::user_with_images(text.into(), vec![image()]),
                hidden,
                observation,
                tool_result,
                assistant,
            ],
            &empty_outputs(),
            &ToolOutputLines::default(),
        );
        assert_eq!(display.len(), 2);
        assert_eq!(display[0].role, DisplayRole::User);
        assert_eq!(display[0].text, expected);
        assert_eq!(display[1].role, DisplayRole::Assistant);
        assert_eq!(display[1].text, IMAGE_PLACEHOLDER);
        for message in display {
            assert_eq!(message.images.len(), 1);
            assert_eq!(&*message.images[0].data, IMAGE_DATA);
        }
    }

    #[test]
    fn queued_user_images_reach_display() {
        let mut chat = chat();
        let result = chat.handle_event(
            AgentEvent::QueueItemConsumed {
                text: String::new(),
                images: vec![image()],
            },
            None,
        );
        let ChatEventResult::QueueItemConsumed { text, images } = result else {
            panic!("{EXPECTED_QUEUE_DELIVERY}");
        };
        chat.show_user_message(text, images);
        let message = chat.message_at(0).unwrap();
        assert_eq!(message.role, DisplayRole::User);
        assert_eq!(message.text, IMAGE_PLACEHOLDER);
        assert_eq!(message.images.len(), 1);
        assert_eq!(&*message.images[0].data, IMAGE_DATA);
    }

    #[test_case("" ; "image_only")]
    #[test_case(REPLY_TEXT ; "streamed_text")]
    fn turn_complete_delivers_images_without_replaying_text(text: &str) {
        let mut chat = chat();
        text_delta(&mut chat, text);
        chat.handle_event(
            AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
                message: Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text { text: text.into() },
                        ContentBlock::Image { source: image() },
                    ],
                    ..Default::default()
                },
                usage: Default::default(),
                model: String::new(),
                cost: None,
                subsidised_list_cost: None,
                context_size: None,
                context_window: 0,
            })),
            None,
        );
        let image_index = usize::from(!text.is_empty());
        assert_eq!(chat.message_count(), image_index + 1);
        if !text.is_empty() {
            assert_eq!(chat.message_at(0).unwrap().text, text);
            assert!(chat.message_at(0).unwrap().images.is_empty());
        }
        let message = chat.message_at(image_index).unwrap();
        assert_eq!(message.role, DisplayRole::Assistant);
        assert_eq!(message.text, IMAGE_PLACEHOLDER);
        assert_eq!(message.images.len(), 1);
        assert_eq!(&*message.images[0].data, IMAGE_DATA);
    }

    fn tool_start(id: &str, tool: &str) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: id.into(),
            tool: tool.into(),
            summary: String::new(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
        }))
    }

    fn tool_done(id: &str, tool: &str, output: ToolOutput) -> AgentEvent {
        tool_done_with_written_path(id, tool, output, None)
    }

    fn tool_done_with_written_path(
        id: &str,
        tool: &str,
        output: ToolOutput,
        written_path: Option<String>,
    ) -> AgentEvent {
        AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            call: None,
            id: id.into(),
            tool: tool.into(),
            output: Arc::new(output),
            is_error: false,
            annotation: None,
            written_path,
            backgrounded: false,
        }))
    }

    fn write_output(path: &str) -> (ToolOutput, Option<String>) {
        (
            ToolOutput::Plain(format!("wrote 42 bytes to {path}").into()),
            Some(path.to_owned()),
        )
    }

    fn edit_output(path: &str) -> ToolOutput {
        ToolOutput::Diff {
            path: path.into(),
            before: String::new(),
            after: String::new(),
            summary: String::new(),
        }
    }

    fn empty_outputs() -> HashMap<String, Arc<ToolOutput>> {
        HashMap::new()
    }

    const MAIN_NAME: &str = "Main";
    const SUBAGENT_NAME: &str = "research";
    const TASK_ID: &str = "toolu_01";
    const USER_TEXT: &str = "one more thing";
    const REPLY_TEXT: &str = "on it";
    const NOTICE_BODY: &str = "Background task find auth finished:\n\nok";
    const NOTICE_LINE: &str = "Background agent finished: find auth";
    const TASK_PLUGIN: &str = "monitor";

    fn chat() -> Chat {
        Chat::new(
            MakiId::generate(),
            None,
            MAIN_NAME.into(),
            UiConfig::default(),
            maki_lua::EventHandle::disconnected_for_test(),
        )
    }

    fn subagent_chat() -> Chat {
        Chat::new(
            MakiId::generate(),
            Some(TASK_ID),
            SUBAGENT_NAME.into(),
            UiConfig::default(),
            maki_lua::EventHandle::disconnected_for_test(),
        )
    }

    fn end(chat: &mut Chat, outcome: TaskOutcome) {
        chat.mark_finished(outcome, outcome.text());
    }

    fn text_delta(chat: &mut Chat, text: &str) {
        chat.handle_event(AgentEvent::TextDelta { text: text.into() }, None);
    }

    #[test]
    fn tool_lifecycle() {
        let mut chat = chat();
        chat.handle_event(tool_start("t1", "bash"), None);
        assert_eq!(chat.in_progress_count(), 1);

        chat.handle_event(
            tool_done("t1", "bash", ToolOutput::Plain("ok".into())),
            None,
        );
        assert_eq!(chat.in_progress_count(), 0);
    }

    /// The call never opened a prompt, so the row is the only place the user
    /// learns something let it through. A code_execution run marks its row
    /// once per inner call, and the row needs to say it only once.
    #[test]
    fn a_call_a_plugin_allowed_is_marked_on_its_row_once() {
        const PLUGIN: &str = "automode";
        let mut chat = chat();
        chat.handle_event(tool_start("t1", "code_execution"), None);
        for _ in 0..2 {
            chat.handle_event(
                AgentEvent::AllowedByPlugin {
                    id: "t1".into(),
                    plugin: PLUGIN.into(),
                },
                None,
            );
        }
        let row = chat.message_at(0).expect("the tool row");
        assert_eq!(
            row.annotation.as_deref(),
            Some(format!("{ALLOWED_BY} {PLUGIN}").as_str())
        );
    }

    #[test]
    fn plan_write_renders_file_content() {
        let mut chat = chat();
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.md");
        std::fs::write(&plan_path, "# My Plan\n\n- Step 1").unwrap();
        let plan_str = plan_path.to_str().unwrap();

        chat.handle_event(tool_start("w1", "write"), Some(plan_path.as_path()));
        let (output, wp) = write_output(plan_str);
        chat.handle_event(
            tool_done_with_written_path("w1", "write", output, wp),
            Some(plan_path.as_path()),
        );

        assert!(chat.last_message_is_plan());
        let last = chat.last_message_text();
        assert!(last.contains("# My Plan"));
    }

    #[test]
    fn plan_write_ignores_different_path() {
        let mut chat = chat();
        let plan_path = Path::new("/plans/123.md");
        chat.handle_event(tool_start("w1", "write"), Some(plan_path));
        let (output, wp) = write_output("src/main.rs");
        chat.handle_event(
            tool_done_with_written_path("w1", "write", output, wp),
            Some(plan_path),
        );
        assert!(!chat.last_message_is_plan());
    }

    #[test]
    fn plan_edit_shows_path_only() {
        let mut chat = chat();
        let dir = tempfile::tempdir().unwrap();
        let plan_path = dir.path().join("plan.md");
        std::fs::write(&plan_path, "# My Plan\n\n- Step 1").unwrap();
        let plan_str = plan_path.to_str().unwrap();

        chat.handle_event(tool_start("e1", "edit"), Some(plan_path.as_path()));
        chat.handle_event(
            tool_done("e1", "edit", edit_output(plan_str)),
            Some(plan_path.as_path()),
        );

        assert!(chat.last_message_is_plan());
        assert!(chat.last_message_text().is_empty());
    }

    #[test]
    fn history_skips_empty_text() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: String::new(),
            }],
            ..Default::default()
        }];
        assert!(
            history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default())
                .0
                .is_empty()
        );
    }

    #[test_case(Message::observation("build failed".into()) ; "observation")]
    #[test_case(Message::context_update("<context-update>Date is now tomorrow.</context-update>".into(), Default::default()) ; "context_update")]
    fn history_hides_host_messages_but_keeps_the_prompt(host: Message) {
        let msgs = vec![host, Message::user(USER_TEXT.into())];
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default()).0;
        let shown: Vec<(DisplayRole, &str)> = display
            .iter()
            .map(|d| (d.role.clone(), d.text.as_str()))
            .collect();
        assert_eq!(shown, [(DisplayRole::User, USER_TEXT)]);
    }

    /// The line is saved with the observation, so a restored session shows
    /// it as a notice row, and the body stays the model's alone. Where the
    /// row goes is `display_order`'s call.
    #[test_case(Some(NOTICE_LINE), &[(DisplayRole::User, USER_TEXT), (DisplayRole::Notice, NOTICE_LINE)] ; "labelled")]
    #[test_case(None, &[(DisplayRole::User, USER_TEXT)] ; "unlabelled")]
    fn a_restored_observation_shows_only_its_line(
        line: Option<&str>,
        expected: &[(DisplayRole, &str)],
    ) {
        let mut observation = Message::observation(NOTICE_BODY.into());
        observation.display_text = line.map(str::to_owned);
        let next = Message::user(USER_TEXT.into());
        let saved = serde_json::to_string(&[observation, next]).unwrap();
        let restored: Vec<Message> = serde_json::from_str(&saved).unwrap();

        let display =
            history_to_display(&restored, &empty_outputs(), &ToolOutputLines::default()).0;

        let shown: Vec<(DisplayRole, &str)> = display
            .iter()
            .map(|d| (d.role.clone(), d.text.as_str()))
            .collect();
        assert_eq!(shown, expected);
    }

    const NOTICE_OUTPUT: &str = "ok";

    /// The line already names the task, so a background result unfolds to
    /// its output alone, and its dot says how the task ended.
    #[test_case(Some(TaskEnd::Failed), NOTICE_OUTPUT, Some(BackgroundOutcome::Error) ; "a_result")]
    #[test_case(None, NOTICE_BODY, None ; "a_plain_observation")]
    fn a_restored_notice_unfolds_to_what_it_reports(
        end: Option<TaskEnd>,
        detail: &str,
        task_end: Option<BackgroundOutcome>,
    ) {
        let mut observation = Message::observation(NOTICE_BODY.into());
        observation.display_text = Some(NOTICE_LINE.into());
        observation.settles = end.map(|end| TaskSettled {
            task: TASK_ID.into(),
            end,
        });

        let display = history_to_display(
            &[observation],
            &empty_outputs(),
            &ToolOutputLines::default(),
        )
        .0;

        assert_eq!(display[0].detail.as_deref(), Some(detail));
        assert_eq!(display[0].task_end, task_end);
    }

    fn tool_use_pair(
        tool: &str,
        input: serde_json::Value,
        result: &str,
        is_error: bool,
    ) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("t1", tool, input)],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("t1", result, is_error)],
                ..Default::default()
            },
        ]
    }

    #[test_case(false, ToolStatus::Success ; "success")]
    #[test_case(true,  ToolStatus::Error   ; "error")]
    fn history_tool_result_status(is_error: bool, expected: ToolStatus) {
        let msgs = tool_use_pair(
            "bash",
            serde_json::json!({"command": "ls"}),
            "output",
            is_error,
        );
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default()).0;
        assert_eq!(display.len(), 1);
        assert!(matches!(&display[0].role, DisplayRole::Tool(t) if t.status == expected));
    }

    #[test]
    fn history_mixed_conversation() {
        let msgs = vec![
            Message::user("do something".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Sure, let me help.".into(),
                    },
                    ContentBlock::tool_use("t1", "bash", serde_json::json!({"command": "echo hi"})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result("t1", "hi", false)],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Done!".into(),
                }],
                ..Default::default()
            },
        ];
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default()).0;
        assert_eq!(display.len(), 4);
        assert_eq!(display[0].role, DisplayRole::User);
        assert_eq!(display[1].role, DisplayRole::Assistant);
        assert!(matches!(display[2].role, DisplayRole::Tool(_)));
        assert_eq!(display[3].role, DisplayRole::Assistant);
        assert_eq!(display[3].text, "Done!");
    }

    #[test]
    fn history_stored_output_variants_pass_through() {
        let variants: Vec<(&str, serde_json::Value, ToolOutput)> = vec![
            (
                "edit",
                serde_json::json!({"path": "a", "old_string": "x", "new_string": "y"}),
                ToolOutput::Diff {
                    path: "a".into(),
                    before: "x\n".into(),
                    after: "y\n".into(),
                    summary: "edited a".into(),
                },
            ),
            (
                "read",
                serde_json::json!({"path": "/src/main.rs"}),
                ToolOutput::ReadCode {
                    path: "/src/main.rs".into(),
                    start_line: 1,
                    lines: vec!["fn main() {}".into()],
                    total_lines: 1,
                    instructions: None,
                },
            ),
            (
                "grep",
                serde_json::json!({"pattern": "TODO"}),
                ToolOutput::GrepResult { entries: vec![] },
            ),
            (
                "todo_write",
                serde_json::json!({"todos": []}),
                ToolOutput::Plain("Todos cleared".into()),
            ),
        ];
        for (tool_name, input_json, output) in variants {
            let discriminant = std::mem::discriminant(&output);
            let msgs = tool_use_pair(tool_name, input_json, "ok", false);
            let outputs = HashMap::from([("t1".into(), Arc::new(output))]);
            let display = history_to_display(&msgs, &outputs, &ToolOutputLines::default()).0;
            assert_eq!(
                std::mem::discriminant(display[0].tool_output.as_deref().unwrap()),
                discriminant,
                "stored {tool_name} output should pass through"
            );
        }
    }

    #[test]
    fn history_stored_write_has_annotation() {
        let write_output = ToolOutput::WriteCode {
            path: "/src/main.rs".into(),
            byte_count: 12,
            lines: vec!["fn main() {}".into()],
        };
        let msgs = tool_use_pair(
            "write",
            serde_json::json!({"path": "/src/main.rs", "content": "fn main() {}"}),
            "wrote 12 bytes",
            false,
        );
        let outputs = HashMap::from([("t1".into(), Arc::new(write_output))]);
        let display = history_to_display(&msgs, &outputs, &ToolOutputLines::default()).0;
        assert!(display[0].annotation.is_some());
    }

    #[test]
    fn history_bash_output_truncated() {
        let long_output = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>();
        let joined = long_output.join("\n");
        let msgs = tool_use_pair(
            "bash",
            serde_json::json!({"command": "cmd"}),
            &joined,
            false,
        );
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default()).0;
        let line_count = display[0].text.lines().count();
        assert!(
            line_count < long_output.len(),
            "output should be truncated, got {line_count} lines for {} input lines",
            long_output.len()
        );
    }
    #[test]
    fn history_no_stored_output_falls_back_to_plain_text() {
        let msgs = tool_use_pair(
            "read",
            serde_json::json!({"path": "/src/main.rs"}),
            "1: fn main() {}",
            false,
        );
        let display = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default()).0;
        assert!(display[0].tool_output.is_none());
        assert!(display[0].text.contains("fn main"));
    }

    #[test]
    fn history_to_display_thinking_blocks() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
                ContentBlock::RedactedThinking { data: "x".into() },
            ],
            ..Default::default()
        }];
        let display = history_to_display(&msgs, &HashMap::new(), &ToolOutputLines::default()).0;
        assert_eq!(display.len(), 2);
        assert_eq!(display[0].role, DisplayRole::Thinking);
        assert_eq!(display[0].text, "reasoning");
        assert_eq!(display[1].role, DisplayRole::Assistant);
    }

    const RESTORE_OUTPUT: &str = "rendered output";

    fn tool_msg_with_input(tool: &str) -> DisplayMessage {
        let mut msg = DisplayMessage::new(DisplayRole::User, String::new());
        msg.role = DisplayRole::Tool(Box::new(ToolRole {
            id: "t1".into(),
            status: ToolStatus::Success,
            name: tool.into(),
        }));
        msg.tool_raw_input = Some(Arc::new(serde_json::json!({ "q": tool })));
        msg.tool_output = Some(Arc::new(ToolOutput::Plain(RESTORE_OUTPUT.into())));
        msg
    }

    const RESTORE_THEME_GEN: u64 = 7;

    #[test]
    fn restore_item_for_round_trips_fields() {
        let msg = tool_msg_with_input("bash");
        let item = restore_item_for(&msg, ToolOutputLines::default(), RESTORE_THEME_GEN)
            .expect("tool message with input and output must produce a RestoreItem");
        assert_eq!(&*item.tool, "bash");
        assert_eq!(item.tool_use_id, "t1");
        assert!(!item.is_error);
        assert_eq!(item.output, RESTORE_OUTPUT);
        assert_eq!(item.theme_gen, Some(RESTORE_THEME_GEN));
        assert_eq!(item.input, serde_json::json!({ "q": "bash" }));
    }

    #[test]
    fn restore_item_for_skips_structured_outputs_rust_renders() {
        let mut msg = tool_msg_with_input("edit");
        msg.tool_output = Some(Arc::new(edit_output("/src/main.rs")));
        assert!(restore_item_for(&msg, ToolOutputLines::default(), RESTORE_THEME_GEN).is_none());
    }

    #[test]
    fn history_structured_output_produces_no_restore_item() {
        let msgs = tool_use_pair(
            "edit",
            serde_json::json!({"path": "a", "old_string": "x", "new_string": "y"}),
            "edited a",
            false,
        );
        let outputs = HashMap::from([("t1".to_owned(), Arc::new(edit_output("a")))]);
        let (_, items) = history_to_display(&msgs, &outputs, &ToolOutputLines::default());
        assert!(items.is_empty(), "Rust owns Diff rendering on restore");

        let (_, items) = history_to_display(&msgs, &empty_outputs(), &ToolOutputLines::default());
        assert_eq!(items.len(), 1, "text-only history still restores via Lua");
    }

    #[test]
    fn restore_item_for_returns_none_when_data_missing() {
        let tol = ToolOutputLines::default();

        let plain = DisplayMessage::new(DisplayRole::Assistant, "hi".into());
        assert!(restore_item_for(&plain, tol, RESTORE_THEME_GEN).is_none());

        let mut no_input = tool_msg_with_input("bash");
        no_input.tool_raw_input = None;
        assert!(restore_item_for(&no_input, tol, RESTORE_THEME_GEN).is_none());

        let mut no_output = tool_msg_with_input("bash");
        no_output.tool_output = None;
        assert!(restore_item_for(&no_output, tol, RESTORE_THEME_GEN).is_none());
    }

    #[test]
    fn compaction_done_flushes_streaming_buffers() {
        let mut chat = chat();

        chat.handle_event(
            AgentEvent::AutoCompacting {
                context_size: 0,
                context_window: 0,
            },
            None,
        );
        assert_eq!(chat.message_count(), 1);

        chat.handle_event(
            AgentEvent::TextDelta {
                text: "summary".into(),
            },
            None,
        );
        chat.handle_event(
            AgentEvent::ThinkingDelta {
                text: "thinking".into(),
            },
            None,
        );
        assert!(!chat.streaming_text_is_empty());
        assert!(!chat.streaming_thinking_is_empty());

        chat.handle_event(
            AgentEvent::CompactionDone {
                context_size_before: 0,
                context_size_after: 0,
                context_window: 0,
                summary: String::new(),
            },
            None,
        );
        assert!(chat.streaming_text_is_empty());
        assert!(chat.streaming_thinking_is_empty());
        assert_eq!(chat.message_count(), 3);

        chat.handle_event(AgentEvent::TextDelta { text: "new".into() }, None);
        chat.flush();
        assert_eq!(chat.message_count(), 4);
        assert_eq!(chat.last_message_text(), "new");
    }

    /// The transcript keeps growing after the ending, since the subagent chat
    /// stays on screen while the parent turn talks on. A fix aimed at the last
    /// message would eat whatever landed in between.
    #[test]
    fn a_correction_rewrites_the_recorded_bubble_not_the_last_one() {
        let mut chat = chat();
        end(&mut chat, TaskOutcome::Unknown);
        let ending = chat.message_count() - 1;

        chat.show_user_message(USER_TEXT, Vec::new());
        text_delta(&mut chat, REPLY_TEXT);
        chat.flush();
        let before = chat.message_count();

        end(&mut chat, TaskOutcome::Error);

        assert_eq!(chat.message_count(), before);
        let bubble = chat
            .message_at(ending)
            .expect("recorded ending still there");
        assert_eq!(bubble.text, ERROR_TEXT);
        assert_eq!(bubble.role, DisplayRole::Error);
        assert_eq!(
            chat.message_at(ending + 1).map(|m| &m.role),
            Some(&DisplayRole::User)
        );
        assert_eq!(
            chat.message_at(ending + 1).map(|m| m.text.as_str()),
            Some(USER_TEXT)
        );
        assert_eq!(chat.last_message_text(), REPLY_TEXT);
        assert_eq!(chat.last_message_role(), Some(&DisplayRole::Assistant));
    }

    /// The stream is flushed before the ending is pushed, so a reply still in
    /// the buffer keeps its place ahead of it and the recorded index points at
    /// the ending, not at the flushed text.
    #[test]
    fn a_pending_stream_lands_before_the_ending_bubble() {
        let mut chat = chat();
        text_delta(&mut chat, REPLY_TEXT);
        assert!(!chat.streaming_text_is_empty());

        end(&mut chat, TaskOutcome::Unknown);

        assert!(chat.streaming_text_is_empty());
        assert_eq!(chat.message_count(), 2);
        assert_eq!(
            chat.message_at(0).map(|m| m.text.as_str()),
            Some(REPLY_TEXT)
        );
        assert_eq!(chat.last_message_text(), DONE_TEXT);

        end(&mut chat, TaskOutcome::Error);

        assert_eq!(chat.message_count(), 2);
        assert_eq!(
            chat.message_at(0).map(|m| m.text.as_str()),
            Some(REPLY_TEXT)
        );
        assert_eq!(
            chat.message_at(0).map(|m| &m.role),
            Some(&DisplayRole::Assistant)
        );
        assert_eq!(chat.last_message_text(), ERROR_TEXT);
        assert_eq!(chat.last_message_role(), Some(&DisplayRole::Error));
    }

    /// Every order two endings can arrive in. Only the placeholder gives way,
    /// a verdict is never walked back, and no order grows a second bubble.
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Done, DONE_TEXT, DisplayRole::Done, TaskStatus::Done   ; "placeholder_settles_as_done")]
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Error, ERROR_TEXT, DisplayRole::Error, TaskStatus::Error ; "placeholder_corrected_to_error")]
    #[test_case(TaskOutcome::Done, TaskOutcome::Error, DONE_TEXT, DisplayRole::Done, TaskStatus::Done     ; "verdict_survives_late_error")]
    #[test_case(TaskOutcome::Error, TaskOutcome::Done, ERROR_TEXT, DisplayRole::Error, TaskStatus::Error  ; "verdict_survives_late_done")]
    #[test_case(TaskOutcome::Done, TaskOutcome::Done, DONE_TEXT, DisplayRole::Done, TaskStatus::Done      ; "repeated_verdict")]
    #[test_case(TaskOutcome::Unknown, TaskOutcome::Unknown, DONE_TEXT, DisplayRole::Done, TaskStatus::Done ; "repeated_placeholder")]
    fn a_second_ending_never_adds_a_bubble(
        first: TaskOutcome,
        second: TaskOutcome,
        text: &str,
        role: DisplayRole,
        status: TaskStatus,
    ) {
        let mut chat = chat();
        chat.show_user_message(USER_TEXT, Vec::new());
        end(&mut chat, first);
        let count = chat.message_count();

        end(&mut chat, second);

        assert_eq!(chat.message_count(), count);
        assert_eq!(chat.last_message_text(), text);
        assert_eq!(chat.last_message_role(), Some(&role));
        assert_eq!(chat.task_status(), status);
    }

    /// The shape `maki.task.list()` serializes: the main chat is not a task,
    /// and a subagent is one from the moment it starts, working until an
    /// ending lands on it.
    #[test]
    fn only_a_subagent_reports_a_task_and_its_status() {
        let main = chat();
        assert!(main.task_id().is_none());
        assert!(!main.is_finished());

        let mut sub = subagent_chat();
        assert_eq!(sub.task_id().map(|id| &**id), Some(TASK_ID));
        assert_eq!(sub.task_status(), TaskStatus::Working);
        assert!(!sub.is_finished());

        end(&mut sub, TaskOutcome::Error);

        assert!(sub.is_finished());
        assert_eq!(sub.task_status(), TaskStatus::Error);
        assert_eq!(sub.task_id().map(|id| &**id), Some(TASK_ID));
    }

    /// Once its run ends, only another run brings a task back. A woken run
    /// opens with no prompt, and what trails a run says nothing of the next.
    #[test_case(AgentEvent::TextDelta { text: USER_TEXT.into() }, TaskStatus::Working ; "a_run_streams")]
    #[test_case(AgentEvent::ObservationLine { text: USER_TEXT.into(), detail: None, end: None }, TaskStatus::Working ; "a_woken_run_lands_its_notice")]
    #[test_case(AgentEvent::SubagentHistory { tool_use_id: TASK_ID.into(), messages: Vec::new() }, TaskStatus::Idle ; "transcript_trails_the_run")]
    #[test_case(AgentEvent::StreamClosed, TaskStatus::Idle ; "close_trails_the_run")]
    fn only_a_run_event_brings_an_idle_task_back(event: AgentEvent, status: TaskStatus) {
        let mut sub = subagent_chat();
        sub.plugin = Some(TASK_PLUGIN.into());
        sub.track_run(&AgentEvent::Error {
            message: ERROR_TEXT.into(),
            auth: false,
            totals: RunTotals::default(),
        });
        assert_eq!(sub.task_status(), TaskStatus::Idle);

        sub.track_run(&event);

        assert_eq!(sub.task_status(), status);
    }

    const STEER_TEXT: &str = "keep going";
    const ASK_REASON: &str = "plugin wants a human to look";
    const EXPECTED_PERMISSION_REQUEST: &str = "a permission request must reach the caller";

    #[test_case(SteerKind::MessageRewritten, REWRITTEN_PREFIX ; "message_rewritten")]
    #[test_case(SteerKind::MessageDropped, DROPPED_PREFIX ; "message_dropped")]
    #[test_case(SteerKind::Continued, CONTINUED_PREFIX ; "continued")]
    fn steered_shows_one_prefixed_assistant_message(kind: SteerKind, prefix: &str) {
        let mut chat = chat();
        chat.handle_event(
            AgentEvent::Steered {
                kind,
                text: STEER_TEXT.into(),
            },
            None,
        );
        assert_eq!(chat.message_count(), 1);
        let text = chat.last_message_text();
        assert!(
            text.starts_with(prefix) && text.ends_with(STEER_TEXT),
            "{text}"
        );
        assert_eq!(chat.last_message_role(), Some(&DisplayRole::Assistant));
    }

    #[test]
    fn permission_request_carries_reason() {
        let mut chat = chat();
        let result = chat.handle_event(
            AgentEvent::PermissionRequest {
                id: TASK_ID.into(),
                tool: ToolKey::native("bash"),
                scopes: Vec::new(),
                reason: Some(ASK_REASON.into()),
            },
            None,
        );
        let ChatEventResult::PermissionRequest { id, reason, .. } = result else {
            panic!("{EXPECTED_PERMISSION_REQUEST}");
        };
        assert_eq!(id, TASK_ID);
        assert_eq!(reason.as_deref(), Some(ASK_REASON));
    }

    const TEST_ITEM_LABEL: &str = "monitor";
    const TEST_ITEM_TITLE: &str = "watch tests";
    const TEST_ITEM_DETAIL: &str = "exited (code 0)";

    fn chat_item(status: ChatItemStatus, detail: Option<String>) -> ChatItem {
        ChatItem {
            id: format!("{TEST_ITEM_LABEL}:1").into(),
            label: TEST_ITEM_LABEL.into(),
            title: TEST_ITEM_TITLE.into(),
            status,
            detail,
        }
    }

    fn tool_role(chat: &Chat, index: usize) -> (&ToolRole, &str) {
        let msg = chat.messages_panel.message_at(index).unwrap();
        let DisplayRole::Tool(t) = &msg.role else {
            panic!("expected a tool-shaped message");
        };
        (t, &msg.text)
    }

    #[test]
    fn chat_item_running_creates_a_tool_shaped_item() {
        let mut chat = chat();
        chat.apply_chat_item(&chat_item(ChatItemStatus::Running, None));
        let (role, text) = tool_role(&chat, 0);
        assert_eq!(&*role.name, TEST_ITEM_LABEL);
        assert_eq!(role.status, ToolStatus::InProgress);
        assert_eq!(text, TEST_ITEM_TITLE);
    }

    #[test]
    fn chat_item_done_closes_with_the_detail() {
        let mut chat = chat();
        chat.apply_chat_item(&chat_item(ChatItemStatus::Running, None));
        chat.apply_chat_item(&chat_item(
            ChatItemStatus::Done,
            Some(TEST_ITEM_DETAIL.into()),
        ));
        assert_eq!(
            chat.messages_panel.message_count(),
            1,
            "done updates in place"
        );
        let (role, text) = tool_role(&chat, 0);
        assert_eq!(role.status, ToolStatus::Success);
        assert_eq!(text, TEST_ITEM_DETAIL);
    }

    #[test]
    fn chat_item_failed_marks_the_item_as_error() {
        let mut chat = chat();
        chat.apply_chat_item(&chat_item(ChatItemStatus::Running, None));
        chat.apply_chat_item(&chat_item(ChatItemStatus::Failed, None));
        let (role, _) = tool_role(&chat, 0);
        assert_eq!(role.status, ToolStatus::Error);
    }

    #[test]
    fn closing_an_unknown_chat_item_stays_quiet() {
        let mut chat = chat();
        chat.apply_chat_item(&chat_item(ChatItemStatus::Done, None));
        assert_eq!(chat.messages_panel.message_count(), 0);
    }
}
