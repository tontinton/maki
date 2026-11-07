use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::{AgentEvent, Envelope, SubagentInfo, ToolDoneEvent};
use maki_providers::{TokenUsage, add_cost};
use maki_storage::sessions::StoredTokenUsage;

use super::{
    AUTH_EXPIRED_MSG, App, ERROR_BUBBLE_MAX_CHARS, ExitOnDone, MAIN_CHAT, MISSING_TOOL_COMPLETION,
    Mode, PlanTrigger, RESTORE_RUN_ID,
};
use crate::app::tasks::{TaskClock, TaskOutcome, TaskParent};
use crate::chat::{Chat, ChatEventResult, DONE_TEXT, ERROR_TEXT};
use crate::components::permission_prompt::ProjectAnswers;
use crate::components::{
    BackgroundOutcome, DisplayMessage, DisplayRole, ExitRequest, RetryInfo, Status,
};
use crate::markdown::TRUNCATION_PREFIX;

fn cap_error_text(message: &str) -> String {
    match message.char_indices().nth(ERROR_BUBBLE_MAX_CHARS) {
        Some((end, _)) => format!("{}{TRUNCATION_PREFIX}", &message[..end]),
        None => message.to_owned(),
    }
}

/// The task an event belongs to: a transcript names it itself, every other
/// event through its subagent stamp.
fn task_id_of(envelope: &Envelope) -> Option<&str> {
    match &envelope.event {
        AgentEvent::SubagentHistory { tool_use_id, .. } => Some(tool_use_id.as_str()),
        _ => envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.as_str()),
    }
}

fn started_by(info: &SubagentInfo) -> Option<TaskParent> {
    info.parent_call
        .as_deref()
        .map(|call| TaskParent::Started(Arc::from(call)))
}

/// The one path from a `SubagentInfo` into a chat, at its birth and on
/// every later event. A chat can open before its session speaks (a moved
/// call's tab, a subagent a plugin adopted), so each field fills in from
/// the first info that names it.
fn fill_missing_subagent_info(chat: &mut Chat, info: &SubagentInfo) {
    if chat.parent.is_none() {
        chat.parent = started_by(info);
    }
    if chat.is_finished() {
        return;
    }
    if chat.answer_tx.is_none() {
        chat.answer_tx.clone_from(&info.answer_tx);
    }
    // Every subagent has a control, so none yet means this is its first word,
    // unless an ending took it and `Chat::reopen` brought the tab back: that
    // transcript already opens with the prompt.
    if chat.control.is_none() {
        chat.control = Some(Arc::clone(&info.control));
        if let Some(prompt) = &info.prompt
            && !chat.has_user_message()
        {
            chat.push_user_message(prompt);
        }
    }
    if chat.model_id.is_none() {
        chat.model_id.clone_from(&info.model);
    }
    chat.opts = chat.opts.or(info.opts);
}

impl App {
    fn bill(&mut self, chat_idx: usize, model: &str, usage: StoredTokenUsage) {
        self.state.token_usage += TokenUsage::from(usage);
        add_cost(&mut self.state.cost, usage.cost);
        add_cost(&mut self.chats[chat_idx].cost, usage.cost);
        add_cost(
            &mut self.state.subsidised_list_cost,
            usage.subsidised_list_cost,
        );
        add_cost(
            &mut self.chats[chat_idx].list_cost,
            usage.subsidised_list_cost,
        );
        self.state.session_mut().add_model_usage(model, usage);
    }

