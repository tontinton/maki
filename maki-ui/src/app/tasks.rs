//! A task is a subagent chat, addressed by its `tool_use_id`. The main chat
//! goes by [`MAIN_TASK_ID`] and carries no status, since its work is the
//! session's own and `maki.session.live()` already reports that.
//!
//! Both `maki.task.list()` and the `TaskStatusChanged` autocmd serialize the
//! types below, so the two can never spell a status differently.

use std::mem;
use std::sync::Arc;
use std::time::Instant;

use maki_agent::tools::{MAIN_TASK_ID, ToolRegistry, ToolSource};
use maki_agent::{AgentEvent, ToolDoneEvent};
use maki_lua::MoveOutcome;
use maki_storage::id::MakiId;
use serde::Serialize;
use strum::IntoStaticStr;

use crate::app::{App, MAIN_CHAT};
use crate::chat::{CANCELLED_TEXT, Chat, DONE_TEXT, ERROR_TEXT};
use crate::components::{Action, BackgroundBadge, BackgroundOutcome, DisplayRole, ToolStatus};
use crate::repaint::Dirty;

const UNKNOWN_TASK_ERR: &str = "unknown task: ";
const MAIN_DELETE_ERR: &str = "cannot delete the main chat";
const RUNNING_DELETE_ERR: &str = "task is still running: ";
const MAIN_STOP_ERR: &str = "cannot stop the main chat";
const NOT_RUNNING_ERR: &str = "task is not running: ";
const SESSION_CHANGED_ERR: &str = "task session is no longer focused: ";
const MOVED_ONE_MSG: &str = "Moved 1 call to the background";
const MOVED_PREFIX: &str = "Moved ";
const MOVED_MANY_SUFFIX: &str = " calls to the background";
pub(crate) const NOTHING_TO_MOVE_MSG: &str = "Nothing here can run in the background";
const AWAITING_PERMISSION_SUFFIX: &str = " is waiting for your permission answer";

/// How a chat ended, from the vaguest to the most specific. `SubagentHistory`
/// only sees the transcript close, and the `ToolDone` carrying `is_error`
/// lands after it, so [`Self::Unknown`] holds the place until then.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TaskOutcome {
    /// Ended, and nobody said how. Reads as done unless a verdict follows.
    Unknown,
    Done,
    Error,
}

impl TaskOutcome {
    /// Only the placeholder gives way, so a late event cannot walk a finished
    /// task back to another ending.
    pub(crate) fn refines(self, previous: Self) -> bool {
        previous == Self::Unknown && self != Self::Unknown
    }

    /// The bubble that ends the transcript. Derived from the outcome rather
    /// than passed beside it, so a green marker cannot sit on a failed task.
    pub(crate) fn role(self) -> DisplayRole {
        match self {
            Self::Error => DisplayRole::Error,
            Self::Unknown | Self::Done => DisplayRole::Done,
        }
    }
}

/// `Idle` is a plugin task alive but quiet between prompts: neither a spinner
/// nor "done" would be honest.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, IntoStaticStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub(crate) enum TaskStatus {
    Working,
    Idle,
    Done,
    Error,
}

