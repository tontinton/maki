use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::app::tasks::TaskOutcome;
use crate::chat::{Chat, DONE_TEXT, history_to_display};
use crate::components::Action;
use crate::components::command::PROMPT_NEEDS;
use crate::components::input::{COMMAND_PREFIX, sent_text};
use crate::components::rewind_picker::RewindEntry;
use crate::text_buffer::Writers;
use maki_agent::Transcript;
use maki_agent::agent::SessionExport;
use maki_agent::session::SessionHeader;
use maki_lua::agent_autocmd::HostEvent;
use maki_lua::{PluginPermissions, PluginReach, SessionEndReason};
use maki_providers::{Message, Model, RequestOptions, TokenUsage, estimate_message_tokens};
use maki_storage::id::MakiId;
use maki_storage::sessions::{
    SessionClaim, SessionMeta, StoredMode, StoredSubagent, StoredWriters,
};

use crate::{AppSession, OpenSession};

use super::session_state::{SessionState, rules_to_stored, stored_to_rules};
use super::{App, MAIN_CHAT, Mode, PendingInput, PlanState, Sending, Status};

/// The shortest gap between two writes that carry only UI state.
const SOFT_SAVE_DELAY: Duration = Duration::from_millis(1000);

pub(super) const FORK_TITLE_PREFIX: &str = "fork of ";
pub(super) const FORK_EMPTY_MSG: &str = "Nothing to fork, the copy would be empty";
const FORK_FAILED_MSG: &str = "Fork failed";

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

/// Every user turn was sent by the user or by a plugin that could send a
/// prompt, and we do not keep which. So a turn that Enter would run rather
/// than send comes back as written by a nameless plugin holding only what a
/// prompt takes, see [`PROMPT_NEEDS`].
fn turn_writers(text: &str) -> Option<StoredWriters> {
    let sent = sent_text(text);
    let runs = sent.starts_with(COMMAND_PREFIX) || Sending::of(sent, false).runs();
    runs.then_some(StoredWriters::Agent)
}

/// Grants held by fewer than all of {writers} cannot be vouched for once
/// their names are gone, so only what a prompt takes is kept, and only when
/// each of them held it.
fn stored_writers(writers: &Writers) -> Option<StoredWriters> {
    if writers.is_empty() {
        return None;
    }
    let each_sends_prompts = writers
        .iter()
        .all(|writer| PROMPT_NEEDS.iter().all(|&perm| writer.is_allowed(perm)));
    Some(if each_sends_prompts {
        StoredWriters::Agent
    } else {
        StoredWriters::Unknown
    })
}

fn unknown_writer(writers: StoredWriters) -> PluginReach {
    let mut permissions = PluginPermissions::denied();
    for &perm in PROMPT_NEEDS {
        permissions.set(perm, writers == StoredWriters::Agent);
    }
    PluginReach::new(permissions)
}

/// The one content check. `App::checkpoint` saves a session only when this
/// holds, and the shutdown report asks the same question, so the report and the
/// disk never disagree. A draft, a queue or plan mode used to count too, and
/// every tab that had one showed up in the picker as another empty session.
pub(crate) fn session_has_content(session: &AppSession) -> bool {
    !session.messages().is_empty()
}

/// Cuts the transcript right before `turn_index` and returns what is left of
/// `context_size`. Rewind and fork both go through here so they never disagree
/// about what a cut leaves behind.
fn truncate_to_turn(session: &mut AppSession, turn_index: usize, context_size: u32) -> u32 {
    // The live size came from the provider, so it also counts the system
    // prompt and the tool schemas, a baseline the estimator cannot see.
    // Subtract only what we drop, or the gauge collapses until the next
    // turn measures it again. An emptied history is a fresh session though,
    // baseline included.
    let baseline = context_size.saturating_sub(estimate_message_tokens(session.messages()));
    session.truncate_messages(turn_index);
    session.prune_orphans(|m| m.tool_uses().map(|(id, _, _)| id.to_owned()).collect());
    let kept = estimate_message_tokens(session.messages());
    if kept == 0 { 0 } else { baseline + kept }
}

