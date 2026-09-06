//! A task is a subagent chat, addressed by its `tool_use_id`. The main chat
//! goes by [`MAIN_TASK_ID`] and carries no status, since its work is the
//! session's own and `maki.session.live()` already reports that.
//!
//! Both `maki.task.list()` and the `TaskStatusChanged` autocmd serialize the
//! types below, so the two can never spell a status differently.

use std::sync::Arc;

use serde::Serialize;

use crate::app::App;
use crate::components::DisplayRole;

pub(crate) const MAIN_TASK_ID: &str = "main";
const UNKNOWN_TASK_ERR: &str = "unknown task: ";
const MAIN_DELETE_ERR: &str = "cannot delete the main chat";
const RUNNING_DELETE_ERR: &str = "task is still running: ";

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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TaskStatus {
    Working,
    Done,
    Error,
}

impl From<Option<TaskOutcome>> for TaskStatus {
    fn from(outcome: Option<TaskOutcome>) -> Self {
        match outcome {
            None => Self::Working,
            Some(TaskOutcome::Error) => Self::Error,
            Some(TaskOutcome::Unknown | TaskOutcome::Done) => Self::Done,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TaskState<'a> {
    pub(crate) id: &'a Arc<str>,
    pub(crate) name: &'a str,
    pub(crate) status: TaskStatus,
}

/// The wire shape of `maki.task.list()`. The main chat leaves `status` unset.
#[derive(Serialize)]
pub(crate) struct TaskInfo {
    id: Arc<str>,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<TaskStatus>,
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
            })
        })
    }

    /// The same walk widened to every chat, so the main chat at index 0 lands
    /// first on its own and no extra code path has to place it.
    pub(crate) fn tasks(&self) -> Vec<TaskInfo> {
        self.chats
            .iter()
            .enumerate()
            .map(|(idx, chat)| {
                let task_id = chat.task_id();
                TaskInfo {
                    id: task_id.map_or_else(|| Arc::from(MAIN_TASK_ID), Arc::clone),
                    name: chat.name.clone(),
                    status: task_id.map(|_| chat.task_status()),
                    focused: idx == self.active_chat,
                }
            })
            .collect()
    }

    /// The only writer of `active_chat` outside the chat cycling keys. Tasks
    /// are looked up by id, never by position and never through `chat_index`,
    /// a routing cache wiped at the end of every turn.
    pub(crate) fn focus_task(&mut self, id: &str) -> Result<(), String> {
        self.active_chat = if id == MAIN_TASK_ID {
            0
        } else {
            self.chats
                .iter()
                .position(|chat| chat.task_id().is_some_and(|task_id| &**task_id == id))
                .ok_or_else(|| format!("{UNKNOWN_TASK_ERR}{id}"))?
        };
        Ok(())
    }

    /// Drops a finished subagent chat and its stored transcript, so a reload
    /// cannot bring the deleted task back. Main and still-running tasks are
    /// refused. If the deleted task was focused, focus moves to the previous
    /// chat (or main if it was first).
    pub(crate) fn remove_task(&mut self, id: &str) -> Result<(), String> {
        if id == MAIN_TASK_ID {
            return Err(MAIN_DELETE_ERR.to_owned());
        }
        let pos = self
            .chats
            .iter()
            .position(|chat| chat.task_id().is_some_and(|task_id| &**task_id == id))
            .ok_or_else(|| format!("{UNKNOWN_TASK_ERR}{id}"))?;
        let chat = &self.chats[pos];
        if !chat.is_finished() {
            return Err(format!("{RUNNING_DELETE_ERR}{id}"));
        }

        self.chats.remove(pos);
        self.chat_index.retain(|_, &mut idx| idx != pos);
        for idx in self.chat_index.values_mut() {
            if *idx > pos {
                *idx -= 1;
            }
        }
        if self.active_chat >= pos {
            self.active_chat = self.active_chat.saturating_sub(1);
        }
        self.state.session_mut().remove_subagent(id, |m| {
            m.tool_uses().map(|(id, _, _)| id.to_owned()).collect()
        });
        Ok(())
    }
}

/// Fires when a known task changes status, or when a new one shows up working.
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
            None => task.status == TaskStatus::Working,
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
    use crate::app::tests::{
        RESEARCH_NAME, app_with_subagent_id, cancel_app, close_subagent_transcript, end_turn,
        error_app, finish_subagent, start_subagent,
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

        assert_eq!(app.chats.len(), 2);
        assert_eq!(
            app.task_states()
                .map(|task| task.id.to_string())
                .collect::<Vec<_>>(),
            vec![OTHER_ID.to_owned()]
        );
        assert_eq!(
            app.state.session.subagents()[0].tool_use_id,
            OTHER_ID,
            "and stays deleted"
        );
        assert!(!app.state.session.subagent_messages().contains_key(TASK_ID));
    }

    #[test_case(MAIN_TASK_ID, MAIN_DELETE_ERR ; "main_is_refused")]
    #[test_case(MISSING_ID, UNKNOWN_TASK_ERR ; "a_gone_task_reports_its_id")]
    fn remove_refuses_without_touching_chats(id: &str, prefix: &str) {
        let mut app = app_with_two_finished_subagents();
        assert!(app.remove_task(id).unwrap_err().starts_with(prefix));
        assert_eq!(app.chats.len(), 3);
    }

    #[test]
    fn remove_refuses_a_running_task() {
        let mut app = app_with_two_subagents();
        let expected = format!("{RUNNING_DELETE_ERR}{TASK_ID}");
        assert_eq!(app.remove_task(TASK_ID), Err(expected));
        assert_eq!(app.chats.len(), 3);
    }

    /// The focus follows its task by name where possible: `chat_index` is a
    /// turn-scoped cache, so the ids it routes to must shift with the chats.
    #[test]
    fn removing_a_task_before_the_focus_keeps_the_same_task_on_screen() {
        let mut app = app_with_two_finished_subagents();
        app.focus_task(OTHER_ID).unwrap();
        assert_eq!(app.active_chat, 2);

        app.remove_task(TASK_ID).unwrap();

        assert_eq!(app.active_chat, 1);
        assert_eq!(app.chats[app.active_chat].name, BUILD_NAME);
        assert_eq!(app.chat_index.get(OTHER_ID), Some(&1));
    }

    /// The focused task itself may be the one deleted; the view lands on the
    /// previous chat rather than pointing past the end of the list.
    #[test]
    fn removing_the_focused_task_moves_the_view_to_the_previous_chat() {
        let mut app = app_with_two_finished_subagents();
        app.focus_task(OTHER_ID).unwrap();

        app.remove_task(OTHER_ID).unwrap();

        assert_eq!(app.active_chat, 1);
        assert_eq!(app.chats[app.active_chat].name, RESEARCH_NAME);
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