impl From<TaskOutcome> for TaskStatus {
    fn from(outcome: TaskOutcome) -> Self {
        match outcome {
            TaskOutcome::Error => Self::Error,
            TaskOutcome::Unknown | TaskOutcome::Done => Self::Done,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TaskState<'a> {
    pub(crate) id: &'a Arc<str>,
    pub(crate) name: &'a str,
    pub(crate) status: TaskStatus,
    pub(crate) plugin: Option<&'a Arc<str>>,
}

/// The wire shape of `maki.task.list()`. The main chat leaves `status` unset,
/// a tool task `plugin`.
#[derive(Serialize)]
pub(crate) struct TaskInfo {
    id: Arc<str>,
    session: MakiId,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<TaskStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin: Option<Arc<str>>,
    focused: bool,
}

/// The answers to the last Ctrl+B, or click on a call's hint, folded as they
/// come in. One flash sums them up once the last one is in.
#[derive(Default)]
pub(crate) struct BackgroundMoves {
    pub(super) pending: Vec<flume::Receiver<MoveOutcome>>,
    moved: usize,
    declined: Option<String>,
}

impl BackgroundMoves {
    fn record(&mut self, outcome: MoveOutcome) {
        match outcome {
            MoveOutcome::Moved => self.moved += 1,
            MoveOutcome::NotMovable(reason) => {
                self.declined.get_or_insert(reason);
            }
            MoveOutcome::NoHook => {}
        }
    }

    /// The flash, once every request has answered. A host that went away
    /// moved nothing.
    fn poll(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let mut answered = Vec::new();
        self.pending.retain(|reply| match reply.try_recv() {
            Ok(outcome) => {
                answered.push(outcome);
                false
            }
            Err(flume::TryRecvError::Empty) => true,
            Err(flume::TryRecvError::Disconnected) => false,
        });
        for outcome in answered {
            self.record(outcome);
        }
        self.pending.is_empty().then(|| self.summary())
    }

    fn summary(&mut self) -> String {
        let declined = self.declined.take();
        match mem::take(&mut self.moved) {
            0 => declined.unwrap_or_else(|| NOTHING_TO_MOVE_MSG.to_owned()),
            1 => MOVED_ONE_MSG.to_owned(),
            moved => format!("{MOVED_PREFIX}{moved}{MOVED_MANY_SUFFIX}"),
        }
    }
}

/// {one} for a single task, {count} then {many_suffix} for more.
pub(crate) fn count_phrase(count: usize, one: &str, many_suffix: &str) -> String {
    if count == 1 {
        one.to_owned()
    } else {
        format!("{count}{many_suffix}")
    }
}

/// The plugin that registered {tool}, or the tool itself for one no plugin
/// owns, so the task still has an owner to show.
fn tool_owner(tool: &Arc<str>) -> Arc<str> {
    match ToolRegistry::global().get(tool).map(|entry| entry.source) {
        Some(ToolSource::Lua { plugin }) => plugin,
        _ => Arc::clone(tool),
    }
}

impl App {
    /// Asks the main chat's running calls to move, {only} one of them when
    /// set. The main chat is the only mailbox a result can come back to, so
    /// this never looks at the chat in front. A call parked on a permission
    /// prompt has not started its work, so it is not asked.
    /// Whether any running call was asked to move, so a key handler can try
    /// something else when nothing could.
    pub(super) fn move_to_background(&mut self, only: Option<&str>) -> bool {
        let mut asked = false;
        for call in self.chats[MAIN_CHAT]
            .running_calls()
            .iter()
            .filter(|call| only.is_none_or(|id| id == call.id))
        {
            asked = true;
            if call.awaiting_permission {
                self.background_moves
                    .record(MoveOutcome::NotMovable(format!(
                        "{}{AWAITING_PERMISSION_SUFFIX}",
                        call.tool
                    )));
            } else {
                self.background_moves
                    .pending
                    .push(self.lua_event_handle.request_background(call.id.clone()));
            }
        }
        if asked && self.background_moves.pending.is_empty() {
            let summary = self.background_moves.summary();
            self.flash(summary);
        }
        asked
    }

    pub(super) fn tick_background_moves(&mut self) -> Dirty {
        let Some(summary) = self.background_moves.poll() else {
            return Dirty::NO;
        };
        self.flash(summary);
        Dirty::YES
    }

    /// A moved call keeps streaming into the buf its row was drawing, so its
    /// task tab starts as a copy of that row holding the same buf: nothing is
    /// replayed and nothing is lost. Runs before the main row takes the
    /// receipt, which retires the buf there. A call that already has a tab, a
    /// subagent, keeps it and the tab is adopted.
    pub(super) fn open_background_call(&mut self, done: &ToolDoneEvent) {
        let badge = BackgroundBadge {
            id: done.id.clone(),
            started_at: Some(Instant::now()),
            outcome: None,
        };
        if let Ok(idx) = self.chat_of_task(&done.id) {
            self.adopt_subagent(idx, &tool_owner(&done.tool));
        } else if let Some(mut row) = self.chats[MAIN_CHAT].tool_message(&done.id) {
            row.movable = false;
            // Esc may have failed the row just before a racing move landed.
            if let DisplayRole::Tool(tool) = &mut row.role {
                tool.status = ToolStatus::InProgress;
            }
            let name = row
                .text
                .lines()
                .next()
                .filter(|line| !line.is_empty())
                .unwrap_or(&*done.tool)
                .to_owned();
            let buf = self.chats[MAIN_CHAT].live_buf(&done.id);
            let mut chat = Chat::new(
                self.state.session.id,
                Some(&done.id),
                name,
                self.ui_config.clone(),
                self.lua_event_handle.clone(),
            );
            chat.set_restore_channel(self.restore_event_tx.clone());
            chat.plugin = Some(tool_owner(&done.tool));
            chat.load_messages(vec![row]);
            if let Some(buf) = buf {
                chat.register_live_buf(done.id.clone(), buf);
            }
            self.chats.push(chat);
        }
        self.chats[MAIN_CHAT].set_tool_background(&done.id, badge);
    }

    /// The final result of a moved call, which ends its task. A tab the user
    /// already stopped reads as cancelled, whatever the call reported: only a
    /// stop ends a task off the turn with an error before its result. The
    /// close of an adopted subagent's session may come first, and knows no
    /// verdict.
    pub(super) fn finish_background_call(&mut self, idx: usize, event: AgentEvent) {
        let AgentEvent::ToolDone(done) = &event else {
            return;
        };
        let id = done.id.clone();
        let chat = &mut self.chats[idx];
        let (outcome, ending, text) = if chat.task_status() == TaskStatus::Error {
            (
                BackgroundOutcome::Cancelled,
                TaskOutcome::Error,
                CANCELLED_TEXT,
            )
        } else if done.is_error {
            (BackgroundOutcome::Error, TaskOutcome::Error, ERROR_TEXT)
        } else {
            (BackgroundOutcome::Done, TaskOutcome::Done, DONE_TEXT)
        };
        chat.handle_event(event, None);
        chat.mark_finished(ending, text);
        self.chats[MAIN_CHAT].set_background_outcome(&id, outcome);
    }

    /// A subagent whose call moved on becomes a task of {plugin}. Off the
    /// turn's `chat_index`, neither the turn's end nor Esc in the main chat
    /// sweeps it, and it keeps working. A chat that has an owner keeps it, so
    /// the receipt and the subagent's first plugin event adopt in either
    /// order.
    pub(super) fn adopt_subagent(&mut self, idx: usize, plugin: &Arc<str>) {
        let chat = &mut self.chats[idx];
        if idx == MAIN_CHAT || chat.plugin.is_some() {
            return;
        }
        chat.plugin = Some(Arc::clone(plugin));
        if let Some(id) = chat.task_id() {
            self.chat_index.remove(&**id);
        }
    }

    /// The plugin a task answers to, for an event that came the run's way.
    /// A subagent still on the turn is in `chat_index`, which spares the walk.
    pub(super) fn task_plugin(&self, id: &str) -> Option<Arc<str>> {
        if self.chat_index.contains_key(id) {
            return None;
        }
        let idx = self.chat_of_task(id).ok()?;
        self.chats[idx].plugin.clone()
    }

    /// What `/tasks` counts in its hint: the tasks quitting or a rewind would
    /// end, as opposed to the tool tasks a turn owns.
    pub(crate) fn running_background_tasks(&self) -> impl Iterator<Item = &Chat> {
        self.chats
            .iter()
            .filter(|chat| chat.plugin.is_some() && chat.task_status() == TaskStatus::Working)
    }

    /// Subagent chats only, in chat order.
    pub(crate) fn task_states(&self) -> impl Iterator<Item = TaskState<'_>> {
        self.chats.iter().filter_map(|chat| {
            Some(TaskState {
                id: chat.task_id()?,
                name: &chat.name,
                status: chat.task_status(),
                plugin: chat.plugin.as_ref(),
            })
        })
    }

    /// The same walk widened to every chat, so the main chat at index 0 lands
    /// first on its own and no extra code path has to place it.
    pub(crate) fn tasks(&self) -> Vec<TaskInfo> {
        self.chats
            .iter()
            .enumerate()
            .map(|(idx, chat)| TaskInfo {
                id: chat.task_id_or_main(),
                session: self.state.session.id,
                name: chat.name.clone(),
                status: chat.task_id().map(|_| chat.task_status()),
                plugin: chat.plugin.clone(),
                focused: idx == self.active_chat,
            })
            .collect()
    }

    /// The task whose transcript is on screen, in the ids `maki.task` uses.
    pub(crate) fn active_task_id(&self) -> Arc<str> {
        self.chats[self.active_chat].task_id_or_main()
    }

    pub(crate) fn focus_task(&mut self, id: &str) -> Result<(), String> {
        let idx = self.chat_of_task(id)?;
        self.set_active_chat(idx);
        Ok(())
    }

    /// A tool task can be stopped only while its turn still routes to it. A
    /// plugin task can be stopped any time it is alive, idle included.
    pub(crate) fn stop_task(&mut self, id: &str) -> Result<Action, String> {
        let idx = self.chat_of_task(id)?;
        if idx == 0 {
            return Err(MAIN_STOP_ERR.to_owned());
        }
        let chat = &mut self.chats[idx];
        let live = !chat.is_finished();
        let action = match chat.plugin {
            Some(_) if live => Action::CancelPluginTask { id: id.to_owned() },
            None if live && self.chat_index.get(id) == Some(&idx) => Action::CancelSubagent {
                tool_use_id: id.to_owned(),
            },
            _ => return Err(format!("{NOT_RUNNING_ERR}{id}")),
        };
        chat.flush();
        chat.cancel_in_progress();
        chat.mark_finished(TaskOutcome::Error, CANCELLED_TEXT);
        self.permission_prompt.drop_subagent(id);
        Ok(action)
    }

    /// The picker sends the session its row came from. By the time the stop
    /// lands the user may have switched to another session, where the same
    /// task id can name a different task.
    pub(crate) fn stop_task_in_session(
        &mut self,
        id: &str,
        session: &str,
    ) -> Result<Action, String> {
        if self.state.session.id.to_string() != session {
            return Err(format!("{SESSION_CHANGED_ERR}{session}"));
        }
        self.stop_task(id)
    }

    /// Drops a finished subagent chat and its stored transcript, so a reload
    /// cannot bring it back. A running task is refused, since its agent would
    /// keep writing into a chat that is gone.
    pub(crate) fn remove_task(&mut self, id: &str) -> Result<(), String> {
        let pos = self.chat_of_task(id)?;
        if pos == 0 {
            return Err(MAIN_DELETE_ERR.to_owned());
        }
        if !self.chats[pos].is_finished() {
            return Err(format!("{RUNNING_DELETE_ERR}{id}"));
        }

        // Step off through `set_active_chat`, or the box keeps the deleted
        // chat's draft and the draft of the chat we land on stays parked.
        if self.active_chat == pos {
            self.set_active_chat(pos - 1);
        }
        self.chats.remove(pos);
        if self.active_chat > pos {
            self.active_chat -= 1;
        }
        self.chat_index.retain(|_, idx| {
            let keep = *idx != pos;
            if *idx > pos {
                *idx -= 1;
            }
            keep
        });
        self.state.session_mut().remove_subagent(id, |m| {
            m.tool_uses()
                .map(|(tool_id, _, _)| tool_id.to_owned())
                .collect()
        });
        self.deleted_tasks.insert(id.to_owned());
        Ok(())
    }

    /// Tasks are looked up by id, never by position and never through
    /// `chat_index`, a routing cache wiped at the end of every turn and left
    /// by an adopted subagent.
    pub(super) fn chat_of_task(&self, id: &str) -> Result<usize, String> {
        if id == MAIN_TASK_ID {
            return Ok(0);
        }
        self.chats
            .iter()
            .position(|chat| chat.task_id().is_some_and(|task_id| &**task_id == id))
            .ok_or_else(|| format!("{UNKNOWN_TASK_ERR}{id}"))
    }
}

/// Fires when a known task changes status, or when a new live task appears.
/// A task first seen already finished is recorded quietly, so a restored
/// session does not replay yesterday's tasks.
///
/// `previous` is keyed by id, because a session reset reuses positions.
pub(crate) fn diff_task_states<'a>(
    previous: &mut Vec<(Arc<str>, TaskStatus)>,
    current: impl Iterator<Item = TaskState<'a>>,
    mut emit: impl FnMut(TaskState<'a>),
) {
    let mut next = Vec::with_capacity(previous.len());
    for task in current {
        let announce = match previous.iter().find(|(id, _)| id == task.id) {
            Some((_, status)) => *status != task.status,
            None => matches!(task.status, TaskStatus::Working | TaskStatus::Idle),
        };
        if announce {
            emit(task);
        }
        next.push((Arc::clone(task.id), task.status));
    }
    *previous = next;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Msg;
    use crate::app::tests::{
        MONITOR_ID, MONITOR_PLUGIN, MOVE_RECEIPT, MOVED_ID, RESEARCH_NAME, SLOW_TOOL, agent_msg,
        app_with_moved_call, app_with_plugin_task, app_with_subagent_call, app_with_subagent_id,
        cancel_app, close_subagent_transcript, done, end_turn, error_app, finish_subagent,
        moved_done, moved_subagent_event, plugin_event, start_subagent, streaming_app,
        subagent_info, test_app, tool_start,
    };
    use crate::chat::{DONE_TEXT, ERROR_TEXT};
    use crate::components::Overlay;
    use crate::components::keybindings::key;
    use crate::repaint::Cadence;
    use maki_agent::{Envelope, ToolOutput};
    use maki_config::ToolKey;
    use test_case::test_case;

    const TASK_ID: &str = "toolu_01";
    const OTHER_ID: &str = "toolu_02";
    const MISSING_ID: &str = "toolu_nope";
    const BUILD_NAME: &str = "build";
    const UNCHANGED_CHAT: usize = 2;
    const DECLINE_REASON: &str = "batch is waiting on edit";
    const OTHER_DECLINE_REASON: &str = "16 background tasks already running";
    const FINAL_OUTPUT: &str = "test result: ok";

    fn call_id(n: usize) -> String {
        format!("{TASK_ID}_{n}")
    }

    fn app_running(calls: usize) -> App {
        let mut app = streaming_app();
        for n in 0..calls {
            app.update(agent_msg(tool_start(&call_id(n), SLOW_TOOL)));
        }
        app
    }

    fn press_background(app: &mut App) {
        app.update(Msg::Key(key::BACKGROUND.to_key_event()));
    }

    fn answered(outcome: MoveOutcome) -> flume::Receiver<MoveOutcome> {
        let (reply, answer) = flume::bounded(1);
        reply.send(outcome).unwrap();
        answer
    }

    #[test_case(vec![MoveOutcome::Moved], MOVED_ONE_MSG.to_owned() ; "one_moved")]
    #[test_case(vec![MoveOutcome::Moved, MoveOutcome::NoHook, MoveOutcome::Moved], format!("{MOVED_PREFIX}2{MOVED_MANY_SUFFIX}") ; "moves_are_counted")]
    #[test_case(vec![MoveOutcome::NotMovable(DECLINE_REASON.into()), MoveOutcome::Moved], MOVED_ONE_MSG.to_owned() ; "a_move_outranks_a_decline")]
    #[test_case(vec![MoveOutcome::NoHook, MoveOutcome::NotMovable(DECLINE_REASON.into()), MoveOutcome::NotMovable(OTHER_DECLINE_REASON.into())], DECLINE_REASON.to_owned() ; "the_first_decline_says_why")]
    #[test_case(vec![MoveOutcome::NoHook, MoveOutcome::NoHook], NOTHING_TO_MOVE_MSG.to_owned() ; "nothing_has_a_hook")]
    fn background_key_flashes_what_the_calls_answered(
        outcomes: Vec<MoveOutcome>,
        expected: String,
    ) {
        let mut app = app_running(outcomes.len());
        press_background(&mut app);
        assert_eq!(app.background_moves.pending.len(), outcomes.len());

        app.background_moves.pending = outcomes.into_iter().map(answered).collect();

        assert_eq!(app.tick_background_moves(), Dirty::YES);
        assert_eq!(app.status_bar.flash_text(), Some(expected.as_str()));
    }

    #[test]
    fn background_key_flashes_once_every_call_answered() {
        let mut app = app_running(2);
        press_background(&mut app);
        let (late_reply, late_answer) = flume::bounded(1);
        app.background_moves.pending = vec![answered(MoveOutcome::Moved), late_answer];

        assert_eq!(app.tick_background_moves(), Dirty::NO);
        assert_eq!(app.status_bar.flash_text(), None);

        late_reply.send(MoveOutcome::Moved).unwrap();
        assert_eq!(app.tick_background_moves(), Dirty::YES);
        assert_eq!(
            app.status_bar.flash_text(),
            Some(format!("{MOVED_PREFIX}2{MOVED_MANY_SUFFIX}").as_str())
        );
    }

    /// A call parked on its permission prompt has not started its work, so
    /// it is not asked. Ctrl+B still gets past the prompt to say so.
    #[test_case(1, 0 ; "alone_flashes_at_once")]
    #[test_case(2, 1 ; "beside_another_call")]
    fn a_call_waiting_for_permission_says_so(calls: usize, asked: usize) {
        let mut app = app_running(calls);
        app.update(agent_msg(AgentEvent::PermissionRequest {
            id: call_id(0),
            tool: ToolKey::native(SLOW_TOOL),
            scopes: vec![],
            reason: None,
        }));
        assert!(app.permission_prompt.is_open());

        press_background(&mut app);
        assert_eq!(app.background_moves.pending.len(), asked);
        app.background_moves.pending = (0..asked).map(|_| answered(MoveOutcome::NoHook)).collect();
        let _ = app.tick_background_moves();

        assert_eq!(
            app.status_bar.flash_text(),
            Some(format!("{SLOW_TOOL}{AWAITING_PERMISSION_SUFFIX}").as_str())
        );
    }

    /// The tab starts as a copy of the row holding the very buf the call
    /// streams into, so it shows the command still running with nothing
    /// replayed, and `/tasks` lists it as running.
    #[test]
    fn a_moved_call_becomes_a_working_task_sharing_its_live_buf() {
        let (app, buf) = app_with_moved_call();

        let tab = &app.chats[1];
        assert_eq!(tab.task_id().map(|id| &**id), Some(MOVED_ID));
        assert_eq!(tab.task_status(), TaskStatus::Working);
        assert_eq!(tab.plugin.as_deref(), Some(SLOW_TOOL));
        assert!(
            tab.live_buf(MOVED_ID)
                .is_some_and(|shared| Arc::ptr_eq(&shared, &buf))
        );
        let badge = app.chats[0]
            .tool_message(MOVED_ID)
            .and_then(|row| row.background);
        assert_eq!(
            badge.map(|badge| (badge.id, badge.started_at.is_some(), badge.outcome)),
            Some((MOVED_ID.to_owned(), true, None))
        );
        let tasks = serde_json::to_value(app.tasks()).unwrap();
        assert_eq!(tasks[1]["status"], "working");
    }

    /// A moved call may run for hours, and its tab keeps the spinner of the
    /// row it copied. Only the tab on screen may claim that motion, or an
    /// idle session paints at spinner rate for as long as the task lives.
    #[test]
    fn a_background_task_spins_only_while_its_tab_is_on_screen() {
        let (mut app, _buf) = app_with_moved_call();
        end_turn(&mut app);
        assert_eq!(app.cadence(), Cadence::IDLE);

        app.focus_task(MOVED_ID).unwrap();
        assert_eq!(app.cadence(), Cadence::SPINNER);
    }

    /// A reply wakes nothing, so the loop keeps looking without painting
    /// while one is owed, and a session out of focus still reads its own.
    #[test]
    fn a_pending_move_is_looked_for_until_it_answers() {
        let (mut app, _buf) = app_with_moved_call();
        end_turn(&mut app);
        let (reply, answer) = flume::bounded(1);
        app.background_moves.pending = vec![answer];
        assert_eq!(app.cadence(), Cadence::PENDING);

        reply.send(MoveOutcome::Moved).unwrap();
        app.tick_background();

        assert!(app.background_moves.pending.is_empty());
        assert_eq!(app.status_bar.flash_text(), Some(MOVED_ONE_MSG));
    }

    fn final_result(is_error: bool) -> AgentEvent {
        AgentEvent::ToolDone(Box::new(ToolDoneEvent {
            call: None,
            id: MOVED_ID.into(),
            tool: SLOW_TOOL.into(),
            output: Arc::new(ToolOutput::Plain(FINAL_OUTPUT.into())),
            is_error,
            annotation: None,
            written_path: None,
            backgrounded: false,
        }))
    }

    /// The end of a moved call travels the plugin task path: its final
    /// `ToolDone`, then `StreamClosed`, whether or not the envelope names
    /// the task.
    #[test_case(false, false, false, TaskStatus::Done,  BackgroundOutcome::Done      ; "finished")]
    #[test_case(false, false, true,  TaskStatus::Done,  BackgroundOutcome::Done      ; "finished_named")]
    #[test_case(true,  false, false, TaskStatus::Error, BackgroundOutcome::Error     ; "failed")]
    #[test_case(false, true,  false, TaskStatus::Error, BackgroundOutcome::Cancelled ; "stopped_by_the_user")]
    fn the_final_result_ends_the_task_and_its_badge(
        is_error: bool,
        stopped: bool,
        named: bool,
        status: TaskStatus,
        outcome: BackgroundOutcome,
    ) {
        let (mut app, _buf) = app_with_moved_call();
        if stopped {
            assert!(matches!(
                app.stop_task(MOVED_ID),
                Ok(Action::CancelPluginTask { id }) if id == MOVED_ID
            ));
        }
        for event in [final_result(is_error), AgentEvent::StreamClosed] {
            app.deliver(
                Envelope {
                    event,
                    subagent: named.then(|| subagent_info(MOVED_ID, SLOW_TOOL)),
                    run_id: 0,
                },
                Some(Arc::from(SLOW_TOOL)),
            );
        }

        assert_eq!(app.chats.len(), 2);
        assert_eq!(app.chats[1].task_status(), status);
        assert_eq!(
            app.chats[0]
                .tool_message(MOVED_ID)
                .and_then(|row| row.background)
                .and_then(|badge| badge.outcome),
            Some(outcome)
        );
        assert_eq!(
            app.state.session.tool_outputs()[MOVED_ID].as_text(),
            MOVE_RECEIPT,
            "history keeps the receipt the model got"
        );
    }

    fn deliver_final_result(app: &mut App, is_error: bool) {
        app.deliver(
            Envelope {
                event: final_result(is_error),
                subagent: None,
                run_id: 0,
            },
            Some(Arc::from(SLOW_TOOL)),
        );
    }

    /// An adopted subagent's session may close before its call's final
    /// result or after it. The close knows no verdict, so the result decides
    /// the task and the main row's badge either way.
    #[test_case(false, false, TaskStatus::Done,  BackgroundOutcome::Done  ; "close_then_success")]
    #[test_case(true,  false, TaskStatus::Error, BackgroundOutcome::Error ; "close_then_failure")]
    #[test_case(true,  true,  TaskStatus::Error, BackgroundOutcome::Error ; "failure_then_close")]
    fn an_adopted_subagent_ends_on_its_calls_result(
        is_error: bool,
        result_first: bool,
        status: TaskStatus,
        outcome: BackgroundOutcome,
    ) {
        let mut app = app_with_subagent_call(MOVED_ID);
        app.update(agent_msg(moved_done(MOVED_ID)));

        if result_first {
            deliver_final_result(&mut app, is_error);
        }
        moved_subagent_event(&mut app, MOVED_ID, AgentEvent::StreamClosed);
        if !result_first {
            deliver_final_result(&mut app, is_error);
        }

        assert_eq!(app.chats.len(), 2);
        assert_eq!(app.chats[1].task_status(), status);
        assert_eq!(
            app.chats[0]
                .tool_message(MOVED_ID)
                .and_then(|row| row.background)
                .and_then(|badge| badge.outcome),
            Some(outcome)
        );
    }

    fn app_with_two_subagents() -> App {
        let mut app = app_with_subagent_id(TASK_ID);
        start_subagent(&mut app, OTHER_ID, BUILD_NAME);
        app
    }

    fn app_with_two_finished_subagents() -> App {
        let mut app = app_with_two_subagents();
        close_subagent_transcript(&mut app, TASK_ID);
        close_subagent_transcript(&mut app, OTHER_ID);
        app
    }

    #[test]
    fn stop_tool_task_routes_cancellation_without_changing_focus_or_siblings() {
        let mut app = app_with_two_subagents();
        assert!(matches!(
            app.stop_task(TASK_ID),
            Ok(Action::CancelSubagent { tool_use_id }) if tool_use_id == TASK_ID
        ));
        assert_eq!(app.active_task_id().as_ref(), MAIN_TASK_ID);
        assert_eq!(app.chats[1].task_status(), TaskStatus::Error);
        assert_eq!(app.chats[1].last_message_text(), CANCELLED_TEXT);
        assert_eq!(app.chats[2].task_status(), TaskStatus::Working);
    }

    #[test_case(false ; "running")]
    #[test_case(true ; "idle")]
    fn stop_plugin_task_routes_to_its_native_session(idle: bool) {
        let mut app = app_with_plugin_task();
        if idle {
            plugin_event(&mut app, done());
        }
        assert!(matches!(
            app.stop_task(MONITOR_ID),
            Ok(Action::CancelPluginTask { id }) if id == MONITOR_ID
        ));
        assert_eq!(app.chats[1].task_status(), TaskStatus::Error);
        assert_eq!(app.active_task_id().as_ref(), MAIN_TASK_ID);
    }

    #[test]
    fn plugin_task_is_listed_and_stoppable_before_its_first_prompt() {
        let mut app = test_app();
        let info = subagent_info(MONITOR_ID, BUILD_NAME);
        app.open_plugin_task(&info, Arc::from(MONITOR_PLUGIN));
        let tasks = serde_json::to_value(app.tasks()).unwrap();
        assert_eq!(tasks[1]["id"], MONITOR_ID);
        assert_eq!(tasks[1]["status"], "idle");
        assert_eq!(tasks[1]["plugin"], MONITOR_PLUGIN);

        assert!(app.stop_task(MONITOR_ID).is_ok());
        app.open_plugin_task(&info, Arc::from(MONITOR_PLUGIN));
        assert_eq!(app.chats.len(), 2);
        assert_eq!(app.chats[1].task_status(), TaskStatus::Error);
    }

    /// `chat_index` is wiped at the end of every turn, so a tool task that
    /// lost its route has no agent left to cancel.
    #[test_case(MAIN_TASK_ID, |_| {}, MAIN_STOP_ERR.to_owned() ; "main")]
    #[test_case(MISSING_ID, |_| {}, format!("{UNKNOWN_TASK_ERR}{MISSING_ID}") ; "unknown")]
    #[test_case(TASK_ID, |app| close_subagent_transcript(app, TASK_ID), format!("{NOT_RUNNING_ERR}{TASK_ID}") ; "finished")]
    #[test_case(TASK_ID, |app| app.chat_index.clear(), format!("{NOT_RUNNING_ERR}{TASK_ID}") ; "turn_ended")]
    fn stop_refuses_without_touching_any_task(id: &str, setup: fn(&mut App), expected: String) {
        let mut app = app_with_two_subagents();
        setup(&mut app);
        let before = serde_json::to_value(app.tasks()).unwrap();
        assert_eq!(app.stop_task(id).err(), Some(expected));
        assert_eq!(serde_json::to_value(app.tasks()).unwrap(), before);
    }

    #[test]
    fn stop_refuses_a_session_that_lost_focus() {
        let mut app = app_with_two_subagents();
        let stale = MakiId::generate().to_string();
        assert_eq!(
            app.stop_task_in_session(TASK_ID, &stale).err(),
            Some(format!("{SESSION_CHANGED_ERR}{stale}"))
        );
        assert_eq!(app.chats[1].task_status(), TaskStatus::Working);

        let own = app.state.session.id.to_string();
        assert!(app.stop_task_in_session(TASK_ID, &own).is_ok());
    }

    /// Restores the two subagents in the reverse of the order their ids
    /// suggest, so a lookup guessing a position from an id lands on the wrong
    /// transcript instead of being right by accident.
    fn restored_app_with_two_subagents() -> App {
        let mut app = app_with_subagent_id(OTHER_ID);
        start_subagent(&mut app, TASK_ID, BUILD_NAME);
        // Only a subagent that handed over its transcript survives a reload,
        // so both have to close before the session is written.
        close_subagent_transcript(&mut app, OTHER_ID);
        close_subagent_transcript(&mut app, TASK_ID);
        app.reset_ui_chrome();
        app.restore_display();
        app
    }

    fn state(id: &Arc<str>, status: TaskStatus) -> TaskState<'_> {
        TaskState {
            id,
            name: "task",
            status,
            plugin: None,
        }
    }

