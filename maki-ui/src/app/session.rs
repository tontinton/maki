use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::chat::{Chat, DONE_TEXT, history_to_display};
use crate::components::DisplayRole;
use crate::components::rewind_picker::RewindEntry;
use crate::components::{Action, LoadedSession};
use maki_providers::{Model, TokenUsage};
use maki_storage::id::MakiId;
use maki_storage::sessions::{SessionMeta, StoredSubagent};

use crate::AppSession;

use super::session_state::{SessionState, rules_to_stored, stored_to_rules};
use super::{App, Mode, PendingInput, PlanState};

/// The one content check: `App::checkpoint` saves a session only when this
/// holds, and the shutdown report reuses it to say which tabs were saved, so
/// the report and the disk can never disagree.
pub(crate) fn session_has_content(session: &AppSession) -> bool {
    !session.messages().is_empty()
        || session.meta.input_draft.is_some()
        || !session.meta.queued_messages.is_empty()
        || session.meta.mode != Some(maki_storage::sessions::StoredMode::Build)
}

impl App {
    pub(crate) fn has_content(&self) -> bool {
        session_has_content(&self.state.session)
    }

    /// The event loop runs this once per frame per session. It syncs whatever
    /// the session mirrors from live state, then writes only if a mutator
    /// really changed something. No dirty flags and no per-event save calls,
    /// so there is nothing left to forget.
    pub(crate) fn checkpoint(&mut self) {
        let snapshot = self.shared_history.as_ref().map(|h| h.load_full());
        let meta = self.build_meta();
        AppSession::checkpoint(
            &mut self.state.session,
            snapshot.as_deref(),
            meta,
            self.state.token_usage,
        );

        if !self.has_content() {
            return;
        }
        let stamp = (self.state.session.id, self.state.session.revision());
        if self.last_sent == Some(stamp) {
            return;
        }
        self.storage_writer.send(Arc::clone(&self.state.session));
        self.last_sent = Some(stamp);
    }

    /// Everything the session mirrors from live state, built field by field so
    /// a new `SessionMeta` field forces a decision here. Every frame calls
    /// this, so it must stay allocation-free while the UI is idle: the empty
    /// draft, queue and rule list below all collect into an empty `Vec`, which
    /// does not allocate.
    fn build_meta(&self) -> SessionMeta {
        let state = &self.state;
        let draft = self.input_box.buffer.value();
        SessionMeta {
            mode: Some(state.mode.into()),
            plan_path: state.plan.path().map(|p| p.to_string_lossy().into_owned()),
            plan_written: state.plan.is_ready(),
            session_rules: rules_to_stored(&self.permissions.session_rules_snapshot()),
            context_size: state.context_size,
            input_draft: (!draft.is_empty()).then_some(draft),
            queued_messages: if self.recoverable_queue.is_empty() {
                self.queue.text_messages()
            } else {
                self.recoverable_queue.clone()
            },
            thinking: Some(state.thinking.into()),
            fast: state.fast,
            workflow: state.workflow,
        }
    }

    /// Called where the set of subagent tabs changes, not at checkpoint time:
    /// the turn-end path clears `chat_index` right after pruning it, so a later
    /// rebuild would only ever find an empty map.
    pub(super) fn sync_subagents(&mut self) {
        let mut ordered: Vec<_> = self.chat_index.iter().collect();
        ordered.sort_by_key(|&(_, chat_index)| chat_index);
        let subagents = ordered
            .into_iter()
            .map(|(tool_id, &chat_index)| {
                let chat = &self.chats[chat_index];
                StoredSubagent {
                    tool_use_id: tool_id.clone(),
                    name: chat.name.clone(),
                    prompt: None,
                    model: chat.model_id.clone(),
                }
            })
            .collect();
        self.state.session_mut().set_subagents(subagents);
    }

    pub(super) fn save_input_history(&self) {
        if let Err(e) = self.input_box.history().save(&self.storage) {
            tracing::warn!(error = %e, "input history save failed");
        }
    }

