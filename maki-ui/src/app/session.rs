use crate::chat::{Chat, DONE_TEXT, history_to_display};
use crate::components::DisplayRole;
use crate::components::rewind_picker::RewindEntry;
use crate::components::{Action, LoadedSession};
use maki_providers::{Model, TokenUsage};
use maki_storage::sessions::StoredSubagent;

use crate::AppSession;

use super::session_state::{SessionState, stored_to_rules};
use super::{App, Mode, PendingInput, PlanState};
use crate::agent::QueuedMessage;

impl App {
    pub(crate) fn has_messages(&self) -> bool {
        !self.state.session.messages.is_empty()
    }

    pub(crate) fn has_ephemeral(&self) -> bool {
        self.state.session.meta.input_draft.is_some()
            || self.state.session.meta.todo_dismissed
            || !self.state.session.meta.queued_messages.is_empty()
            || self.state.session.meta.mode != Some(maki_storage::sessions::StoredMode::Build)
    }

    pub(crate) fn has_content(&self) -> bool {
        self.has_messages() || self.has_ephemeral()
    }

    pub(crate) fn save_session(&mut self) {
        self.state.sync_session(
            &self.shared_history,
            &self.shared_tool_outputs,
            &self.permissions,
        );
        self.sync_ephemeral_state();
        if !self.has_content() {
            return;
        }
        self.enqueue_save();
    }

    fn sync_ephemeral_state(&mut self) {
        let draft = self.input_box.buffer.value();
        self.state.session.meta.input_draft = if draft.is_empty() { None } else { Some(draft) };

        self.state.session.meta.todo_dismissed = self.chats[0].todo_panel.is_user_dismissed();

        self.state.session.meta.queued_messages = self.queue.text_messages();

        self.state.session.meta.subagents = self
            .chats
            .iter()
            .skip(1)
            .zip(self.chat_index.iter())
            .map(|(chat, (tool_id, _))| StoredSubagent {
                tool_use_id: tool_id.clone(),
                name: chat.name.clone(),
                prompt: None,
                model: chat.model_id.clone(),
            })
            .collect();
    }

    pub(super) fn save_input_history(&self) {
        if let Err(e) = self.input_box.history().save(&self.storage) {
            tracing::warn!(error = %e, "input history save failed");
        }
    }

    pub(super) fn enqueue_save(&self) {
        self.storage_writer
            .send(Box::new(self.state.session.clone()));
    }

    pub(super) fn reset_ui_chrome(&mut self) {
        self.chats.clear();
        self.chats.push(Chat::new("Main".into(), self.ui_config));
        self.active_chat = 0;
        self.chat_index.clear();
        self.status = super::Status::Idle;
        self.queue.clear();
        #[cfg(feature = "demo")]
        {
            self.demo_questions = None;
        }
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.status_bar.clear_flash();
        self.task_picker_original = None;
        self.last_esc = None;
        self.chats[0].todo_panel.reset();
        self.plan_form.reset();
    }

    pub(crate) fn restore_display(&mut self) {
        let display_msgs = history_to_display(
            &self.state.session.messages,
            &self.state.session.tool_outputs,
            &self.ui_config.tool_output_lines,
        );
        self.main_chat().load_messages(display_msgs);
        self.main_chat().token_usage = self.state.token_usage;
        self.main_chat().context_size = self.state.context_size;
        self.chats[0].todo_panel.restore(
            &self.state.session.tool_outputs,
            self.state.session.meta.todo_dismissed,
        );

        if let Some(draft) = self.state.session.meta.input_draft.take() {
            self.input_box.set_input(draft);
            self.input_box.buffer.move_to_end();
        }

        for text in std::mem::take(&mut self.state.session.meta.queued_messages) {
            let msg = QueuedMessage {
                text,
                images: Vec::new(),
            };
            self.queue_and_notify(msg);
        }

        for sa in std::mem::take(&mut self.state.session.meta.subagents) {
            let idx = self.chats.len();
            self.chat_index.insert(sa.tool_use_id.clone(), idx);
            let mut chat = Chat::new(sa.name, self.ui_config);
            chat.model_id = sa.model;
            if let Some(messages) = self.state.session.subagent_messages.get(&sa.tool_use_id) {
                let display = history_to_display(
                    messages,
                    &self.state.session.tool_outputs,
                    &self.ui_config.tool_output_lines,
                );
                chat.load_messages(display);
                chat.mark_finished(DisplayRole::Done, DONE_TEXT);
            }
            self.chats.push(chat);
        }
    }