    fn collect(
        previous: &mut Vec<(Arc<str>, TaskStatus)>,
        current: &[(Arc<str>, TaskStatus)],
    ) -> Vec<(String, TaskStatus)> {
        let mut fired = Vec::new();
        diff_task_states(
            previous,
            current.iter().map(|(id, status)| state(id, *status)),
            |task| fired.push((task.id.to_string(), task.status)),
        );
        fired
    }

    /// Identity lives on the chat, not in `chat_index`: that one is wiped at
    /// the end of every turn, while the picker keeps addressing tasks long
    /// after.
    #[test_case(MAIN_TASK_ID, Some(0) ; "main")]
    #[test_case(TASK_ID, Some(1)      ; "subagent")]
    #[test_case(MISSING_ID, None      ; "unknown_id")]
    fn focus_addresses_a_task_by_id_alone(id: &str, expected: Option<usize>) {
        let mut app = app_with_two_subagents();
        app.focus_task(OTHER_ID).unwrap();
        assert_eq!(app.active_chat, UNCHANGED_CHAT);
        app.chat_index.clear();

        let err = app.focus_task(id).err();

        assert_eq!(
            err,
            expected
                .is_none()
                .then(|| format!("{UNKNOWN_TASK_ERR}{id}"))
        );
        assert_eq!(app.active_chat, expected.unwrap_or(UNCHANGED_CHAT));
    }

