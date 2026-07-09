use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::chat::{Chat, DONE_TEXT, history_to_display};
use crate::components::DisplayRole;
use crate::components::restore_mode_picker::RestoreMode;
use crate::components::tree_selector::{
    NO_TURNS_MSG, TreeSelectorOutcome, landing_target_before, last_user_prompt_text,
    undo_target_for_tree,
};
use crate::components::{Action, LoadedSession};
use maki_agent::snapshots::{SnapshotError, SnapshotStore};
use maki_providers::{Model, TokenUsage};
use maki_storage::paths::{session_dir, snapshots_dir};
use maki_storage::session_log::load_folder;
use maki_storage::sessions::StoredSubagent;
use serde_json::value::RawValue;

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
        self.storage_writer.send(&self.state.session);
    }

    /// Snapshot the working tree at run end (§7 — completed AND cancelled
    /// runs), keyed by the closing node. The first turn also writes the
    /// session-start anchor (turn-0 snapshot, keyed to the session not a node).
    /// A missing cwd or readonly session is skipped silently; snapshot errors
    /// surface as a status flash (snapshots are opt-in, never fatal).
    pub(super) fn snapshot_at_run_end(&mut self) {
        let Some(session_id) = self.storage_writer.session_id() else {
            return;
        };
        let cwd = Path::new(&self.state.session.cwd);
        if !cwd.is_dir() {
            return;
        }
        let dir = session_dir(self.storage.path(), &session_id);
        let store = SnapshotStore::new(snapshots_dir(&dir));
        if !store.has_session_start()
            && let Err(e) = store.snapshot_session_start(cwd)
        {
            self.status_bar
                .flash(format!("Session-start snapshot failed: {e}"));
            return;
        }
        let leaf = self.storage_writer.leaf_position();
        if let Some(node) = leaf.node_ref() {
            let node_id = node.to_string();
            if let Err(e) = store.snapshot(cwd, &node_id) {
                self.status_bar
                    .flash(format!("Working-tree snapshot failed: {e}"));
            }
        }
    }

    pub(super) fn reset_ui_chrome(&mut self) {
        self.chats.clear();
        let mut main = Chat::new("Main".into(), self.ui_config);
        main.set_restore_channel(self.lua_event_handle.clone(), self.restore_event_tx.clone());
        main.set_renders(self.renders.clone());
        self.chats.push(main);
        self.active_chat = 0;
        self.chat_index.clear();
        self.status = super::Status::Idle;
        self.queue.clear();
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
            &self.state.session.messages,
            &self.state.session.tool_outputs,
            &self.ui_config.tool_output_lines,
        );
        self.main_chat().load_messages(display_msgs);
        self.main_chat().queue_restore_items(restore_items);
        self.main_chat().token_usage = self.state.token_usage;
        self.main_chat().context_size = self.state.context_size;
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
            chat.set_restore_channel(self.lua_event_handle.clone(), self.restore_event_tx.clone());
            chat.set_renders(self.renders.clone());
            chat.model_id = sa.model;
            if let Some(messages) = self.state.session.subagent_messages.get(&sa.tool_use_id) {
                let (display, items) = history_to_display(
                    messages,
                    &self.state.session.tool_outputs,
                    &self.ui_config.tool_output_lines,
                );
                chat.load_messages(display);
                chat.queue_restore_items(items);
                chat.mark_finished(DisplayRole::Done, DONE_TEXT);
            }
            self.chats.push(chat);
        }

        if let Some(eh) = &self.lua_event_handle {
            eh.send_restore_complete(restoring);
        } else {
            self.restoring
                .store(false, std::sync::atomic::Ordering::Relaxed);
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
        if let Some(ref handle) = self.lua_event_handle {
            handle.fire_autocmd("SessionReset", serde_json::json!({}));
        }
        self.state.token_usage = TokenUsage::default();
        self.state.context_size = 0;
        self.state.plan = PlanState::None;
        if self.state.mode == Mode::Plan {
            self.enter_plan();
        }
        self.state.session = AppSession::new(&self.state.session.model, &self.state.session.cwd);
        vec![Action::NewSession]
    }

    /// Open the tree selector (C3, §11). Barriers first so the on-disk log
    /// reflects every prior push, then reloads the tree and builds the view.
    pub(super) fn open_tree_selector(&mut self) -> Vec<Action> {
        self.save_session();
        if let Err(e) = self.storage_writer.barrier() {
            tracing::warn!(error = %e, "barrier before tree selector failed");
        }
        self.state.refresh_tree(&self.storage);
        let Some(tree) = &self.state.tree else {
            self.status_bar.flash(NO_TURNS_MSG.into());
            return vec![];
        };
        if tree.nodes.is_empty() {
            self.status_bar.flash(NO_TURNS_MSG.into());
            return vec![];
        }
        let dir = session_dir(self.storage.path(), &self.state.session.id);
        match load_folder(&dir, &self.state.session.id) {
            Ok(loaded) => {
                if let Err(msg) = self.tree_selector.open(&loaded) {
                    self.status_bar.flash(msg);
                }
            }
            Err(e) => {
                self.status_bar
                    .flash(format!("Failed to load session tree: {e}"));
            }
        }
        vec![]
    }

    /// Commit a rewind per the §4 landing rule (§7 restore modes). Rewinds are
    /// appends (`TreeMutation::Rewind` writes a `Leaf`); nothing is deleted —
    /// the abandoned branch is preserved on disk (decision 1). `mode` selects
    /// conversation-only (C3 leaf move), code restore (working-tree snapshot),
    /// or both. Code restore touches only the union of the two manifests' paths
    /// (§A.9); the current tree is snapshotted first so the restore is undoable.
    pub(super) fn rewind_with_mode(
        &mut self,
        outcome: Option<TreeSelectorOutcome>,
        mode: RestoreMode,
    ) -> Vec<Action> {
        let Some(outcome) = outcome else {
            return vec![];
        };
        if self.status != super::Status::Idle {
            return vec![];
        }
        let tree = self.state.tree.clone();

        if let TreeSelectorOutcome::RewindBoundary { parent, blocks } = &outcome {
            let content_raw: Vec<Box<RawValue>> = blocks
                .iter()
                .filter_map(|b| serde_json::value::to_raw_value(b).ok())
                .collect();
            self.run_id += 1;
            let new_id = match self
                .storage_writer
                .rewind_to_interrupted_sibling(parent.clone(), content_raw)
            {
                Ok(id) => id,
                Err(e) => {
                    self.status_bar.flash(format!("Rewind failed: {e}"));
                    return vec![];
                }
            };
            if mode.restores_code() {
                self.restore_code_at(new_id.as_str());
            }
            self.rebuild_after_rewind();
            self.state.session.update_title_if_default();
            self.enqueue_save();
            return vec![Action::LoadSession(Box::new(
                self.loaded_session_snapshot(),
            ))];
        }

        let Some(tree) = tree else {
            return vec![];
        };
        let target = match &outcome {
            TreeSelectorOutcome::RewindBefore { prompt_text } => {
                landing_target_before(&tree, prompt_text)
            }
            TreeSelectorOutcome::RewindOn => tree.leaf.clone(),
            TreeSelectorOutcome::RewindBoundary { .. } => unreachable!(),
        };

        self.run_id += 1;
        match self.storage_writer.rewind(target.clone()) {
            Ok(_) => {}
            Err(e) => {
                self.status_bar.flash(format!("Rewind failed: {e}"));
                return vec![];
            }
        }

        if mode.restores_code()
            && let Some(node_id) = target.node_ref()
        {
            self.restore_code_at(&node_id.to_string());
        }

        let prefill = match &outcome {
            TreeSelectorOutcome::RewindBefore { prompt_text } => Some(prompt_text.clone()),
            _ => None,
        };

        self.rebuild_after_rewind();
        if let Some(text) = prefill {
            self.input_box.set_input(text);
            self.input_box.buffer.move_to_end();
        }

        self.state.session.update_title_if_default();
        self.enqueue_save();

        vec![Action::LoadSession(Box::new(
            self.loaded_session_snapshot(),
        ))]
    }

    /// Restore the working tree to `node_id`'s snapshot (§7). Opens the
    /// snapshot store under the active session folder and restores, touching
    /// only the union of the pre-restore and target manifests' paths. The
    /// restore is undoable (current state snapshotted first, §A.9). The caller
    /// supplies the target node — the store falls back to the session-start
    /// anchor when the node has no manifest (so restore-to-root undoes
    /// everything this session did). Errors are surfaced as a status flash; a
    /// missing snapshot is silent.
    fn restore_code_at(&mut self, node_id: &str) {
        let Some(session_id) = self.storage_writer.session_id() else {
            return;
        };
        let dir = session_dir(self.storage.path(), &session_id);
        let snapshots = snapshots_dir(&dir);
        let store = SnapshotStore::new(snapshots);
        let cwd = Path::new(&self.state.session.cwd);
        if !cwd.is_dir() {
            tracing::warn!(cwd = %self.state.session.cwd, "restore: cwd missing, skipping");
            return;
        }
        match store.restore(cwd, &[node_id]) {
            Ok(_) => {}
            Err(SnapshotError::NotFound(_)) => {
                self.status_bar.flash("No code snapshot to restore".into());
            }
            Err(e) => {
                self.status_bar.flash(format!("Code restore failed: {e}"));
            }
        }
    }

    /// Fast path (§11): single-Esc-then-Enter. Lands before the last user
    /// prompt on the active branch (same as selecting it in the tree
    /// selector). No-op if there is no user prompt to rewind to.
    pub(super) fn rewind_to_last_user_message(&mut self) -> Vec<Action> {
        if self.status != super::Status::Idle {
            return vec![];
        }
        self.state.refresh_tree(&self.storage);
        let Some(tree) = self.state.tree.clone() else {
            self.status_bar.flash(NO_TURNS_MSG.into());
            return vec![];
        };
        let Some(prompt_text) = last_user_prompt_text(&tree) else {
            self.status_bar.flash(NO_TURNS_MSG.into());
            return vec![];
        };
        self.rewind_with_mode(
            Some(TreeSelectorOutcome::RewindBefore { prompt_text }),
            RestoreMode::Conversation,
        )
    }

    /// Undo-of-rewind (§4): `position_before_last_leaf`, offered only while the
    /// most recent record is itself a `Leaf`. Commits by appending another
    /// `Leaf` (undo is a rewind to the pre-tip position).
    pub(super) fn undo_rewind(&mut self) -> Vec<Action> {
        if self.status != super::Status::Idle {
            return vec![];
        }
        self.state.refresh_tree(&self.storage);
        let Some(target) = self.state.tree.as_ref().and_then(undo_target_for_tree) else {
            self.status_bar.flash("Nothing to undo".into());
            return vec![];
        };

        self.run_id += 1;
        if let Err(e) = self.storage_writer.rewind(target) {
            self.status_bar.flash(format!("Undo failed: {e}"));
            return vec![];
        }
        self.rebuild_after_rewind();
        self.enqueue_save();
        vec![Action::LoadSession(Box::new(
            self.loaded_session_snapshot(),
        ))]
    }

    /// Rebuild chat + tree from the post-rewind active branch. The folded
    /// `ValidContext` is the active branch (§2); `messages` becomes it so the
    /// chat renders the post-rewind conversation. Non-destructive: the on-disk
    /// abandoned branch is untouched (decision 1).
    fn rebuild_after_rewind(&mut self) {
        self.state.refresh_tree(&self.storage);
        if let Some(tree) = &self.state.tree {
            let ctx = maki_agent::History::fold_tree(tree);
            self.state.session.messages = ctx.to_vec();
            self.storage_writer
                .reset_msg_cursor(self.state.session.messages.len());
        }
        self.reset_ui_chrome();
        self.restore_display();
        self.shared_history = None;
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
