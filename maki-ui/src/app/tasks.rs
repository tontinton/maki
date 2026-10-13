//! A task is a subagent chat, addressed by its `tool_use_id`. The main chat
//! goes by [`MAIN_TASK_ID`] and carries no status, since its work is the
//! session's own and `maki.session.live()` already reports that.
//!
//! Both `maki.task.list()` and the `TaskStatusChanged` autocmd serialize the
//! types below, so the two can never spell a status differently.

use std::sync::Arc;

use maki_agent::tools::MAIN_TASK_ID;
use maki_storage::id::MakiId;
use serde::Serialize;
use strum::IntoStaticStr;

use crate::app::App;
use crate::chat::CANCELLED_TEXT;
use crate::components::{Action, DisplayRole};

const UNKNOWN_TASK_ERR: &str = "unknown task: ";
const MAIN_DELETE_ERR: &str = "cannot delete the main chat";
const RUNNING_DELETE_ERR: &str = "task is still running: ";
const MAIN_STOP_ERR: &str = "cannot stop the main chat";
const NOT_RUNNING_ERR: &str = "task is not running: ";
const SESSION_CHANGED_ERR: &str = "task session is no longer focused: ";

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

impl App {
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
        self.subagent_answers.remove(id);
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
    /// `chat_index`, a routing cache wiped at the end of every turn.
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
        MONITOR_ID, MONITOR_PLUGIN, RESEARCH_NAME, app_with_plugin_task, app_with_subagent_id,
        cancel_app, close_subagent_transcript, done, end_turn, error_app, finish_subagent,
        plugin_event, start_subagent, subagent_info, test_app,
    };
    use crate::chat::{DONE_TEXT, ERROR_TEXT};
    use test_case::test_case;

    const TASK_ID: &str = "toolu_01";
    const OTHER_ID: &str = "toolu_02";
    const MISSING_ID: &str = "toolu_nope";
    const BUILD_NAME: &str = "build";
    const UNCHANGED_CHAT: usize = 2;

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