    /// A reload rebuilds the chats from scratch, so the id is all the picker
    /// has left to aim with.
    #[test_case(OTHER_ID, RESEARCH_NAME, 1 ; "first_restored")]
    #[test_case(TASK_ID, BUILD_NAME, 2     ; "second_restored")]
    fn focus_addresses_a_restored_task_by_id(id: &str, name: &str, expected: usize) {
        let mut app = restored_app_with_two_subagents();
        app.focus_task(id).unwrap();

        assert_eq!(app.active_chat, expected);
        let chat = &app.chats[app.active_chat];
        assert_eq!(chat.task_id().map(|task_id| &**task_id), Some(id));
        assert_eq!(chat.name, name);
    }

    /// A subagent reaches disk when it spawns but its transcript only when it
    /// ends, so quitting mid-turn strands one with no messages. Restoring it
    /// would pin a task no agent backs at the top of the picker, running
    /// forever. The reload drops it instead.
    #[test]
    fn a_subagent_stranded_without_a_transcript_is_not_restored() {
        let mut app = app_with_subagent_id(TASK_ID);
        start_subagent(&mut app, OTHER_ID, BUILD_NAME);
        close_subagent_transcript(&mut app, OTHER_ID);

        app.reset_ui_chrome();
        app.restore_display();

        let restored: Vec<_> = app
            .task_states()
            .map(|task| (task.id.to_string(), task.status))
            .collect();
        assert_eq!(restored, vec![(OTHER_ID.to_owned(), TaskStatus::Done)]);
        let recorded: Vec<_> = app
            .state
            .session
            .subagents()
            .iter()
            .map(|sa| sa.tool_use_id.clone())
            .collect();
        assert_eq!(recorded, vec![OTHER_ID.to_owned()], "and stays dropped");
    }