impl App {
    /// Stricter than `session_has_content`. A draft or plan mode is not worth a
    /// file, but it is still worth keeping when the picker wants this tab for
    /// another session.
    pub(crate) fn is_blank(&self) -> bool {
        let session = &self.state.session;
        let meta = &session.meta;
        !session_has_content(session)
            && meta.input_draft.is_none()
            && meta.queued_messages.is_empty()
            && meta.mode == Some(StoredMode::Build)
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

        if !session_has_content(&self.state.session) {
            // A rewind to the first prompt empties a session already on disk,
            // which the picker would otherwise still offer to resume.
            let id = self.state.session.id;
            if self.last_sent.take_if(|last| last.id == id).is_some() {
                self.storage_writer
                    .delete(id, Some(self.state.claim.clone()), |_| {});
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

        self.storage_writer
            .send(Arc::clone(&self.state.session), self.state.claim.clone());
        self.last_sent = Some(sent);
    }

    /// Everything the session mirrors from live state, built field by field so
    /// a new `SessionMeta` field forces a decision here. Every frame calls it,
    /// so it stays cheap: an idle UI has an empty draft, queue and rule list,
    /// and an empty `Vec` does not allocate.
    fn build_meta(&self) -> SessionMeta {
        let state = &self.state;
        let (draft, writers) = self.main_draft();
        let has_draft = !draft.is_empty();
        SessionMeta {
            mode: Some(state.mode.into()),
            plan_path: state.plan.path().map(|p| p.to_string_lossy().into_owned()),
            plan_written: state.plan.is_ready(),
            session_rules: rules_to_stored(&self.permissions.session_rules_snapshot()),
            context_size: state.context_size,
            input_draft_writers: stored_writers(writers).filter(|_| has_draft),
            input_draft: has_draft.then_some(draft),
            queued_messages: if self.recoverable_queue.is_empty() {
                self.queue.text_messages()
            } else {
                self.recoverable_queue.clone()
            },
            thinking: Some(state.thinking_intent().into()),
            fast: state.fast_intent(),
            workflow: state.workflow,
            yolo: self.permissions.persisted_yolo(),
        }
    }

    /// Mirrors every subagent tab into the session meta. Walks `chats`, not
    /// `chat_index`: that one is a per-turn routing cache, and a finished
    /// task dropped from it must still come back on reload.
    pub(super) fn sync_subagents(&mut self) {
        let subagents = self
            .chats
            .iter()
            .filter_map(|chat| {
                Some(StoredSubagent {
                    tool_use_id: chat.task_id()?.to_string(),
                    name: chat.name.clone(),
                    model: chat.model_id.clone(),
                    thinking: chat.opts.map(|o| o.thinking.into()),
                    fast: chat.opts.is_some_and(|o| o.fast),
                    plugin: chat.plugin.clone(),
                    cost: chat.cost,
                })
            })
            .collect();
        self.state.session_mut().set_subagents(subagents);
    }

    pub(super) fn save_input_history(&self) {
        if let Err(e) = self.input_box.history().save() {
            tracing::warn!(error = %e, "input history save failed");
        }
    }

    /// Call only once `state.session` is final: the chats it builds are
    /// stamped with that session for life. A plugin task of that same session
    /// stays if it is still running: a rewind rebuilds the main chat, not the
    /// monitor a plugin runs beside it, and ending it here would leave a task
    /// shown as done that keeps streaming and that Esc can no longer reach.
    pub(super) fn reset_ui_chrome(&mut self) {
        // Brings the main draft back to the box before the chats holding it go.
        self.set_active_chat(MAIN_CHAT);
        let session = self.state.session.id;
        self.chats
            .retain(|chat| chat.is_live_plugin_task_of(session));
        let mut main = Chat::new(
            self.state.session.id,
            None,
            "Main".into(),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        main.set_restore_channel(self.restore_event_tx.clone());
        self.chats.insert(MAIN_CHAT, main);
        self.active_chat = MAIN_CHAT;
        self.chat_index.clear();
        self.deleted_tasks.clear();
        self.status = Status::Idle;
        self.clear_exit_request();
        self.queue.clear();
        self.recoverable_queue.clear();
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.status_bar.clear_flash();
        self.last_esc = None;
        self.restoring = Arc::new(AtomicBool::new(false));
        self.plan_form.reset();
        // The draft this chrome belonged to is gone, and the Lua thread still
        // owes answers about it. Retiring them here is what keeps a handler
        // that comes back after a `/new`, a rewind or a tab switch from
        // implementing a plan in a session the user never picked in.
        self.plan_answers.abandon();
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
        let context_size = self.state.context_size;
        let main = self.main_chat();
        main.cost = cost;
        main.context_size = context_size;
        let meta = &self.state.session.meta;
        if let Some(draft) = meta.input_draft.clone() {
            let writers = meta.input_draft_writers;
            self.restore_input(&draft, writers);
        }

        self.chats[MAIN_CHAT].request_restores(restore_items);

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
            if self.chat_of_task(&sa.tool_use_id).is_ok() {
                continue;
            }
            let (display, items) = history_to_display(
                messages,
                self.state.session.tool_outputs(),
                &self.ui_config.tool_output_lines,
            );
            // Plugin tasks are never routed through `chat_index`.
            if sa.plugin.is_none() {
                self.chat_index
                    .insert(sa.tool_use_id.clone(), self.chats.len());
            }
            let mut chat = Chat::new(
                self.state.session.id,
                Some(&sa.tool_use_id),
                sa.name,
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            chat.set_restore_channel(self.restore_event_tx.clone());
            chat.plugin = sa.plugin;
            chat.cost = sa.cost;
            chat.model_id = sa.model;
            chat.opts = sa.thinking.map(|thinking| RequestOptions {
                thinking: thinking.into(),
                fast: sa.fast,
            });
            chat.load_messages(display);
            // The session file keeps the transcript but never how it ended,
            // so a reload admits that instead of guessing.
            chat.mark_finished(TaskOutcome::Unknown, DONE_TEXT);
            chat.request_restores(items);
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
    fn install_local_history(&mut self) -> Vec<Message> {
        self.shared_history = None;
        self.state.session.messages().to_vec()
    }

    /// `/new` swaps a fresh session under this tab, `Ctrl-N` spawns a new tab
    /// that rebuilds its whole state from this meta, so both gestures keep
    /// exactly what is listed here. Written out field by field so a new
    /// `SessionMeta` field has to pick a side: settings that say how the user
    /// works ride along, anything a finished turn produced stays behind, or the
    /// new session writes over work it never did.
    pub(crate) fn blank_session(&self) -> OpenSession {
        let mut open = OpenSession::fresh(
            &self.state.model.spec(),
            &self.state.session.cwd,
            &self.storage,
        );
        open.session.meta = SessionMeta {
            mode: Some(self.state.mode.into()),
            thinking: Some(self.state.thinking_intent().into()),
            fast: self.state.fast_intent(),
            workflow: self.state.workflow,
            plan_path: None,
            plan_written: false,
            session_rules: Vec::new(),
            context_size: 0,
            input_draft: None,
            input_draft_writers: None,
            queued_messages: Vec::new(),
            yolo: self.permissions.persisted_yolo(),
        };
        open
    }

    /// `maki.session.new` lays its document over [`Self::blank_session`], so
    /// a plugin's session starts where `Ctrl-N` would.
    pub(crate) fn session_from(&self, transcript: Transcript) -> Result<OpenSession, String> {
        let OpenSession { session, claim } = self.blank_session();
        let session = transcript
            .into_session(session)
            .map_err(|e| e.to_string())?;
        Ok(OpenSession { session, claim })
    }

    /// Settings come from live state rather than the session, because a
    /// plugin can change one and export in the same drain, before the next
    /// checkpoint syncs them. The agent's mirror runs ahead of what the tab
    /// last saved and is shared, not copied. A tab whose agent never started
    /// shares the session's own.
    pub(crate) fn export(&self) -> SessionExport {
        let state = &self.state;
        let session = &state.session;
        let header = SessionHeader {
            mode: Some(state.mode.into()),
            thinking: Some(state.thinking_intent().into()),
            fast: state.fast_intent(),
            usage: state.token_usage,
            context_size: state.context_size,
            ..SessionHeader::of(session)
        };
        match &self.shared_history {
            Some(mirror) => SessionExport::live(header, &mirror.load()),
            None => SessionExport {
                header,
                messages: Arc::clone(session.shared_messages()),
                frame: session.frame().cloned(),
            },
        }
    }

    pub(super) fn reset_session(&mut self) -> Vec<Action> {
        self.checkpoint_now();
        self.state.token_usage = TokenUsage::default();
        self.state.cost = None;
        self.state.context_size = 0;
        self.state.plan = PlanState::None;
        if self.state.mode == Mode::Plan {
            self.enter_plan();
        }
        // Fire before the swap. A handler cleaning up after the session
        // that just ended needs its id, and the stamp always reads
        // whichever session is current.
        self.fire_session_event(HostEvent::SessionReset {});
        self.lua_event_handle
            .end_session(self.state.session.id, SessionEndReason::Reset);
        // Swapping the claim is what gives the old session back: the snapshot
        // `checkpoint_now` just queued holds its own clone until it lands.
        let OpenSession { session, claim } = self.blank_session();
        self.apply_stored_permissions(&session.meta);
        self.state.session = Arc::new(session);
        self.state.claim = claim;
        self.reset_ui_chrome();
        maki_otel::emit::session_started(
            maki_otel::emit::START_FRESH,
            Some(&self.state.session.id.to_string()),
        );
        vec![Action::RestartAgent(self.install_local_history())]
    }

    /// `/fork` copies the session under a fresh id. The event loop opens the
    /// copy as a session of its own and focuses it, so this one keeps running
    /// in the background, untouched.
    pub(super) fn fork_session(&mut self) -> Vec<Action> {
        self.fork_session_with(|_| {})
    }

    /// The rewind picker's fork. The copy stops right before the chosen prompt
    /// and keeps that prompt as its draft, so it feels like a rewind that left
    /// the original alone.
    pub(super) fn fork_to(&mut self, entry: RewindEntry) -> Vec<Action> {
        self.fork_session_with(|forked| {
            // The agent restores its gauge from this, and the full size on a
            // short copy would auto compact it on its first prompt.
            let context_size = forked.meta.context_size;
            forked.meta.context_size = truncate_to_turn(forked, entry.turn_index, context_size);
            forked.meta.input_draft_writers = turn_writers(&entry.prompt_text);
            forked.meta.input_draft = Some(entry.prompt_text);
        })
    }

    fn fork_session_with(&mut self, adjust: impl FnOnce(&mut AppSession)) -> Vec<Action> {
        // The last frame's snapshot still has `/fork` in the input box, and it
        // can miss what the agent wrote since, so take a fresh one.
        self.checkpoint_now();
        let mut copy = AppSession::clone(&self.state.session);
        adjust(&mut copy);
        if !session_has_content(&copy) {
            self.status_bar.flash(FORK_EMPTY_MSG.into());
            return vec![];
        }
        let claim = SessionClaim::fresh(&self.storage);
        let mut session = match copy.fork(claim.id(), &self.state.session.cwd, &self.storage) {
            Ok(session) => session,
            Err(e) => {
                self.status_bar.flash(format!("{FORK_FAILED_MSG}: {e}"));
                return vec![];
            }
        };
        // Without this a young session forks into "fork of New session".
        session.update_title_if_default();
        session.set_title(format!("{FORK_TITLE_PREFIX}{}", session.title));
        vec![Action::OpenFork(Box::new(OpenSession { session, claim }))]
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
        let measured = self.state.context_size;
        let session = self.state.session_mut();
        let context_size = truncate_to_turn(session, entry.turn_index, measured);
        session.update_title_if_default();
        self.state.context_size = context_size;

        self.reset_ui_chrome();
        self.restore_display();

        self.restore_input(&entry.prompt_text, turn_writers(&entry.prompt_text));

        vec![Action::RestartAgent(self.install_local_history())]
    }

    /// Puts back a draft from disk or a rewound turn. Its {writers} come back
    /// as one nameless plugin holding what was kept of their grants, see
    /// [`turn_writers`] for a turn.
    fn restore_input(&mut self, text: &str, writers: Option<StoredWriters>) {
        self.input_box.set_input(text);
        if let Some(writers) = writers {
            self.input_box.buffer.add_writer(&unknown_writer(writers));
        }
        self.input_box.buffer.move_to_end();
    }

    /// The event loop owns the policy and the providers, so it resolves the
    /// model and this only adopts it.
    pub(crate) fn apply_loaded_session(
        &mut self,
        open: OpenSession,
        model: &Model,
    ) -> Vec<Message> {
        let previous = self.state.session.id;
        self.checkpoint_now();
        self.apply_stored_permissions(&open.session.meta);
        self.state = SessionState::from_session(open, model, &self.storage);
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
}

#[cfg(test)]
mod tests {
    use crate::app::tests::test_app;
    use crate::app::{App, FAST_OFF_MSG, FAST_PENDING_MSG};
    use crate::components::command::ParsedCommand;
    use maki_providers::model::FastSupport;
    use test_case::test_case;

    fn pending_app() -> App {
        let mut app = test_app();
        app.state.model.supports_fast_override = Some(FastSupport::Pending);
        app
    }

    /// A `/fast` typed before the model list lands has to outlive the snapshot
    /// a new session inherits, otherwise the answer arrives and the wish is
    /// already gone.
    #[test_case(false ; "kept")]
    #[test_case(true ; "cancelled")]
    fn pending_fast_survives_snapshot_until_discovery_answers(cancel: bool) {
        let mut app = pending_app();
        app.set_fast(true).unwrap();
        if cancel {
            app.set_fast(false).unwrap();
        }
        assert!(!app.state.fast);
        assert_eq!(app.state.pending_fast, !cancel);
        assert_eq!(app.build_meta().fast, !cancel);
        assert_eq!(app.blank_session().session.meta.fast, !cancel);

        let mut model = app.state.model.clone();
        model.supports_fast_override = Some(FastSupport::Supported);
        app.state.update_model(&model);
        assert_eq!(app.state.fast, !cancel);
        assert!(!app.state.pending_fast);
        assert_eq!(app.build_meta().fast, !cancel);
    }

    #[test]
    fn fast_command_flashes_pending_while_discovery_runs() {
        let mut app = pending_app();
        for expected in [FAST_PENDING_MSG, FAST_OFF_MSG] {
            app.execute_command(
                ParsedCommand {
                    name: "/fast".into(),
                    args: String::new(),
                    bang: false,
                },
                0,
            );
            assert_eq!(app.status_bar.flash_text(), Some(expected));
            assert!(!app.state.fast);
        }
        assert!(!app.state.pending_fast);
    }
}
