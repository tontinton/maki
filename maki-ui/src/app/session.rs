use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::app::tasks::TaskOutcome;
use crate::chat::{Chat, DONE_TEXT, history_to_display};
use crate::components::rewind_picker::RewindEntry;
use crate::components::{Action, LoadedSession};
use maki_agent::agent::estimate_message_tokens;
use maki_lua::SessionEndReason;
use maki_providers::{Model, TokenUsage};
use maki_storage::id::MakiId;
use maki_storage::sessions::{SessionMeta, StoredSubagent};

use crate::AppSession;

use super::session_state::{SessionState, rules_to_stored, stored_to_rules};
use super::{App, Mode, PendingInput, PlanState, Status};

/// The shortest gap between two writes that carry only UI state.
const SOFT_SAVE_DELAY: Duration = Duration::from_millis(1000);

/// What `App::checkpoint` last handed to the writer: which session, how far
/// along it was, and when. The id is part of it because a session swapped into
/// the tab starts its revisions back at zero and would otherwise look older
/// than the stamp left by the one it replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sent {
    pub id: MakiId,
    pub revision: u64,
    pub content_revision: u64,
    pub at: Instant,
}

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
        self.checkpoint_with(SOFT_SAVE_DELAY);
    }

    /// A checkpoint for the paths that get no later frame, so a draft typed a
    /// keystroke ago still reaches disk: shutdown, and swapping the session out
    /// from under the tab.
    pub(crate) fn checkpoint_now(&mut self) {
        self.checkpoint_with(Duration::ZERO);
    }

    pub(super) fn checkpoint_with(&mut self, soft_delay: Duration) {
        let snapshot = self.shared_history.as_ref().map(|h| h.load_full());
        let meta = self.build_meta();
        AppSession::checkpoint(
            &mut self.state.session,
            snapshot.as_deref(),
            meta,
            self.state.token_usage,
        );

        if !self.has_content() {
            // A draft typed and then deleted is already on disk, and a file with
            // nothing in it is a session the picker still offers to resume. Idle
            // only: submitting empties the draft a frame before the agent mirrors
            // the prompt back, and that gap is not an abandoned session.
            let id = self.state.session.id;
            if self.status == Status::Idle && self.last_sent.take_if(|last| last.id == id).is_some()
            {
                self.storage_writer.delete(id, |_| {});
            }
            return;
        }
        let session = &self.state.session;
        let sent = Sent {
            id: session.id,
            revision: session.revision(),
            content_revision: session.content_revision(),
            at: Instant::now(),
        };
        if let Some(last) = &self.last_sent
            && last.id == sent.id
        {
            if last.revision == sent.revision {
                return;
            }
            // Only UI state moved: a keystroke in the draft, the queue or a
            // session rule. Each one costs a meta record plus an fsync, so they
            // land at most once per `soft_delay`, which bounds what a crash
            // takes with it. Anything the agent produced skips the wait.
            if last.content_revision == sent.content_revision && last.at.elapsed() < soft_delay {
                return;
            }
        }

        self.storage_writer.send(Arc::clone(&self.state.session));
        self.last_sent = Some(sent);
    }

    /// Everything the session mirrors from live state, built field by field so
    /// a new `SessionMeta` field forces a decision here. Every frame calls it,
    /// so it stays cheap: an idle UI has an empty draft, queue and rule list,
    /// and an empty `Vec` does not allocate.
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
            yolo: self.permissions.persisted_yolo(),
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
        self.clear_exit_request();
        self.queue.clear();
        self.recoverable_queue.clear();
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.status_bar.clear_flash();
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
        let cost = self.state.cost;
        let list_cost = self.state.list_cost;
        let context_size = self.state.context_size;
        let main = self.main_chat();
        main.cost = cost;
        main.list_cost = list_cost;
        main.context_size = context_size;
        if let Some(draft) = self.state.session.meta.input_draft.clone() {
            self.input_box.set_input(draft);
            self.input_box.buffer.move_to_end();
        }

        self.fire_restore_items(restore_items);

        // Read, not taken: the live chats below are the source `sync_subagents`
        // mirrors back, so emptying the session here would only make the next
        // checkpoint write the same list again.
        for sa in self.state.session.subagents().to_vec() {
            // A subagent reaches disk when it spawns but its transcript only
            // when it ends, so one without an entry here never got to finish:
            // leftovers from a kill mid-turn. It has nothing to show, and
            // restoring it would park a task no agent backs at the top of the
            // picker, running forever. `sync_subagents` below drops it for good.
            let Some(messages) = self.state.session.subagent_messages().get(&sa.tool_use_id) else {
                continue;
            };
            let (display, items) = history_to_display(
                messages,
                self.state.session.tool_outputs(),
                &self.ui_config.tool_output_lines,
            );
            self.chat_index
                .insert(sa.tool_use_id.clone(), self.chats.len());
            let mut chat = Chat::subagent(
                &sa.tool_use_id,
                sa.name,
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            chat.set_restore_channel(self.restore_event_tx.clone());
            chat.model_id = sa.model;
            chat.load_messages(display);
            // The session file keeps the transcript but never how it ended,
            // so a reload admits that instead of guessing.
            chat.mark_finished(TaskOutcome::Unknown, DONE_TEXT);
            self.fire_restore_items(items);
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

    /// The one funnel from a session's meta to the manager that enforces it,
    /// `App::new` included. A tab keeps one manager while sessions come and go
    /// under it, and a new tab forks the prototype the process started with, so
    /// whoever takes a session has to state its answer in full, rules and yolo
    /// both, or it runs on what the last one was granted. A session that stored
    /// no yolo falls back to `--yolo` and `always_yolo`.
    pub(super) fn apply_stored_permissions(&self, meta: &SessionMeta) {
        self.permissions
            .load_session_rules(stored_to_rules(&meta.session_rules));
        self.permissions.set_session_yolo(meta.yolo);
    }

    /// Resume at process start: the agent was already spawned with this
    /// history, so no respawn follows and the restored queue must be
    /// flushed here.
    pub(crate) fn restore_resumed_session(&mut self) {
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

    /// `/new` swaps a fresh session under this tab, `Ctrl-N` spawns a new tab
    /// that rebuilds its whole state from this meta, so both gestures keep
    /// exactly what is listed here. Written out field by field so a new
    /// `SessionMeta` field has to pick a side: settings that say how the user
    /// works ride along, anything a finished turn produced stays behind, or the
    /// new session writes over work it never did.
    pub(crate) fn blank_session(&self) -> AppSession {
        let mut session = AppSession::new(&self.state.model.spec(), &self.state.session.cwd);
        session.meta = SessionMeta {
            mode: Some(self.state.mode.into()),
            thinking: Some(self.state.thinking.into()),
            fast: self.state.fast,
            workflow: self.state.workflow,
            plan_path: None,
            plan_written: false,
            session_rules: Vec::new(),
            context_size: 0,
            input_draft: None,
            queued_messages: Vec::new(),
            yolo: self.permissions.persisted_yolo(),
        };
        session
    }

    pub(super) fn reset_session(&mut self) -> Vec<Action> {
        self.checkpoint_now();
        self.reset_ui_chrome();
        self.state.token_usage = TokenUsage::default();
        self.state.cost = None;
        self.state.list_cost = None;
        self.state.context_size = 0;
        self.state.plan = PlanState::None;
        if self.state.mode == Mode::Plan {
            self.enter_plan();
        }
        // Fire before the swap. A handler cleaning up after the session
        // that just ended needs its id, and the stamp always reads
        // whichever session is current.
        self.fire_session_autocmd("SessionReset", serde_json::json!({}));
        self.lua_event_handle
            .end_session(self.state.session.id, SessionEndReason::Reset);
        let session = self.blank_session();
        self.apply_stored_permissions(&session.meta);
        self.state.session = Arc::new(session);
        maki_otel::emit::session_started(
            maki_otel::emit::START_FRESH,
            Some(&self.state.session.id.to_string()),
        );
        self.install_local_history();
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
        // The live size came from the provider, so it also counts the system
        // prompt and the tool schemas, a baseline the estimator cannot see.
        // Subtract only what we drop, or the gauge collapses until the next
        // turn measures it again. An emptied history is a fresh session though,
        // baseline included.
        let baseline = self
            .state
            .context_size
            .saturating_sub(estimate_message_tokens(self.state.session.messages()));
        let session = self.state.session_mut();
        session.truncate_messages(entry.turn_index);
        session.prune_orphans(|m| m.tool_uses().map(|(id, _, _)| id.to_owned()).collect());
        session.update_title_if_default();
        let kept = estimate_message_tokens(self.state.session.messages());
        self.state.context_size = if kept == 0 { 0 } else { baseline + kept };

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
        let previous = self.state.session.id;
        self.checkpoint_now();
        self.apply_stored_permissions(&session.meta);
        self.state =
            SessionState::from_session(session, fallback_model, &self.storage, &self.model_policy);
        if previous != self.state.session.id {
            self.lua_event_handle
                .end_session(previous, SessionEndReason::Load);
        }
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
        let loaded = self.apply_loaded_session(session, &self.state.model.clone());
        vec![Action::LoadSession(Box::new(loaded))]
    }
}