    /// Run-scoped delivery for the tab's own agent: restore snapshots go to
    /// the chats that have the tool, stale runs are dropped, the rest goes to
    /// [`Self::deliver`].
    pub(super) fn handle_agent_event(&mut self, envelope: Envelope) {
        if envelope.run_id == RESTORE_RUN_ID {
            let (id, snapshot, theme_gen, is_header) = match envelope.event {
                AgentEvent::ToolSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, false),
                AgentEvent::ToolHeaderSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, true),
                _ => return,
            };
            let mut delivered = false;
            for chat in self.chats.iter_mut().filter(|chat| chat.has_tool(&id)) {
                delivered = true;
                if is_header {
                    chat.tool_header_snapshot(&id, snapshot.clone(), theme_gen);
                } else {
                    chat.tool_snapshot(&id, snapshot.clone(), theme_gen);
                }
            }
            if !delivered {
                tracing::debug!(tool_id = %id, "restore snapshot dropped: no chat has this tool");
            }
            return;
        }
        // Taken before the run check, since a moved call keeps reporting
        // after its turn ended.
        if let AgentEvent::ToolParts { id, parts } = envelope.event {
            self.set_parts(id, parts);
            return;
        }
        // What an adopted subagent sent before its call moved belongs to its
        // task, not to the run, and may land after the next run started.
        if let Some(plugin) = task_id_of(&envelope).and_then(|id| self.task_plugin(id)) {
            self.deliver(envelope, Some(plugin));
            return;
        }
        if envelope.run_id != self.run_id && !self.is_racing_receipt(&envelope) {
            // A snapshot dropped here degrades the tool body to llm_output.
            if let AgentEvent::ToolSnapshot { id, .. }
            | AgentEvent::ToolHeaderSnapshot { id, .. }
            | AgentEvent::LiveToolBuf { id, .. } = &envelope.event
            {
                tracing::debug!(
                    tool_id = %id,
                    event_run_id = envelope.run_id,
                    current_run_id = self.run_id,
                    "tool render event dropped: stale run_id"
                );
            }
            return;
        }
        self.deliver(envelope, None);
    }

    /// A move racing Esc lands after the run it moved out of. Its task lives
    /// on, so the row it moved, while still on screen, needs its tab. Only a
    /// main chat call: Esc ends the subagents with the run, and a call moved
    /// out of one has no agent left to report to.
    fn is_racing_receipt(&self, envelope: &Envelope) -> bool {
        envelope.subagent.is_none()
            && matches!(&envelope.event, AgentEvent::ToolDone(done)
                if done.backgrounded && self.chats[MAIN_CHAT].tool_message(&done.id).is_some())
    }

    /// `plugin` names the plugin whose session sent the event, which the chat
    /// shows as a task of that plugin. Those events skip the run gate above:
    /// the session outlives turns, Esc and rewinds, so the only gate is the
    /// chat session, which the event loop already matched.
    pub(crate) fn deliver(&mut self, envelope: Envelope, plugin: Option<Arc<str>>) {
        let Some(envelope) = self.route_outside_chats(envelope, plugin.is_some()) else {
            return;
        };

        maki_lua::agent_autocmd::dispatch(
            &self.lua_event_handle,
            &self.state.session.id,
            &envelope,
        );

        let subagent_id = envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.clone());

        let is_plugin_task = plugin.is_some();
        let chat_idx = match envelope.subagent {
            Some(ref subagent) => self.resolve_or_create_chat(subagent, &plugin),
            // A plugin event without its task must never land in the main chat.
            None if is_plugin_task => return,
            None => MAIN_CHAT,
        };
        if is_plugin_task && matches!(envelope.event, AgentEvent::StreamClosed) {
            self.close_plugin_task(chat_idx, subagent_id.as_deref());
            return;
        }
        let event = envelope.event;
        self.chats[chat_idx].track_run(&event);

        if let AgentEvent::ToolDone(ref done) = event {
            self.record_tool_done(chat_idx, done);
        }

        if let AgentEvent::Retry {
            attempt,
            message,
            delay_ms,
        } = event
        {
            self.chats[chat_idx].stream_reset();
            if chat_idx == MAIN_CHAT {
                self.retry_info = Some(RetryInfo {
                    attempt,
                    message,
                    deadline: Instant::now() + Duration::from_millis(delay_ms),
                });
            }
            return;
        }

        // A plugin task streams while the main chat may be mid-retry.
        if chat_idx == MAIN_CHAT {
            self.retry_info = None;
        }

        self.account_usage(chat_idx, subagent_id.as_deref(), &event);

        let Some(event) = self.answer_request(chat_idx, subagent_id, event) else {
            return;
        };

        let plan_path = if self.state.mode == Mode::Plan {
            self.state.plan.path()
        } else {
            None
        };
        let result = self.chats[chat_idx].handle_event(event, plan_path);

        if is_plugin_task {
            self.end_plugin_prompt(chat_idx, result);
        } else if chat_idx == MAIN_CHAT {
            self.end_main_turn(result);
        }
    }

    /// What never reaches a chat's stream: events of a deleted task, a
    /// closed transcript, and a moved call's final result. Hands the
    /// envelope back for everything else.
    fn route_outside_chats(&mut self, envelope: Envelope, from_plugin: bool) -> Option<Envelope> {
        if task_id_of(&envelope).is_some_and(|id| self.deleted_tasks.contains(id)) {
            return None;
        }

        if let AgentEvent::SubagentHistory {
            tool_use_id,
            messages,
        } = envelope.event
        {
            // Workflow sessions use synthetic ids that no ToolDone will match,
            // so we finish them here on SubagentHistory. This event only knows
            // that the transcript closed, so say Unknown and leave the verdict
            // to the ToolDone that follows elsewhere. A plugin task is never in
            // `chat_index`, and rightly stays open here: it writes its
            // transcript after every prompt, and its close is `StreamClosed`.
            if let Some(&sub_idx) = self.chat_index.get(tool_use_id.as_str()) {
                self.chats[sub_idx].mark_finished(TaskOutcome::Unknown, DONE_TEXT);
            }
            self.state
                .session_mut()
                .set_subagent_messages(tool_use_id, messages, |m| {
                    m.tool_uses().map(|(id, _, _)| id.to_owned()).collect()
                });
            return None;
        }

        // A moved call's final result names the call, and the envelope names
        // no task or that same one. It ends the task and never reaches
        // history: the receipt is what the model got for that call. A
        // `ToolDone` inside another task's session is that session's own row.
        if from_plugin
            && let AgentEvent::ToolDone(done) = &envelope.event
            && envelope
                .subagent
                .as_ref()
                .is_none_or(|s| s.parent_tool_use_id == done.id)
        {
            match self.chat_of_task(&done.id) {
                Ok(idx) if idx != MAIN_CHAT && self.chats[idx].plugin.is_some() => {
                    self.finish_background_call(idx, envelope.event);
                    return None;
                }
                // A subagent's tab is the turn's until the receipt adopts it,
                // and the turn may have ended it by then. The result waits so
                // the adoption can tell, see `App::adopt_subagent`.
                Ok(idx) if idx != MAIN_CHAT => {
                    self.early_finals.insert(done.id.clone(), envelope.event);
                    return None;
                }
                // A deleted tab's receipt never comes back to claim it.
                Err(_) if envelope.subagent.is_none() => {
                    if !self.deleted_tasks.contains(&done.id) {
                        self.early_finals.insert(done.id.clone(), envelope.event);
                    }
                    return None;
                }
                _ => {}
            }
        }
        Some(envelope)
    }

    /// The session is gone. If Esc already ended the task, its `Error`
    /// stays. A close mid-prompt leaves tools without a result, and nothing
    /// after this would settle them. The close knows no verdict, so the final
    /// result of an adopted subagent's call can still refine it, and its
    /// badge.
    fn close_plugin_task(&mut self, chat_idx: usize, subagent_id: Option<&str>) {
        let chat = &mut self.chats[chat_idx];
        chat.cancel_in_progress();
        if !chat.is_finished() {
            chat.mark_finished(TaskOutcome::Unknown, DONE_TEXT);
            if let Some(id) = subagent_id {
                self.set_badge_outcome(id, BackgroundOutcome::Done);
            }
        }
    }

    fn record_tool_done(&mut self, chat_idx: usize, done: &ToolDoneEvent) {
        if self.state.mode == Mode::Plan
            && self.state.plan.path().is_some_and(|pp| done.wrote_to(pp))
        {
            self.transition_plan(PlanTrigger::WriteDone);
        }
        self.state
            .session_mut()
            .insert_tool_output(done.id.clone(), done.output.clone());
        // A receipt does not end the subagent behind it: the call goes
        // on, and adopting its chat takes it off the turn first.
        if done.backgrounded {
            self.open_background_call(chat_idx, done);
        } else if let Some(&sub_idx) = self.chat_index.get(&done.id) {
            let (outcome, text) = if done.is_error {
                (TaskOutcome::Error, ERROR_TEXT)
            } else {
                (TaskOutcome::Done, DONE_TEXT)
            };
            self.chats[sub_idx].mark_finished(outcome, text);
        }
    }

    fn account_usage(&mut self, chat_idx: usize, subagent_id: Option<&str>, event: &AgentEvent) {
        if let AgentEvent::TurnComplete(tc) = event {
            self.bill(
                chat_idx,
                &tc.model,
                tc.usage
                    .billed_with_subsidised_list_cost(tc.cost, tc.subsidised_list_cost),
            );
            let ctx_size = tc
                .context_size
                .unwrap_or_else(|| tc.usage.context_tokens().min(u32::MAX as u64) as u32);
            self.set_context_size(chat_idx, ctx_size);
            self.chats[chat_idx].set_pending_turn_usage(tc.usage.format(tc.cost));
            if let Some(tool_id) = subagent_id {
                let formatted = tc.usage.format_sum_cost(self.chats[chat_idx].cost);
                self.chats[MAIN_CHAT].set_tool_turn_usage(tool_id, formatted);
            }
        }

        // Compaction is the one thing that lowers the context size with no turn
        // behind it. The number is stored in the session meta and seeds the
        // next run's gauge, so left stale a session compacted just before exit
        // gets compacted again on resume.
        if let AgentEvent::CompactionDone {
            context_size_after, ..
        } = event
        {
            self.set_context_size(chat_idx, *context_size_after);
        }
    }

    /// The events that ask the App rather than the chat for something: a
    /// queued message going out, a permission prompt, a login. Hands any
    /// other event back for the chat.
    fn answer_request(
        &mut self,
        chat_idx: usize,
        subagent_id: Option<String>,
        event: AgentEvent,
    ) -> Option<AgentEvent> {
        match event {
            AgentEvent::QueueItemConsumed { text, images } => {
                if chat_idx == MAIN_CHAT {
                    self.on_queue_item_consumed(text, images);
                } else {
                    self.chats[chat_idx].show_user_message(text, images);
                }
            }
            AgentEvent::PermissionRequest {
                id,
                tool,
                scopes,
                reason,
            } => {
                let project_answers =
                    ProjectAnswers::new(self.permissions.project_is_trusted(), &self.trust);
                self.open_prompt(|app| {
                    app.permission_prompt.push(
                        id,
                        tool,
                        scopes,
                        subagent_id,
                        project_answers,
                        reason,
                    );
                });
            }
            AgentEvent::AuthRequired => self.park_for_auth(chat_idx),
            event => return Some(event),
        }
        None
    }

    fn park_for_auth(&mut self, chat_idx: usize) {
        self.chats[chat_idx].push(DisplayMessage::new(
            DisplayRole::Error,
            AUTH_EXPIRED_MSG.into(),
        ));
        if self.answer_channel(chat_idx).is_none() {
            return;
        }
        if chat_idx != MAIN_CHAT {
            self.main_chat().push(DisplayMessage::new(
                DisplayRole::Error,
                AUTH_EXPIRED_MSG.into(),
            ));
        }
        let park = |app: &mut Self| app.chats[chat_idx].auth_parked = true;
        if self.chats[chat_idx].plugin.is_none() {
            self.open_prompt(park);
        } else {
            park(self);
        }
    }

    /// Either way the prompt has returned, and the session waits for the next
    /// one. A run its Lua task's end cut short mid-tool has no
    /// `handle_cancel` to settle the tool, so it is settled here.
    fn end_plugin_prompt(&mut self, chat_idx: usize, result: ChatEventResult) {
        let chat = &mut self.chats[chat_idx];
        match result {
            ChatEventResult::Done => chat.cancel_in_progress(),
            ChatEventResult::Error(message) => {
                chat.cancel_in_progress();
                // Esc in the tab already wrote the task's ending, and the
                // error its cut short run sends after that would say it twice.
                if !chat.is_finished() {
                    chat.push(DisplayMessage::new(
                        DisplayRole::Error,
                        cap_error_text(&message),
                    ));
                }
            }
            ChatEventResult::Continue => {}
        }
    }

    fn end_main_turn(&mut self, result: ChatEventResult) {
        match result {
            ChatEventResult::Done => {
                self.status_bar.clear_flash();
                self.terminalize_turn(MISSING_TOOL_COMPLETION);
                self.chat_index.clear();
                self.status = Status::Idle;
                if let Some(ExitOnDone { background_wait }) = self.exit_on_done {
                    self.exit_request = ExitRequest::Success;
                    self.background_deadline = self
                        .background_deadline
                        .or_else(|| background_wait.map(|wait| Instant::now() + wait));
                }
            }
            ChatEventResult::Error(message) => {
                self.status = Status::error(message.clone());
                self.status_bar.clear_flash();
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Error,
                    cap_error_text(&message),
                ));
                self.terminalize_turn(&message);
                self.recoverable_queue = self.queue.text_messages();
                self.queue.clear();
                self.chat_index.clear();
                if self.exits_on_done() {
                    self.exit_request = ExitRequest::Error;
                }
            }
            ChatEventResult::Continue => {}
        }
    }

    /// Chat 0 is the session itself, and its size is the one stored in the
    /// session meta.
    fn set_context_size(&mut self, chat_idx: usize, size: u32) {
        self.chats[chat_idx].context_size = size;
        if chat_idx == MAIN_CHAT {
            self.state.context_size = size;
        }
    }

    /// A tool task is found through `chat_index`, a plugin task by id: the
    /// index is wiped every turn, and a plugin task would come back as a
    /// second chat at the main chat's next turn end. A plugin event naming a
    /// subagent's chat adopts it, see [`Self::adopt_subagent`].
    fn resolve_or_create_chat(
        &mut self,
        subagent: &SubagentInfo,
        plugin: &Option<Arc<str>>,
    ) -> usize {
        let id = &subagent.parent_tool_use_id;
        let known = match plugin {
            None => self.chat_index.get(id.as_str()).copied(),
            Some(_) => self.chat_of_task(id).ok(),
        };
        if let Some(idx) = known {
            if let Some(plugin) = plugin {
                self.adopt_subagent(idx, plugin);
            }
            fill_missing_subagent_info(&mut self.chats[idx], subagent);
            return idx;
        }
        let idx = self.chats.len();
        if plugin.is_none() {
            self.chat_index.insert(id.clone(), idx);
            self.chats[MAIN_CHAT].update_tool_summary(id, &subagent.name);
            if let Some(ref model) = subagent.model {
                self.chats[MAIN_CHAT].update_tool_model(id, model);
            }
        }
        let mut chat = Chat::new(
            self.state.session.id,
            Some(id),
            subagent.name.clone(),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        chat.set_restore_channel(self.restore_event_tx.clone());
        chat.plugin = plugin.clone();
        chat.clock = Some(TaskClock::start());
        fill_missing_subagent_info(&mut chat, subagent);
        self.chats.push(chat);
        idx
    }

    /// A chat this task already has, maybe one the user just stopped, stays as
    /// it is rather than turning idle again. A subagent's chat is adopted,
    /// which is how a session nested under a moved call leaves the turn.
    pub(crate) fn open_plugin_task(&mut self, info: &SubagentInfo, plugin: Arc<str>) {
        let id = &info.parent_tool_use_id;
        if self.deleted_tasks.contains(id) {
            return;
        }
        let known = self.chat_of_task(id).is_ok();
        let idx = self.resolve_or_create_chat(info, &Some(plugin));
        if !known {
            self.chats[idx].idle = true;
        }
    }
}