    pub(super) fn reset_ui_chrome(&mut self) {
        self.chats.clear();
        let mut main = Chat::new(
            "Main".into(),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        main.set_restore_channel(self.restore_event_tx.clone());
        self.chats.push(main);
        self.active_chat = 0;
        self.chat_index.clear();
        self.status = super::Status::Idle;
        self.queue.clear();
        self.recoverable_queue.clear();
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.status_bar.clear_flash();
        self.task_picker_original = None;
        self.last_esc = None;
        self.restoring = Arc::new(AtomicBool::new(false));
        self.plan_form.reset();
    }

    pub(crate) fn restore_display(&mut self) {
        let restoring = Arc::new(AtomicBool::new(true));
        self.restoring = restoring.clone();

        let (display_msgs, restore_items) = history_to_display(
            self.state.session.messages(),
            self.state.session.tool_outputs(),
            &self.ui_config.tool_output_lines,
        );
        self.main_chat().load_messages(display_msgs);
        // The restored total predates any per-turn cost, so price it once with the
        // selected model. Later turns add their own exact cost.
        let (usage, context_size) = (self.state.token_usage, self.state.context_size);
        let cost = self.state.model.cost_of(&usage, self.state.fast);
        let main = self.main_chat();
        main.token_usage = usage;
        main.cost = cost;
        main.context_size = context_size;
        if let Some(draft) = self.state.session_mut().meta.input_draft.take() {
            self.input_box.set_input(draft);
            self.input_box.buffer.move_to_end();
        }

        self.fire_restore_items(restore_items);

        for sa in self.state.session_mut().take_subagents() {
            let idx = self.chats.len();
            self.chat_index.insert(sa.tool_use_id.clone(), idx);
            let mut chat = Chat::new(
                sa.name,
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            chat.set_restore_channel(self.restore_event_tx.clone());
            chat.model_id = sa.model;
            if let Some(messages) = self.state.session.subagent_messages().get(&sa.tool_use_id) {
                let (display, items) = history_to_display(
                    messages,
                    self.state.session.tool_outputs(),
                    &self.ui_config.tool_output_lines,
                );
                chat.load_messages(display);
                chat.mark_finished(DisplayRole::Done, DONE_TEXT);
                self.fire_restore_items(items);
            }
            self.chats.push(chat);
        }

        self.sync_subagents();

        let eh = &self.lua_event_handle;
        if eh.is_disconnected() {
            self.restoring
                .store(false, std::sync::atomic::Ordering::Relaxed);
        } else {
            eh.send_restore_complete(restoring);
        }
    }

    fn fire_restore_items(&self, items: Vec<maki_lua::RestoreItem>) {
        let Some(tx) = &self.restore_event_tx else {
            return;
        };
        let eh = &self.lua_event_handle;
        let theme_gen = crate::theme::generation();
        for mut item in items {
            item.theme_gen = Some(theme_gen);
            eh.request_restore(item, tx.clone());
        }
    }

    /// Resume at process start: the agent was already spawned with this
    /// history, so no respawn follows and the restored queue must be
    /// flushed here.
    pub(crate) fn restore_resumed_session(&mut self) {
        self.permissions
            .load_session_rules(stored_to_rules(&self.state.session.meta.session_rules));
        self.restore_display();
        self.flush_restored_queue();
        for w in self.state.warnings.drain(..) {
            self.status_bar.flash(w);
        }
    }

    /// The one funnel for handing a history over. When the UI installs one the
    /// agent did not give it (rewind, load, new session), the mirror handle
    /// goes away in the same breath, so no later checkpoint can bring the
    /// agent's stale copy back. Only `respawn_agent` hands a live mirror in.
    fn install_local_history(&mut self) -> LoadedSession {
        self.shared_history = None;
        LoadedSession {
            messages: self.state.session.messages().to_vec(),
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
        self.state.session = Arc::new(AppSession::new(
            &self.state.session.model,
            &self.state.session.cwd,
        ));
        self.install_local_history();
        self.fire_session_autocmd("SessionReset", serde_json::json!({}));
        vec![Action::NewSession]
    }

    pub(super) fn open_rewind_picker(&mut self) -> Vec<Action> {
        match self.rewind_picker.open(self.state.session.messages()) {
            Ok(()) => vec![],
            Err(msg) => {
                self.status_bar.flash(msg);
                vec![]
            }
        }
    }

    pub(super) fn rewind_to(&mut self, entry: RewindEntry) -> Vec<Action> {
        let session = self.state.session_mut();
        session.truncate_messages(entry.turn_index);
        session.prune_orphans(|m| m.tool_uses().map(|(id, _, _)| id.to_owned()).collect());
        session.update_title_if_default();
        self.state.context_size =
            maki_agent::agent::estimate_message_tokens(self.state.session.messages());

        self.reset_ui_chrome();
        self.restore_display();

        self.input_box.set_input(entry.prompt_text);
        self.input_box.buffer.move_to_end();

        vec![Action::LoadSession(Box::new(self.install_local_history()))]
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

        self.install_local_history()
    }

    pub(crate) fn load_session(&mut self, session_id: MakiId) -> Vec<Action> {
        let session = match AppSession::load(session_id, &self.storage) {
            Ok(s) => s,
            Err(e) => {
                self.status_bar
                    .flash(format!("Failed to load session: {e}"));
                return vec![];
            }
        };
        self.checkpoint();
        let loaded = self.apply_loaded_session(session, &self.state.model.clone());
        vec![Action::LoadSession(Box::new(loaded))]
    }
}