    /// The `task` tool closes the subagent session before it reports a failure,
    /// so the `SubagentHistory` that only knows "it ended" always arrives before
    /// the `ToolDone` holding the verdict. Let the first one decide and every
    /// failure reads as done.
    #[test_case(
        |app: &mut App| close_subagent_transcript(app, TASK_ID),
        TaskStatus::Done, DONE_TEXT
        ; "a_close_nothing_follows_reads_as_done"
    )]
    #[test_case(
        |app| {
            close_subagent_transcript(app, TASK_ID);
            finish_subagent(app, TASK_ID, true);
        },
        TaskStatus::Error, ERROR_TEXT
        ; "a_late_verdict_corrects_the_close"
    )]
    #[test_case(
        |app| {
            finish_subagent(app, TASK_ID, true);
            close_subagent_transcript(app, TASK_ID);
        },
        TaskStatus::Error, ERROR_TEXT
        ; "a_close_never_clears_a_verdict"
    )]
    fn the_most_specific_ending_decides(end: fn(&mut App), status: TaskStatus, text: &str) {
        let mut app = app_with_subagent_id(TASK_ID);
        end(&mut app);
        assert_eq!(app.chats[1].task_status(), status);
        assert_eq!(app.chats[1].last_message_text(), text);
    }

    /// No way of ending a turn may leave a task `working`: nothing runs after
    /// to correct it, and the picker would spin on it forever. The finished
    /// task comes along to show the sweep leaves it alone.
    #[test_case(end_turn as fn(&mut App) ; "turn_end")]
    #[test_case(error_app                ; "parent_error")]
    #[test_case(cancel_app               ; "user_cancel")]
    fn a_turn_ending_terminalizes_every_unfinished_task(terminate: fn(&mut App)) {
        let mut app = app_with_two_subagents();
        finish_subagent(&mut app, TASK_ID, false);
        assert_eq!(app.chats[2].task_status(), TaskStatus::Working);

        terminate(&mut app);

        assert_eq!(
            app.task_states().map(|t| t.status).collect::<Vec<_>>(),
            vec![TaskStatus::Done, TaskStatus::Error]
        );
    }

    /// The picker remembers the focused id before it previews anything and
    /// goes back there on cancel. With two entries claiming focus, or none,
    /// the user is stranded on a task they were only peeking at.
    #[test_case(MAIN_TASK_ID, 0 ; "main")]
    #[test_case(TASK_ID, 1      ; "first_task")]
    #[test_case(OTHER_ID, 2     ; "second_task")]
    fn exactly_one_task_reports_focused(id: &str, expected: usize) {
        let mut app = app_with_two_subagents();
        app.focus_task(OTHER_ID).unwrap();
        app.focus_task(id).unwrap();

        let tasks = app.tasks();
        let focused: Vec<_> = tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.focused)
            .map(|(idx, task)| (idx, &*task.id))
            .collect();
        assert_eq!(focused, vec![(expected, id)]);
    }

    /// A task announces itself the moment it shows up working and on every
    /// change after, never twice for the same state. A session reset forgets
    /// it, so running the same task again reads as new.
    #[test]
    fn diff_announces_first_sight_and_every_change() {
        let id: Arc<str> = Arc::from(TASK_ID);
        let working = [(Arc::clone(&id), TaskStatus::Working)];
        let mut previous = Vec::new();

        assert_eq!(
            collect(&mut previous, &working),
            vec![(TASK_ID.to_owned(), TaskStatus::Working)]
        );
        assert!(collect(&mut previous, &working).is_empty());
        assert_eq!(
            collect(&mut previous, &[(Arc::clone(&id), TaskStatus::Done)]),
            vec![(TASK_ID.to_owned(), TaskStatus::Done)]
        );

        assert!(collect(&mut previous, &[]).is_empty());
        assert!(previous.is_empty());
        assert_eq!(
            collect(&mut previous, &working),
            vec![(TASK_ID.to_owned(), TaskStatus::Working)]
        );
    }

    #[test]
    fn removing_a_finished_task_drops_it_everywhere() {
        let mut app = app_with_two_subagents();
        close_subagent_transcript(&mut app, TASK_ID);
        assert!(app.state.session.subagent_messages().contains_key(TASK_ID));

        app.remove_task(TASK_ID).unwrap();

        let listed: Vec<_> = app.task_states().map(|task| task.id.to_string()).collect();
        assert_eq!(listed, [OTHER_ID]);
        let recorded: Vec<_> = app
            .state
            .session
            .subagents()
            .iter()
            .map(|sa| sa.tool_use_id.as_str())
            .collect();
        assert_eq!(recorded, [OTHER_ID], "and stays deleted");
        assert!(!app.state.session.subagent_messages().contains_key(TASK_ID));
    }

    /// Cancel is the one way to delete a task whose agent is still running.
    #[test_case(|app: &mut App| close_subagent_transcript(app, TASK_ID) ; "late_transcript")]
    #[test_case(|app| start_subagent(app, TASK_ID, RESEARCH_NAME)      ; "late_event")]
    fn a_deleted_task_stays_deleted_after_its_agent_stops(late: fn(&mut App)) {
        let mut app = app_with_subagent_id(TASK_ID);
        app.focus_task(TASK_ID).unwrap();
        app.handle_subagent_cancel();
        app.remove_task(TASK_ID).unwrap();

        late(&mut app);

        assert_eq!(app.chats.len(), 1);
        assert!(app.state.session.subagents().is_empty());
        assert!(!app.state.session.subagent_messages().contains_key(TASK_ID));
    }

    #[test_case(MAIN_TASK_ID, MAIN_DELETE_ERR ; "main")]
    #[test_case(MISSING_ID, UNKNOWN_TASK_ERR  ; "unknown_id")]
    #[test_case(TASK_ID, RUNNING_DELETE_ERR   ; "running")]
    fn remove_refuses_without_touching_chats(id: &str, prefix: &str) {
        let mut app = app_with_two_subagents();
        assert!(app.remove_task(id).unwrap_err().starts_with(prefix));
        assert_eq!(app.chats.len(), 3);
    }

    /// An idle plugin task is alive: its session answers the next prompt.
    #[test]
    fn remove_refuses_an_idle_plugin_task() {
        let mut app = app_with_plugin_task();
        plugin_event(&mut app, done());
        assert_eq!(app.chats[1].task_status(), TaskStatus::Idle);
        assert!(
            app.remove_task(MONITOR_ID)
                .unwrap_err()
                .starts_with(RUNNING_DELETE_ERR)
        );
        assert_eq!(app.chats.len(), 2);
    }

    /// `chat_index` routes by position, so the chats after the deleted one
    /// have to shift down with it.
    #[test]
    fn removing_a_task_before_the_focus_keeps_the_same_task_on_screen() {
        let mut app = app_with_two_finished_subagents();
        app.focus_task(OTHER_ID).unwrap();

        app.remove_task(TASK_ID).unwrap();

        assert_eq!(app.active_chat, 1);
        assert_eq!(app.chats[app.active_chat].name, BUILD_NAME);
        assert_eq!(app.chat_index.get(OTHER_ID), Some(&1));
    }

    /// One input box serves every chat, so stepping off a deleted chat has to
    /// bring back the draft of the chat we land on.
    #[test]
    fn removing_the_focused_task_moves_the_view_to_the_previous_chat() {
        const MAIN_DRAFT: &str = "for main";
        let mut app = app_with_two_finished_subagents();
        app.update(Msg::Paste(MAIN_DRAFT.into()));
        app.focus_task(TASK_ID).unwrap();

        app.remove_task(TASK_ID).unwrap();

        assert_eq!(app.active_chat, 0);
        assert_eq!(app.main_draft().0, MAIN_DRAFT);
    }

    /// A plugin task shows up idle the moment it opens, long before its first
    /// prompt, and has to announce itself just like a working one.
    #[test_case(TaskStatus::Working ; "working")]
    #[test_case(TaskStatus::Idle ; "idle")]
    fn diff_announces_a_live_task_on_first_sight(status: TaskStatus) {
        let mut previous = Vec::new();
        assert_eq!(
            collect(&mut previous, &[(Arc::from(TASK_ID), status)]),
            vec![(TASK_ID.to_owned(), status)]
        );
    }

    /// A task first seen already finished stays quiet, so a reload does not
    /// replay old news. It is still recorded, or its next change would look
    /// like another first sight and stay quiet too.
    #[test]
    fn a_task_first_seen_finished_is_recorded_silently() {
        let one: Arc<str> = Arc::from(TASK_ID);
        let two: Arc<str> = Arc::from(OTHER_ID);
        let mut previous = Vec::new();

        assert!(collect(&mut previous, &[(Arc::clone(&one), TaskStatus::Done)]).is_empty());
        assert_eq!(
            collect(&mut previous, &[(one, TaskStatus::Error)]),
            vec![(TASK_ID.to_owned(), TaskStatus::Error)]
        );

        assert_eq!(
            collect(&mut previous, &[(Arc::clone(&two), TaskStatus::Working)]),
            vec![(OTHER_ID.to_owned(), TaskStatus::Working)]
        );
        assert_eq!(previous, vec![(two, TaskStatus::Working)]);
    }
}