    fn loaded_session_snapshot(&self) -> LoadedSession {
        LoadedSession {
            messages: self.state.session.messages.clone(),
            tool_outputs: self.state.session.tool_outputs.clone(),
            model_spec: self.state.session.model.clone(),
        }
    }

    pub(super) fn reset_session(&mut self) -> Vec<Action> {
        self.reset_ui_chrome();
        self.state.token_usage = TokenUsage::default();
        self.state.context_size = 0;
        self.state.plan = PlanState::None;
        if self.state.mode == Mode::Plan {
            self.enter_plan();
        }
        self.state.session = AppSession::new(&self.state.session.model, &self.state.session.cwd);
        vec![Action::NewSession]
    }

    pub(super) fn open_rewind_picker(&mut self) -> Vec<Action> {
        self.save_session();
        match self.rewind_picker.open(&self.state.session.messages) {
            Ok(()) => vec![],
            Err(msg) => {
                self.status_bar.flash(msg);
                vec![]
            }
        }
    }

    pub(super) fn rewind_to(&mut self, entry: RewindEntry) -> Vec<Action> {
        self.run_id += 1;

        let stale_ids: Vec<String> = self.state.session.messages[entry.turn_index..]
            .iter()
            .flat_map(|m| m.tool_uses())
            .map(|(id, _, _)| id.to_owned())
            .collect();
        self.state.session.messages.truncate(entry.turn_index);
        for id in &stale_ids {
            self.state.session.tool_outputs.remove(id);
            self.state.session.subagent_messages.remove(id);
        }

        self.reset_ui_chrome();
        self.restore_display();

        self.input_box.set_input(entry.prompt_text);
        self.input_box.buffer.move_to_end();

        self.state.session.update_title_if_default();
        self.enqueue_save();

        vec![Action::LoadSession(Box::new(
            self.loaded_session_snapshot(),
        ))]
    }

    pub(super) fn open_session_picker(&mut self) -> Vec<Action> {
        self.session_picker.open(
            &self.state.session.cwd,
            &self.state.session.id,
            &self.storage,
        );
        vec![]
    }

    pub(crate) fn apply_loaded_session(
        &mut self,
        session: AppSession,
        fallback_model: &Model,
    ) -> LoadedSession {
        self.permissions
            .load_session_rules(stored_to_rules(&session.meta.session_rules));
        self.state = SessionState::from_session(session, fallback_model, &self.storage);
        for w in self.state.warnings.drain(..) {
            self.status_bar.flash(w);
        }
        self.reset_ui_chrome();
        self.restore_display();

        self.enqueue_save();
        self.loaded_session_snapshot()
    }

    pub(super) fn load_session(&mut self, session_id: String) -> Vec<Action> {
        let session = match AppSession::load(&session_id, &self.storage) {
            Ok(s) => s,
            Err(e) => {
                self.status_bar
                    .flash(format!("Failed to load session: {e}"));
                return vec![];
            }
        };
        self.save_session();
        let loaded = self.apply_loaded_session(session, &self.state.model.clone());
        vec![Action::LoadSession(Box::new(loaded))]
    }

    pub(super) fn delete_session(&mut self, session_id: String) -> Vec<Action> {
        if let Err(e) = AppSession::delete(&session_id, &self.storage) {
            self.status_bar
                .flash(format!("Failed to delete session: {e}"));
            return vec![];
        }
        self.session_picker.remove_entry(&session_id);
        self.status_bar.flash("Session deleted".into());
        vec![]
    }
}
