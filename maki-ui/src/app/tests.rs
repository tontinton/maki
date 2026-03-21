use super::*;
use crate::chat::{CANCELLED_TEXT, DONE_TEXT, ERROR_TEXT};
use crate::components::command::ParsedCommand;
use crate::components::keybindings::{KeybindContext, key as kb};
use crate::components::{TEST_CONTEXT_WINDOW, key, test_pricing};
use crate::selection::{EdgeScroll, SelectableZone, SelectionZone};
use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    AgentMode, QuestionInfo, QuestionOption, ToolDoneEvent, ToolOutput, ToolStartEvent,
    TurnCompleteEvent,
};
use maki_config::{PermissionsConfig, UiConfig};
use ratatui::layout::Rect;
use std::env;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use test_case::test_case;

fn set_zone(app: &mut App, zone: SelectionZone, area: Rect) {
    app.zones[zone.idx()] = Some(SelectableZone {
        area,
        highlight_area: area,
        zone,
    });
}

fn test_app() -> App {
    let writer = Arc::new(StorageWriter::new(DataDir::from_path(env::temp_dir())));
    let mcp_infos = Arc::new(ArcSwap::from_pointee(Vec::new()));
    let permissions = Arc::new(PermissionManager::new(
        PermissionsConfig {
            allow_all: false,
            rules: vec![],
        },
        PathBuf::from("/tmp"),
    ));
    App::new(
        "test-model".into(),
        test_pricing(),
        TEST_CONTEXT_WINDOW,
        AppSession::new("test-model", "/tmp/test"),
        DataDir::from_path(env::temp_dir()),
        Arc::new(ArcSwapOption::empty()),
        mcp_infos,
        writer,
        UiConfig::default(),
        100,
        permissions,
    )
}

fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Msg {
    Msg::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn agent_msg(event: AgentEvent) -> Msg {
    agent_msg_with_run_id(event, 1)
}

fn agent_msg_with_run_id(event: AgentEvent, run_id: u64) -> Msg {
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: None,
        run_id,
    }))
}

fn subagent_info(parent_id: &str, name: &str) -> SubagentInfo {
    SubagentInfo {
        parent_tool_use_id: parent_id.into(),
        name: name.into(),
        prompt: None,
        model: None,
        answer_tx: None,
    }
}

fn subagent_msg(event: AgentEvent, parent_id: &str, name: Option<&str>) -> Msg {
    subagent_msg_with_run_id(event, parent_id, name, 1)
}

fn subagent_msg_with_run_id(
    event: AgentEvent,
    parent_id: &str,
    name: Option<&str>,
    run_id: u64,
) -> Msg {
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: Some(subagent_info(parent_id, name.unwrap_or("Agent"))),
        run_id,
    }))
}

fn subagent_msg_with_prompt(
    event: AgentEvent,
    parent_id: &str,
    name: Option<&str>,
    prompt: Option<&str>,
) -> Msg {
    let mut info = subagent_info(parent_id, name.unwrap_or("Agent"));
    info.prompt = prompt.map(String::from);
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: Some(info),
        run_id: 1,
    }))
}

fn subagent_msg_with_model(event: AgentEvent, parent_id: &str, name: &str, model: &str) -> Msg {
    let mut info = subagent_info(parent_id, name);
    info.model = Some(model.into());
    Msg::Agent(Box::new(Envelope {
        event,
        subagent: Some(info),
        run_id: 1,
    }))
}

#[test]
fn typing_and_submit() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::SendMessage(s) if s.message == "hi"));
    assert_eq!(app.status, Status::Streaming);
}

#[test]
fn ctrl_c_clears_nonempty_input() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(actions.is_empty());
    assert!(!app.should_quit);
    assert_eq!(app.input_box.buffer.value(), "");
}

#[test]
fn ctrl_c_quits_when_input_empty() {
    for status in [Status::Idle, Status::Streaming] {
        let mut app = test_app();
        app.status = status;
        let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
        assert!(app.should_quit);
        assert!(matches!(&actions[0], Action::Quit));
    }
}

#[test]
fn error_event_sets_status() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
    assert!(matches!(app.status, Status::Error { ref message, .. } if message == "boom"));
}

#[test]
fn toggle_mode_state_machine() {
    let tab = |app: &mut App| app.update(Msg::Key(key(KeyCode::Tab)));

    let mut app = test_app();
    assert_eq!(app.mode, Mode::Build);

    tab(&mut app);
    assert_eq!(app.mode, Mode::Plan);
    let first_path = app.plan.path().unwrap().to_path_buf();
    assert!(first_path.to_str().unwrap().contains("plans"));

    tab(&mut app);
    assert_eq!(app.mode, Mode::Build);
    assert!(!app.plan.is_written());

    tab(&mut app);
    assert_eq!(app.mode, Mode::Plan);
    assert_eq!(app.plan.path().unwrap(), first_path);

    app.plan.mark_written();
    tab(&mut app);
    assert_eq!(app.mode, Mode::Build);
    assert!(app.plan.is_written());

    tab(&mut app);
    assert_eq!(app.mode, Mode::Plan);
    assert_eq!(app.plan.path().unwrap(), first_path);

    tab(&mut app);
    assert_eq!(app.mode, Mode::Build);
    assert_eq!(app.plan.path().unwrap(), first_path);

    app.mode = Mode::Build;
    app.status = Status::Streaming;
    app.run_id = 1;
    tab(&mut app);
    assert_eq!(app.mode, Mode::Plan);
    assert_eq!(app.plan.path().unwrap(), first_path);
}

#[test_case(Mode::Build, true,  Some("plan.md") ; "build_sends_written_plan")]
#[test_case(Mode::Build, false, None              ; "build_ignores_unwritten_plan")]
#[test_case(Mode::Plan,  true,  Some("plan.md") ; "plan_sends_written_plan")]
fn submit_pending_plan(mode: Mode, written: bool, expected: Option<&str>) {
    let mut app = test_app();
    app.mode = mode;
    app.plan = PlanState::with_path(PathBuf::from("plan.md"), written);
    let actions = type_and_submit(&mut app, "x");
    let Action::SendMessage(ref input) = actions[0] else {
        panic!("expected SendMessage");
    };
    assert_eq!(input.pending_plan.as_deref(), expected.map(Path::new));
}

#[test_case(ToolOutput::WriteCode { path: "plans/test.md".into(), byte_count: 100, lines: vec![] }, true  ; "write_matching")]
#[test_case(ToolOutput::Diff { path: "plans/test.md".into(), hunks: vec![], summary: String::new() }, true  ; "edit_matching")]
#[test_case(ToolOutput::WriteCode { path: "other.rs".into(), byte_count: 100, lines: vec![] }, false ; "write_non_matching")]
fn tool_done_sets_plan_written_flag(output: ToolOutput, expect_written: bool) {
    let mut app = test_app();
    app.mode = Mode::Plan;
    app.plan = PlanState::with_path(PathBuf::from("plans/test.md"), false);
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write",
        output,
        is_error: false,
    }))));

    assert_eq!(app.plan.is_written(), expect_written);
}

#[test]
fn altgr_chars_not_swallowed_by_ctrl_handler() {
    let mut app = test_app();
    let altgr_backslash = KeyEvent {
        code: KeyCode::Char('\\'),
        modifiers: KeyModifiers::CONTROL | KeyModifiers::ALT,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    app.update(Msg::Key(altgr_backslash));
    assert_eq!(app.input_box.buffer.value(), "hi\\");
}

#[test]
fn paste_works_regardless_of_status() {
    for status in [Status::Idle, Status::Streaming] {
        let mut app = test_app();
        app.status = status;
        let actions = app.update(Msg::Paste("pasted".into()));
        assert!(actions.is_empty());
        assert_eq!(app.input_box.buffer.value(), "pasted");
    }
}

#[test]
fn paste_routed_to_question_form_in_custom_mode() {
    let mut app = test_app();
    app.question_form.open(vec![QuestionInfo {
        question: "Pick one".into(),
        header: String::new(),
        options: vec![QuestionOption {
            label: "A".into(),
            description: String::new(),
        }],
        multiple: false,
    }]);
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Enter)));

    app.update(Msg::Paste("pasted".into()));
    assert_eq!(app.input_box.buffer.value(), "");
}

#[test]
fn paste_file_path_triggers_image_load() {
    let mut app = test_app();
    app.update(Msg::Paste("file:///tmp/nonexistent.png".into()));
    assert!(app.image_paste_rx.is_some());
    assert_eq!(app.input_box.buffer.value(), "");
}

#[test]
fn submit_during_streaming_queues_message() {
    let mut app = test_app();
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::SendMessage(_)));
    assert_eq!(app.status, Status::Streaming);

    app.update(Msg::Key(key(KeyCode::Char('b'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(actions.is_empty());
    assert_eq!(app.queue.len(), 1);
    assert!(matches!(app.queue[0], QueuedItem::Message(ref m) if m.text == "b"));
}

#[test_case(error_app as fn(&mut App) ; "error")]
#[test_case(cancel_app as fn(&mut App) ; "cancel")]
fn clears_queue(terminate: fn(&mut App)) {
    let mut app = app_with_queued_message();
    terminate(&mut app);
    assert!(app.queue.is_empty());
}

fn queued_msg(text: &str) -> QueuedItem {
    QueuedItem::Message(QueuedMessage {
        text: text.into(),
        images: vec![],
    })
}

fn app_with_queued_message() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue.push(queued_msg("queued"));
    app
}

fn type_and_submit(app: &mut App, text: &str) -> Vec<Action> {
    for c in text.chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter)))
}

fn cancel_app(app: &mut App) {
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
}

fn error_app(app: &mut App) {
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
}

#[test]
fn first_submit_during_streaming_sends_to_agent() {
    let mut app = test_app();
    let (tx, rx) = flume::unbounded::<crate::AgentCommand>();
    app.cmd_tx = Some(tx);
    app.status = Status::Streaming;
    app.run_id = 1;

    type_and_submit(&mut app, "first");
    assert_eq!(app.queue.len(), 1);
    assert!(app.queue.dispatched());
    assert!(
        rx.try_recv().is_ok(),
        "first queued item should be sent eagerly"
    );

    type_and_submit(&mut app, "second");
    assert_eq!(app.queue.len(), 2);
    assert!(
        rx.try_recv().is_err(),
        "second item should not be sent eagerly"
    );
}

#[test]
fn consumed_event_pops_and_sends_next() {
    let mut app = test_app();
    let (tx, rx) = flume::unbounded::<crate::AgentCommand>();
    app.cmd_tx = Some(tx);
    app.status = Status::Streaming;
    app.run_id = 1;

    app.queue_and_notify(queued_msg("first"));
    app.queue_and_notify(queued_msg("second"));
    let _ = rx.try_recv();

    app.update(agent_msg(AgentEvent::QueueItemConsumed));
    assert_eq!(app.queue.len(), 1);
    assert!(app.queue.dispatched());
    assert!(rx.try_recv().is_ok(), "next item sent after consumed");
    assert_eq!(
        app.chats[0].message_count(),
        1,
        "consumed message shown in chat"
    );
}

#[test]
fn done_while_next_dispatched_stays_streaming() {
    let mut app = test_app();
    let (tx, rx) = flume::unbounded::<crate::AgentCommand>();
    app.cmd_tx = Some(tx);
    app.status = Status::Streaming;
    app.run_id = 1;

    app.queue_and_notify(queued_msg("first"));
    app.queue_and_notify(queued_msg("second"));
    let _ = rx.try_recv();

    app.update(agent_msg(AgentEvent::QueueItemConsumed));
    assert!(app.queue.dispatched());

    let actions = app.update(done_event());
    assert!(actions.is_empty());
    assert!(
        matches!(app.status, Status::Streaming),
        "should stay streaming while next item is in flight"
    );
}
#[test]
fn stale_consumed_after_delete_is_noop() {
    let mut app = test_app();
    let (tx, _rx) = flume::unbounded::<crate::AgentCommand>();
    app.cmd_tx = Some(tx);
    app.status = Status::Streaming;
    app.run_id = 1;

    app.queue_and_notify(queued_msg("first"));
    app.queue_and_notify(queued_msg("second"));
    assert!(app.queue.dispatched());

    app.queue.remove(0);
    assert!(!app.queue.dispatched());
    assert_eq!(app.queue.len(), 1);

    let before = app.chats[0].message_count();
    app.update(agent_msg(AgentEvent::QueueItemConsumed));
    assert_eq!(
        app.chats[0].message_count(),
        before,
        "stale consumed is noop"
    );
    assert_eq!(app.queue.len(), 1);
}

fn cmd(name: &'static str) -> ParsedCommand {
    ParsedCommand {
        name,
        args: String::new(),
    }
}

fn type_slash(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('/'))));
}

#[test]
fn typing_filters_palette() {
    let mut app = test_app();
    type_slash(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(key(KeyCode::Char('z'))));
    assert!(!app.command_palette.is_active());
}

#[test]
fn enter_executes_new_command() {
    let mut app = test_app();
    type_slash(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(&actions[0], Action::NewSession));
    assert!(!app.command_palette.is_active());
}

#[test]
fn ctrl_c_closes_palette() {
    let mut app = test_app();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(!app.command_palette.is_active());
}

#[test]
fn reset_session_preserves_plan() {
    let mut app = test_app();
    app.token_usage.input = 500;
    app.chats[0].context_size = 1000;
    app.mode = Mode::Build;
    app.plan = PlanState::with_path(PathBuf::from("plan.md"), true);
    app.queue.push(queued_msg("q"));
    app.queue.set_focus_at(0);
    app.update(Msg::Key(kb::HELP.to_key_event()));
    let (_tx, rx) = flume::bounded::<crate::components::btw_modal::BtwEvent>(1);
    app.btw_modal.open("q", rx);
    let actions = app.reset_session();
    assert!(matches!(&actions[0], Action::NewSession));
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.token_usage.input, 0);
    assert_eq!(app.chats[0].context_size, 0);
    assert_eq!(app.mode, Mode::Build);
    assert_eq!(app.plan.path(), Some(Path::new("plan.md")));
    assert!(app.plan.is_written());
    assert!(app.queue.is_empty());
    assert_eq!(app.chats.len(), 1);
    assert_eq!(app.chats[0].name, "Main");
    assert_eq!(app.active_chat, 0);
    assert!(app.chat_index.is_empty());
    assert!(app.queue.focus().is_none());
    assert!(!app.help_modal.is_open());
    assert!(!app.btw_modal.is_open());
}

#[test]
fn reset_session_assigns_new_plan_path_in_plan_mode() {
    let mut app = test_app();
    app.mode = Mode::Plan;
    app.plan = PlanState::with_path(PathBuf::from("old-plan.md"), false);
    app.reset_session();
    assert_eq!(app.mode, Mode::Plan);
    assert!(app.plan.path().is_some());
    assert_ne!(app.plan.path(), Some(Path::new("old-plan.md")));
}

#[test_case(true,  Some("leftover.md") ; "preserves_written")]
#[test_case(false, None                 ; "clears_unwritten")]
fn reset_session_plan_in_build_mode(written: bool, expected_path: Option<&str>) {
    let mut app = test_app();
    app.mode = Mode::Build;
    app.plan = PlanState::with_path(PathBuf::from("leftover.md"), written);
    app.reset_session();
    assert_eq!(app.mode, Mode::Build);
    assert_eq!(app.plan.path(), expected_path.map(Path::new));
    assert_eq!(app.plan.is_written(), written);
}

#[test]
fn load_session_clears_plan() {
    let tmp = TempDir::new().unwrap();
    let dir = DataDir::from_path(tmp.path().to_path_buf());
    let writer = Arc::new(StorageWriter::new(DataDir::from_path(
        tmp.path().to_path_buf(),
    )));
    let mcp_infos = Arc::new(ArcSwap::from_pointee(Vec::new()));
    let mut app = App::new(
        "test-model".into(),
        test_pricing(),
        TEST_CONTEXT_WINDOW,
        AppSession::new("test-model", "/tmp/test"),
        dir,
        Arc::new(ArcSwapOption::empty()),
        mcp_infos,
        writer,
        UiConfig::default(),
        100,
        Arc::new(PermissionManager::new(
            PermissionsConfig {
                allow_all: false,
                rules: vec![],
            },
            PathBuf::from("/tmp"),
        )),
    );
    app.session.messages.push(Message::user("test".into()));
    app.session.save(&app.storage).unwrap();
    let id = app.session.id.clone();
    app.mode = Mode::Build;
    app.plan = PlanState::with_path(PathBuf::from("old-plan.md"), true);
    app.load_session(id);
    assert_eq!(app.mode, Mode::Build);
    assert_eq!(app.plan.path(), None);
}

#[test]
fn tab_in_palette_closes_and_toggles_mode() {
    let mut app = test_app();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(key(KeyCode::Tab)));
    assert!(!app.command_palette.is_active());
    assert_eq!(app.mode, Mode::Plan);
}

#[test]
fn ctrl_p_n_navigation() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "sub".into() },
        "task1",
        Some("research"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.active_chat, 0);

    app.update(Msg::Key(kb::NEXT_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 1);

    app.update(Msg::Key(kb::NEXT_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 1);

    app.update(Msg::Key(kb::PREV_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 0);

    app.update(Msg::Key(kb::PREV_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 0);
}

#[test]
fn subagents_get_descriptive_names() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "a".into() },
        "task1",
        Some("first"),
    ));
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "b".into() },
        "task2",
        Some("second"),
    ));
    assert_eq!(app.chats.len(), 3);
    assert_eq!(app.chats[1].name, "first");
    assert_eq!(app.chats[2].name, "second");
}

#[test]
fn subagent_prompt_shown_as_first_message() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg_with_prompt(
        AgentEvent::TextDelta { text: "ok".into() },
        "task1",
        Some("research"),
        Some("Find all TODO comments"),
    ));
    assert_eq!(app.chats[1].message_count(), 1);
    assert_eq!(app.chats[1].last_message_text(), "Find all TODO comments");
}

#[test]
fn subagent_prompt_not_duplicated_on_subsequent_events() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg_with_prompt(
        AgentEvent::TextDelta { text: "a".into() },
        "task1",
        Some("research"),
        Some("Find all TODO comments"),
    ));
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "b".into() },
        "task1",
        Some("research"),
    ));
    app.chats[1].flush();
    assert_eq!(app.chats[1].message_count(), 2);
    assert_eq!(app.chats[1].last_message_text(), "ab");
}

#[test]
fn turn_complete_tracks_usage_and_context_per_chat() {
    let mut app = app_with_subagent();

    let main_usage = TokenUsage {
        input: 100,
        output: 50,
        ..Default::default()
    };
    app.update(agent_msg(AgentEvent::TurnComplete(Box::new(
        TurnCompleteEvent {
            message: Default::default(),
            usage: main_usage,
            model: "test".into(),
            context_size: None,
        },
    ))));

    let sub_usage = TokenUsage {
        input: 200,
        output: 75,
        ..Default::default()
    };
    app.update(subagent_msg(
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Default::default(),
            usage: sub_usage,
            model: "test".into(),
            context_size: None,
        })),
        "task1",
        None,
    ));

    assert_eq!(app.token_usage.input, 300);
    assert_eq!(app.token_usage.output, 125);
    assert_eq!(app.chats[0].token_usage.input, 100);
    assert_eq!(app.chats[1].token_usage.input, 200);
    assert_eq!(app.chats[0].context_size, main_usage.context_tokens());
    assert_eq!(app.chats[1].context_size, sub_usage.context_tokens());
}

#[test]
fn cancel_resets_all_chats_and_indices() {
    let mut app = app_with_subagent();
    app.update(subagent_msg(
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "sub_t1".into(),
            tool: "bash",
            summary: "running".into(),
            annotation: None,
            input: None,
            output: None,
        })),
        "task1",
        None,
    ));

    cancel_app(&mut app);
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.chat_index.is_empty());
}

fn finish_subagent(app: &mut App, id: &str, is_error: bool) {
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: id.into(),
        tool: "task",
        output: ToolOutput::Plain("result".into()),
        is_error,
    }))));
}

fn finish_subagent_task(app: &mut App, is_error: bool) {
    finish_subagent(app, "task1", is_error);
}

#[test]
fn subagent_done_only_in_subagent_chat() {
    let mut app = app_with_subagent();
    finish_subagent_task(&mut app, false);
    assert_ne!(app.chats[0].last_message_role(), Some(&DisplayRole::Done));
}

#[test_case(|app: &mut App| finish_subagent_task(app, false), DONE_TEXT,      &DisplayRole::Done  ; "task_success")]
#[test_case(|app: &mut App| finish_subagent_task(app, true),  ERROR_TEXT,     &DisplayRole::Error ; "task_failure")]
#[test_case(cancel_app as fn(&mut App),                       CANCELLED_TEXT, &DisplayRole::Error ; "cancel")]
#[test_case(error_app  as fn(&mut App),                       ERROR_TEXT,     &DisplayRole::Error ; "main_error")]
fn subagent_terminal_marker(
    terminate: fn(&mut App),
    expected_text: &str,
    expected_role: &DisplayRole,
) {
    let mut app = app_with_subagent();
    terminate(&mut app);
    assert_eq!(app.chats[1].last_message_text(), expected_text);
    assert_eq!(app.chats[1].last_message_role(), Some(expected_role));
}

#[test_case(error_app  as fn(&mut App) ; "error")]
#[test_case(cancel_app as fn(&mut App) ; "cancel")]
fn subagent_already_done_not_double_marked(terminate: fn(&mut App)) {
    let mut app = app_with_subagent();
    finish_subagent_task(&mut app, false);
    let count_before = app.chats[1].message_count();
    terminate(&mut app);
    assert_eq!(app.chats[1].message_count(), count_before);
    assert_eq!(app.chats[1].last_message_text(), DONE_TEXT);
}

#[test_case(false, DONE_TEXT,  &DisplayRole::Done  ; "batch_subagent_success")]
#[test_case(true,  ERROR_TEXT, &DisplayRole::Error ; "batch_subagent_failure")]
fn batch_subagent_done_marker(is_error: bool, expected_text: &str, expected_role: &DisplayRole) {
    let mut app = app_with_subagent_id("batch1__0");
    finish_subagent(&mut app, "batch1__0", is_error);
    assert_eq!(app.chats[1].last_message_text(), expected_text);
    assert_eq!(app.chats[1].last_message_role(), Some(expected_role));
}

fn open_tasks_picker(app: &mut App) {
    for c in "/tasks".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter)));
}

#[test]
fn tasks_command_opens_picker() {
    let mut app = test_app();
    open_tasks_picker(&mut app);
    assert!(app.task_picker.is_open());
}

fn app_with_subagent_id(id: &str) -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::TextDelta { text: "x".into() },
        id,
        Some("research"),
    ));
    app
}

fn app_with_subagent() -> App {
    app_with_subagent_id("task1")
}

#[test]
fn picker_escape_restores_chat() {
    let mut app = app_with_subagent();
    assert_eq!(app.active_chat, 0);

    open_tasks_picker(&mut app);
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(!app.task_picker.is_open());
    assert_eq!(app.active_chat, 0);
}

#[test]
fn picker_enter_stays_at_navigated() {
    let mut app = app_with_subagent();

    open_tasks_picker(&mut app);
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(!app.task_picker.is_open());
    assert_eq!(app.active_chat, 1);
}

#[test]
fn global_ctrl_shortcuts_work_with_picker_open() {
    let mut app = app_with_subagent();
    assert_eq!(app.active_chat, 0);

    open_tasks_picker(&mut app);
    app.update(Msg::Key(kb::NEXT_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 1);

    app.update(Msg::Key(kb::PREV_CHAT.to_key_event()));
    assert_eq!(app.active_chat, 0);

    assert!(app.task_picker.is_open());
}

#[test]
fn compact_command_sets_streaming() {
    let mut app = test_app();
    let actions = app.execute_command(cmd("/compact"));
    assert!(matches!(&actions[0], Action::Compact));
    assert_eq!(app.status, Status::Streaming);
}

#[test]
fn compact_during_streaming_sends_to_agent() {
    let mut app = test_app();
    let (tx, rx) = flume::unbounded::<crate::AgentCommand>();
    app.cmd_tx = Some(tx);
    app.status = Status::Streaming;
    app.run_id = 1;

    let actions = app.execute_command(cmd("/compact"));
    assert!(actions.is_empty());
    assert_eq!(app.queue.len(), 1);
    assert!(matches!(app.queue[0], QueuedItem::Compact));
    assert!(app.queue.dispatched());
    assert!(rx.try_recv().is_ok(), "compact should be sent eagerly");
}

#[test]
fn delete_front_queue_item_drains_on_done() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue.push(queued_msg("first"));
    app.queue.push(queued_msg("second"));

    app.queue.remove(0);
    assert!(!app.queue.dispatched());

    let actions = app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    }));
    let has_send = actions.iter().any(|a| matches!(a, Action::SendMessage(_)));
    assert!(has_send, "agent should receive second item via Done drain");
    assert!(app.queue.is_empty());
}

#[test]
fn delete_only_queue_item_leaves_idle_on_done() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue.push(queued_msg("only"));

    app.queue.remove(0);

    let actions = app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    }));
    let has_send = actions.iter().any(|a| matches!(a, Action::SendMessage(_)));
    assert!(!has_send, "no SendMessage when queue is empty");
    assert_eq!(app.status, Status::Idle);
}

fn long_question_no_options() -> AgentEvent {
    let long = (0..20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    AgentEvent::QuestionPrompt {
        id: "q1".into(),
        questions: vec![QuestionInfo {
            question: long,
            header: String::new(),
            options: vec![],
            multiple: false,
        }],
    }
}

fn short_question_with_options() -> AgentEvent {
    AgentEvent::QuestionPrompt {
        id: "q2".into(),
        questions: vec![QuestionInfo {
            question: "Pick a DB".into(),
            header: "DB".into(),
            options: vec![QuestionOption {
                label: "PostgreSQL".into(),
                description: "Relational".into(),
            }],
            multiple: false,
        }],
    }
}

#[test]
fn question_routing_by_suitability() {
    let cases = [
        (long_question_no_options(), false, true),
        (short_question_with_options(), true, false),
    ];
    for (event, expect_form, expect_pending) in cases {
        let mut app = test_app();
        app.status = Status::Streaming;
        app.run_id = 1;
        app.update(agent_msg(event));
        assert_eq!(app.question_form.is_visible(), expect_form);
        assert_eq!(app.pending_input == PendingInput::Question, expect_pending);
    }
}

#[test]
fn pending_question_submit_routes_through_answer_tx() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let (tx, rx) = flume::unbounded();
    app.answer_tx = Some(tx);

    app.update(agent_msg(long_question_no_options()));
    assert_eq!(app.pending_input, PendingInput::Question);

    let actions = type_and_submit(&mut app, "my answer");
    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(rx.try_recv().unwrap(), "my answer");
}

#[test_case(PendingInput::Question  ; "question")]
#[test_case(PendingInput::AuthRetry ; "auth_retry")]
fn cancel_clears_pending_input(pending: PendingInput) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.pending_input = pending;
    cancel_app(&mut app);
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test_case(3  ; "scroll_up")]
#[test_case(-3 ; "scroll_down")]
fn scroll_disables_auto_scroll(delta: i32) {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    let actions = app.update(Msg::Scroll {
        column: 10,
        row: 10,
        delta,
    });
    assert!(actions.is_empty());
    assert!(!app.chats[0].auto_scroll());
}

#[test]
fn scroll_outside_msg_area_ignored() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: 10,
        row: 25,
        delta: 3,
    });
    assert!(app.chats[0].auto_scroll());
}

#[test_case(kb::SCROLL_TOP.to_key_event(), false ; "ctrl_g_disables_auto_scroll")]
#[test_case(kb::SCROLL_BOTTOM.to_key_event(), true  ; "ctrl_b_enables_auto_scroll")]
fn ctrl_g_scroll_shortcuts(key: KeyEvent, expected_auto_scroll: bool) {
    let mut app = test_app();
    app.active_chat().enable_auto_scroll();
    app.update(Msg::Key(key));
    assert_eq!(app.chats[0].auto_scroll(), expected_auto_scroll);
}

#[test]
fn mouse_drag_updates_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 20, 10));

    let state = app.selection_state.as_ref().unwrap();
    let (_, end) = state.sel.normalized();
    assert_eq!(end.row, 10);
    assert_eq!(end.col, 20);
}

#[test]
fn mouse_drag_clamps_to_area() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        100,
        50,
    ));

    let state = app.selection_state.as_ref().unwrap();
    let (_, end) = state.sel.normalized();
    assert_eq!(end.col, 79);
    assert_eq!(end.row, 19, "clamped to area bottom");
    assert!(
        state.edge_scroll.is_some(),
        "outside area triggers edge scroll"
    );
}

#[test_case(Rect::new(0, 2, 80, 20), (10, 12), (10, 1),  Some(EDGE_SCROLL_LINES)  ; "top_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (10, 22), Some(-EDGE_SCROLL_LINES) ; "bottom_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (20, 15), None                     ; "middle_no_scroll")]
#[test_case(Rect::new(0, 1, 80, 20), (10, 10), (10, 0),  Some(EDGE_SCROLL_LINES)  ; "above_area")]
#[test_case(Rect::new(0, 0, 80, 20), (10, 10), (10, 0),  Some(EDGE_SCROLL_LINES)  ; "first_row")]
#[test_case(Rect::new(0, 0, 80, 20), (10, 10), (10, 20), Some(-EDGE_SCROLL_LINES) ; "below_area")]
#[test_case(Rect::new(0, 0, 80, 20), (10, 10), (10, 19), Some(-EDGE_SCROLL_LINES) ; "last_row")]
#[test_case(Rect::new(0, 0, 80, 20), (10, 10), (10, 1),  None                     ; "interior")]
fn edge_scroll_direction(zone: Rect, down: (u16, u16), drag: (u16, u16), expected: Option<i32>) {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(
        MouseEventKind::Down(MouseButton::Left),
        down.0,
        down.1,
    ));
    app.update(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        drag.0,
        drag.1,
    ));

    let state = app.selection_state.as_ref().unwrap();
    assert_eq!(state.edge_scroll.as_ref().map(|es| es.dir), expected);
}

#[test]
fn mouse_up_clears_edge_scroll() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 2, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    assert!(app.selection_state.as_ref().unwrap().edge_scroll.is_some());

    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 1));
    let state = app.selection_state.as_ref().unwrap();
    assert!(state.edge_scroll.is_none());
}

#[test]
fn tick_edge_scroll_scrolls_continuously() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 2, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));

    let state = app.selection_state.as_mut().unwrap();
    state.edge_scroll.as_mut().unwrap().last_tick = Instant::now() - EDGE_SCROLL_INTERVAL * 2;
    app.tick_edge_scroll();
    assert!(!app.chats[0].auto_scroll());
}

#[test]
fn double_esc_cancels_flushes_and_fails_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta {
        text: "partial".into(),
    }));
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "t1".into(),
        tool: "bash",
        summary: "running".into(),
        annotation: None,
        input: None,
        output: None,
    }))));

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());

    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(matches!(&actions[0], Action::CancelAgent));
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.chats[0].in_progress_count(), 0);
}

#[test]
fn double_esc_idle_opens_rewind_picker() {
    let mut app = test_app();
    type_and_submit(&mut app, "hello");
    app.status = Status::Idle;
    app.run_id = 1;
    app.session.messages.push(Message::user("hello".into()));

    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.rewind_picker.is_open());
}

#[test]
fn double_esc_idle_no_user_turns_flashes_error() {
    let mut app = test_app();
    app.last_esc = Some(Instant::now());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.rewind_picker.is_open());
}

#[test]
fn edge_scroll_makes_app_animating() {
    let mut app = test_app();
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta { text: "x".into() }));
    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    }));
    assert!(!app.is_animating());
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    let state = app.selection_state.as_mut().unwrap();
    state.edge_scroll = Some(EdgeScroll {
        dir: 1,
        last_tick: Instant::now(),
    });
    assert!(app.is_animating());
}

#[test]
fn mouse_up_behavior() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 10));
    assert!(
        app.selection_state.as_ref().unwrap().copy_on_release,
        "non-empty selection sets copy flag"
    );

    app.selection_state.as_mut().unwrap().copy_on_release = false;
    app.selection_state = None;
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 5, 5));
    assert!(app.selection_state.is_none(), "empty selection is cleared");
}

#[test]
fn key_and_scroll_clear_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert!(app.selection_state.is_none(), "key press clears selection");

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(Msg::Scroll {
        column: 10,
        row: 10,
        delta: 3,
    });
    assert!(app.selection_state.is_none(), "scroll clears selection");
}

#[test]
fn form_submit_pushes_answer_to_chat() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let (tx, rx) = flume::unbounded();
    app.answer_tx = Some(tx);

    app.update(agent_msg(short_question_with_options()));
    assert!(app.question_form.is_visible());

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.question_form.is_visible());
    assert_eq!(app.chats[0].last_message_text(), "Pick a DB: PostgreSQL");
    assert!(rx.try_recv().is_ok());
}

#[test]
fn form_dismiss_does_not_push_to_chat() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let (tx, rx) = flume::unbounded();
    app.answer_tx = Some(tx);

    app.update(agent_msg(short_question_with_options()));
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.question_form.is_visible());
    assert_eq!(app.chats[0].last_message_text(), "");
    assert_eq!(rx.try_recv().unwrap(), "");
}

#[test_case(true  ; "non_empty")]
#[test_case(false ; "empty")]
fn queue_command_sets_focus(has_queue: bool) {
    let mut app = if has_queue {
        app_with_queued_message()
    } else {
        test_app()
    };
    app.execute_command(cmd("/queue"));
    assert_eq!(app.queue.focus().is_some(), has_queue);
}

#[test_case(KeyCode::Up,   0, 0 ; "up_at_top_clamps")]
#[test_case(KeyCode::Down, 1, 1 ; "down_at_bottom_clamps")]
fn queue_boundary_clamps(key_code: KeyCode, initial_focus: usize, expected: usize) {
    let mut app = app_with_queued_message();
    app.queue.push(queued_msg("second"));
    app.queue.set_focus_at(initial_focus);

    app.update(Msg::Key(key(key_code)));
    assert_eq!(app.queue.focus(), Some(expected));
}

#[test]
fn queue_enter_removes_selected() {
    let mut app = app_with_queued_message();
    app.queue.push(queued_msg("second"));
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.queue.len(), 1);
    match &app.queue[0] {
        QueuedItem::Message(input) => assert_eq!(input.text, "second"),
        _ => panic!("expected Message variant"),
    }
    assert_eq!(app.queue.focus(), Some(0));
}

#[test]
fn queue_enter_deletes_last_unfocuses() {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.queue.is_empty());
    assert!(app.queue.focus().is_none());
}

#[test]
fn queue_esc_unfocuses_without_removing() {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.queue.focus().is_none());
    assert_eq!(app.queue.len(), 1);
}

#[test_case(false ; "unfocused")]
#[test_case(true  ; "focused_on_second")]
fn ctrl_q_pops_front(focused: bool) {
    let mut app = app_with_queued_message();
    app.queue.push(queued_msg("second"));
    if focused {
        app.queue.set_focus_at(1);
    }

    app.update(Msg::Key(kb::POP_QUEUE.to_key_event()));
    assert_eq!(app.queue.len(), 1);
    match &app.queue[0] {
        QueuedItem::Message(input) => assert_eq!(input.text, "second"),
        _ => panic!("expected Message variant"),
    }
    assert_eq!(app.queue.focus(), if focused { Some(0) } else { None });
}

#[test_case(cancel_app as fn(&mut App) ; "cancel")]
#[test_case(error_app as fn(&mut App)  ; "error")]
fn clears_queue_focus_on_terminate(terminate: fn(&mut App)) {
    let mut app = app_with_queued_message();
    app.queue.set_focus_at(0);
    terminate(&mut app);
    assert!(app.queue.focus().is_none());
}

#[test]
fn compact_fifo_with_messages() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue.push(queued_msg("first"));
    app.queue.push(QueuedItem::Compact);

    let actions = app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    }));
    assert!(matches!(&actions[0], Action::SendMessage(i) if i.message == "first"));

    let actions = app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    }));
    assert!(matches!(&actions[0], Action::Compact));
    assert!(app.queue.is_empty());
}

#[test]
fn stale_events_ignored_after_run_id_increment() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    cancel_app(&mut app);
    assert_eq!(app.run_id, 2);

    let actions = type_and_submit(&mut app, "new prompt");
    assert!(matches!(&actions[0], Action::SendMessage(i) if i.message == "new prompt"));
    assert_eq!(app.run_id, 3);

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "stale text".into(),
        },
        1,
    ));
    assert_eq!(app.chats[0].last_message_text(), "new prompt");

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "new text".into(),
        },
        3,
    ));
    app.chats[0].flush();
    assert_eq!(app.chats[0].last_message_text(), "new text");
}

#[test]
fn stale_done_does_not_drain_queue() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    cancel_app(&mut app);
    app.queue.push(queued_msg("next"));

    app.update(agent_msg_with_run_id(
        AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            stop_reason: None,
        },
        1,
    ));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.status, Status::Idle);
}

#[test]
fn zone_at_returns_correct_zone() {
    let mut app = test_app();
    let msg = Rect::new(0, 0, 80, 15);
    let input = Rect::new(0, 15, 80, 5);
    let status = Rect::new(0, 20, 80, 1);
    set_zone(&mut app, SelectionZone::Messages, msg);
    set_zone(&mut app, SelectionZone::Input, input);
    set_zone(&mut app, SelectionZone::StatusBar, status);

    assert_eq!(app.zone_at(5, 10).unwrap().zone, SelectionZone::Messages);
    assert_eq!(app.zone_at(16, 10).unwrap().zone, SelectionZone::Input);
    assert_eq!(app.zone_at(20, 10).unwrap().zone, SelectionZone::StatusBar);
    assert!(app.zone_at(22, 10).is_none());
}

#[test]
fn mouse_down_in_input_creates_input_zone_selection() {
    let mut app = test_app();
    let input = Rect::new(0, 15, 80, 5);
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 15));
    set_zone(&mut app, SelectionZone::Input, input);

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 16));
    let state = app.selection_state.as_ref().unwrap();
    assert_eq!(state.sel.zone, SelectionZone::Input);
    assert_eq!(state.sel.area, input);
}

#[test]
fn resolve_or_create_chat_sets_model_id_and_annotation() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "task1".into(),
        tool: "task",
        summary: "research".into(),
        annotation: None,
        input: None,
        output: None,
    }))));

    app.update(subagent_msg_with_model(
        AgentEvent::TextDelta { text: "hi".into() },
        "task1",
        "research",
        "anthropic/claude-sonnet-4-20250514",
    ));

    assert_eq!(app.chats.len(), 2);
    assert_eq!(
        app.chats[1].model_id.as_deref(),
        Some("anthropic/claude-sonnet-4-20250514")
    );
}

#[test_case(|app: &mut App| { app.execute_command(cmd("/help")); } ; "slash_help")]
#[test_case(|app: &mut App| { app.update(Msg::Key(kb::HELP.to_key_event())); } ; "ctrl_slash")]
fn help_toggles_modal(toggle: fn(&mut App)) {
    let mut app = test_app();
    assert!(!app.help_modal.is_open());

    toggle(&mut app);
    assert!(app.help_modal.is_open());

    toggle(&mut app);
    assert!(!app.help_modal.is_open());
}

#[test]
fn help_modal_consumes_keys_and_esc_closes() {
    let mut app = test_app();
    app.update(Msg::Key(kb::HELP.to_key_event()));

    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
    assert_eq!(app.input_box.buffer.value(), "");

    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.help_modal.is_open());
}

#[test_case(
    |_: &mut App| {},
    &[KeybindContext::General, KeybindContext::Editing],
    &[KeybindContext::Streaming]
    ; "idle"
)]
#[test_case(
    |app: &mut App| { app.status = Status::Streaming; },
    &[KeybindContext::General, KeybindContext::Streaming, KeybindContext::Editing],
    &[]
    ; "streaming"
)]
#[test_case(
    |app: &mut App| { app.plan_form.open(); },
    &[KeybindContext::FormInput],
    &[KeybindContext::Editing]
    ; "plan_form"
)]
#[test_case(
    |app: &mut App| { app.status = Status::Streaming; app.run_id = 1; app.queue.push(queued_msg("q")); app.queue.set_focus_at(0); },
    &[KeybindContext::QueueFocus],
    &[KeybindContext::Editing]
    ; "queue_focus"
)]
#[test_case(
    |app: &mut App| { open_tasks_picker(app); },
    &[KeybindContext::TaskPicker],
    &[KeybindContext::Editing]
    ; "task_picker"
)]
#[test_case(
    |app: &mut App| {
        app.session.messages.push(Message::user("test".into()));
        app.open_rewind_picker();
    },
    &[KeybindContext::RewindPicker],
    &[KeybindContext::Editing]
    ; "rewind_picker"
)]
fn active_contexts(setup: fn(&mut App), expected: &[KeybindContext], absent: &[KeybindContext]) {
    let mut app = test_app();
    setup(&mut app);
    let contexts = app.active_keybind_contexts();
    for ctx in expected {
        assert!(contexts.contains(ctx), "{ctx:?} should be present");
    }
    for ctx in absent {
        assert!(!contexts.contains(ctx), "{ctx:?} should be absent");
    }
}

#[test_case("exit"    ; "bare_exit")]
#[test_case("  exit " ; "exit_with_whitespace")]
fn submit_exit_quits(input: &str) {
    let mut app = test_app();
    let actions = app.handle_submit(Submission {
        text: input.into(),
        images: vec![],
    });
    assert!(app.should_quit);
    assert!(matches!(&actions[0], Action::Quit));
}

#[test_case(0, "hello"            ; "no_images")]
#[test_case(1, "hello [1 image]"  ; "one_image")]
#[test_case(3, "hello [3 images]" ; "multiple_images")]
fn format_with_images_label(count: usize, expected: &str) {
    assert_eq!(format_with_images("hello", count), expected);
}

#[test]
fn slash_exit_command_quits() {
    let mut app = test_app();
    let actions = app.execute_command(cmd("/exit"));
    assert!(app.should_quit);
    assert!(matches!(&actions[0], Action::Quit));
}

#[test]
fn yolo_toggle() {
    let mut app = test_app();
    assert!(!app.permissions.is_yolo());
    app.execute_command(cmd("/yolo"));
    assert!(app.permissions.is_yolo());
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.contains("enabled"), "flash={flash:?}");
    app.execute_command(cmd("/yolo"));
    assert!(!app.permissions.is_yolo());
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.contains("disabled"), "flash={flash:?}");
}

#[test_case("/tmp",                     "cd /tmp" ; "absolute_path")]
#[test_case("/nonexistent_path_12345",  "cd: "    ; "nonexistent_flashes_error")]
fn cd_flash_message(args: &str, expected_prefix: &str) {
    let mut app = test_app();
    app.execute_command(ParsedCommand {
        name: "/cd",
        args: args.into(),
    });
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.starts_with(expected_prefix), "flash={flash:?}");
}

#[test]
fn cd_updates_session_cwd() {
    let mut app = test_app();
    app.execute_command(ParsedCommand {
        name: "/cd",
        args: "/tmp".into(),
    });
    let canonical = std::fs::canonicalize("/tmp").unwrap();
    assert_eq!(app.session.cwd, canonical.to_string_lossy());
}

#[test]
fn typed_slash_command_executes() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, "/help");
    assert!(actions.is_empty());
    assert!(app.help_modal.is_open());
}

#[test]
fn slash_noncommand_sends_as_prompt() {
    let mut app = test_app();
    let actions = type_and_submit(&mut app, "/nonexistent");
    assert!(app.status_bar.flash_text().is_none());
    assert!(actions.iter().any(|a| matches!(a, Action::SendMessage(..))));
}

fn build_rewind_app() -> App {
    let mut app = test_app();
    use maki_providers::{ContentBlock, Message, Role};

    app.session.messages = vec![
        Message::user("first prompt".into()),
        Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "response 1".into(),
                },
                ContentBlock::ToolUse {
                    id: "tool-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                },
            ],
            ..Default::default()
        },
        Message::user("second prompt".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "response 2".into(),
            }],
            ..Default::default()
        },
        Message::user("third prompt".into()),
    ];
    app.session
        .tool_outputs
        .insert("tool-1".into(), ToolOutput::Plain("output".into()));
    app
}

#[test]
fn rewind_to_middle_truncates_and_populates_input() {
    let mut app = build_rewind_app();
    let old_run_id = app.run_id;
    let entry = crate::components::rewind_picker::RewindEntry {
        turn_index: 2,
        prompt_preview: "2: second".into(),
        prompt_text: "second prompt".into(),
    };
    let actions = app.rewind_to(entry);

    assert_eq!(app.session.messages.len(), 2);
    assert!(app.session.tool_outputs.contains_key("tool-1"));
    assert_eq!(app.input_box.buffer.value(), "second prompt");
    assert_eq!(app.run_id, old_run_id + 1);

    let Action::LoadSession(ref loaded) = actions[0] else {
        panic!("expected LoadSession");
    };
    assert_eq!(loaded.messages.len(), 2);
    assert!(loaded.tool_outputs.contains_key("tool-1"));
}

#[test]
fn rewind_to_first_turn_clears_everything() {
    let mut app = build_rewind_app();
    app.token_usage.input = 500;
    app.token_usage.output = 200;
    let entry = crate::components::rewind_picker::RewindEntry {
        turn_index: 0,
        prompt_preview: "1: first".into(),
        prompt_text: "first prompt".into(),
    };
    let actions = app.rewind_to(entry);

    assert!(app.session.messages.is_empty());
    assert!(!app.session.tool_outputs.contains_key("tool-1"));
    assert_eq!(app.token_usage.input, 500);
    assert_eq!(app.token_usage.output, 200);
    assert!(matches!(&actions[0], Action::LoadSession(_)));
}

#[test_case(Duration::ZERO,          true  ; "keeps_fresh_error")]
#[test_case(Duration::from_secs(60), false ; "clears_stale_error")]
fn tick_error_expiry(age: Duration, expect_error: bool) {
    let mut app = test_app();
    app.status = Status::Error {
        message: "fail".into(),
        since: Instant::now() - age,
    };
    app.tick_error_expiry();
    assert_eq!(matches!(app.status, Status::Error { .. }), expect_error);
}

#[test]
fn retry_clears_in_progress_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolPending {
        id: "t1".into(),
        name: "bash".into(),
    }));
    assert_eq!(app.chats[0].in_progress_count(), 1);

    app.update(agent_msg(AgentEvent::Retry {
        attempt: 1,
        message: "overloaded".into(),
        delay_ms: 1000,
    }));
    assert_eq!(app.chats[0].in_progress_count(), 0);
    assert!(app.retry_info.is_some());
}

#[test]
fn retry_clears_subagent_in_progress_tools() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg(
        AgentEvent::ToolPending {
            id: "st1".into(),
            name: "bash".into(),
        },
        "task1",
        Some("research"),
    ));
    assert_eq!(app.chats.len(), 2);
    assert_eq!(app.chats[1].in_progress_count(), 1);

    app.update(subagent_msg(
        AgentEvent::Retry {
            attempt: 1,
            message: "overloaded".into(),
            delay_ms: 1000,
        },
        "task1",
        Some("research"),
    ));
    assert_eq!(app.chats[1].in_progress_count(), 0);
    assert!(app.retry_info.is_none());
}

#[test]
fn auth_required_sets_pending_auth_retry() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::AuthRequired));
    assert_eq!(app.pending_input, PendingInput::AuthRetry);
    assert_eq!(app.chats[0].last_message_text(), AUTH_EXPIRED_MSG,);
}

#[test]
fn auth_retry_submit_sends_empty_answer() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let (tx, rx) = flume::unbounded();
    app.answer_tx = Some(tx);

    app.update(agent_msg(AgentEvent::AuthRequired));
    assert_eq!(app.pending_input, PendingInput::AuthRetry);

    let actions = type_and_submit(&mut app, "ignored");
    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(rx.try_recv().unwrap(), "");
}

#[test_case(42, false ; "restores_scroll_position")]
#[test_case(0,  true  ; "restores_auto_scroll")]
fn search_escape_restores_scroll(scroll_top: u16, auto_scroll: bool) {
    let mut app = test_app();
    app.active_chat().restore_scroll(scroll_top, auto_scroll);

    app.update(Msg::Key(kb::SEARCH.to_key_event()));
    app.update(Msg::Key(key(KeyCode::Esc)));

    assert!(!app.search_modal.is_open());
    assert_eq!(app.active_chat().scroll_top(), scroll_top);
    assert_eq!(app.active_chat().auto_scroll(), auto_scroll);
}

#[test_case(Mode::Plan, true,  AgentMode::Plan("p.md".into()), None ; "plan_mode_at_drain")]
#[test_case(Mode::Build, false, AgentMode::Build,               None ; "build_mode_at_drain")]
fn done_drains_queued_message_with_current_mode(
    mode: Mode,
    set_plan: bool,
    expected_mode: AgentMode,
    expected_plan: Option<&Path>,
) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue.push(queued_msg("queued"));
    app.mode = mode;
    if set_plan {
        app.plan = PlanState::with_path(PathBuf::from("p.md"), false);
    }
    let actions = app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    }));
    let Action::SendMessage(ref input) = actions[0] else {
        panic!("expected SendMessage");
    };
    assert_eq!(input.mode, expected_mode);
    assert_eq!(input.pending_plan.as_deref(), expected_plan);
    assert!(app.queue.is_empty());
    assert_eq!(app.status, Status::Streaming);
}

#[test]
fn mcp_command_opens_picker() {
    let mut app = test_app();
    app.execute_command(cmd("/mcp"));
    assert!(app.mcp_picker.is_open());
}

#[test]
fn mcp_toggle_dispatches_action() {
    use maki_agent::{McpServerInfo, McpServerStatus};
    use std::path::PathBuf;

    let mut app = test_app();
    app.mcp_picker = McpPicker::new(Arc::new(ArcSwap::from_pointee(vec![McpServerInfo {
        name: "test-srv".into(),
        transport_kind: "stdio",
        tool_count: 2,
        status: McpServerStatus::Running,
        config_path: PathBuf::from("/tmp/config.toml"),
    }])));
    app.execute_command(cmd("/mcp"));

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(
        &actions[0],
        Action::ToggleMcp(name, false) if name == "test-srv"
    ));
}

#[test_case(
    |app: &mut App| { app.plan_form.open(); },
    ""
    ; "consumed_by_plan_form"
)]
#[test_case(
    |app: &mut App| { open_tasks_picker(app); },
    ""
    ; "routed_to_open_picker"
)]
#[test_case(
    |app: &mut App| { app.update(Msg::Key(kb::SEARCH.to_key_event())); },
    ""
    ; "routed_to_search_modal"
)]
#[test_case(
    |_: &mut App| {},
    "pasted"
    ; "falls_through_to_input"
)]
fn paste_routing(setup: fn(&mut App), expected_input: &str) {
    let mut app = test_app();
    setup(&mut app);
    app.update(Msg::Paste("pasted".into()));
    assert_eq!(app.input_box.buffer.value(), expected_input);
}

#[test_case(None,                                 true ; "no_plan")]
#[test_case(Some(("/tmp/plan.md", false)),         true ; "plan_not_written")]
#[test_case(Some(("/tmp/plan.md", true)),          false ; "plan_written")]
fn open_editor(setup: Option<(&str, bool)>, expect_flash: bool) {
    let mut app = test_app();
    let plan_path = setup.map(|(p, w)| {
        let pb = PathBuf::from(p);
        app.plan = PlanState::with_path(pb.clone(), w);
        pb
    });
    let actions = app.update(Msg::Key(kb::OPEN_EDITOR.to_key_event()));
    if expect_flash {
        assert!(actions.is_empty());
        assert_eq!(app.status_bar.flash_text().unwrap(), FLASH_NO_PLAN);
    } else {
        let expected = plan_path.unwrap();
        assert!(matches!(&actions[..], [Action::OpenEditor(p)] if p == &expected));
    }
}

#[test]
fn btw_empty_flashes_error() {
    let mut app = test_app();
    let actions = app.execute_command(ParsedCommand {
        name: "/btw",
        args: String::new(),
    });
    assert!(actions.is_empty());
    assert_eq!(
        app.status_bar.flash_text().unwrap(),
        "Usage: /btw <question>"
    );
}

#[test]
fn btw_with_question_returns_action() {
    let mut app = test_app();
    let actions = app.execute_command(ParsedCommand {
        name: "/btw",
        args: "what is rust?".into(),
    });
    assert!(matches!(&actions[..], [Action::Btw(q)] if q == "what is rust?"));
}

#[test]
fn btw_modal_key_routing_and_animation() {
    let mut app = test_app();
    let (_tx, rx) = flume::bounded(1);
    app.btw_modal.open("test", rx);

    assert!(app.btw_modal.is_animating());

    let actions = app.update(Msg::Key(key(KeyCode::Char('x'))));
    assert!(actions.is_empty());
    assert!(app.btw_modal.is_open());
    assert_eq!(app.input_box.buffer.value(), "");

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());
    assert!(!app.btw_modal.is_open());
    assert!(!app.btw_modal.is_animating());
}

#[test]
fn overlay_zone_click_gating() {
    let mut app = test_app();
    let msg = Rect::new(0, 0, 80, 15);
    let overlay = Rect::new(10, 3, 60, 10);
    set_zone(&mut app, SelectionZone::Messages, msg);
    set_zone(&mut app, SelectionZone::Overlay, overlay);
    app.help_modal.toggle();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 1));
    assert!(app.selection_state.is_none());

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 20, 5));
    let state = app.selection_state.as_ref().unwrap();
    assert_eq!(state.sel.zone, SelectionZone::Overlay);
}

fn streaming_app_with_history() -> App {
    use maki_providers::{ContentBlock, Message, Role};
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let history = vec![
        Message::user("hello".into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "world".into(),
            }],
            ..Default::default()
        },
    ];
    app.shared_history = Some(Arc::new(ArcSwap::from_pointee(history)));
    app
}

#[test_case(
    AgentEvent::Done { usage: TokenUsage::default(), num_turns: 1, stop_reason: None } ; "stale_done_saves_session"
)]
#[test_case(
    AgentEvent::Error { message: "timeout".into() } ; "stale_error_saves_session"
)]
fn stale_terminal_event_after_cancel_saves_session(event: AgentEvent) {
    let mut app = streaming_app_with_history();
    let old_run_id = app.run_id;
    cancel_app(&mut app);
    assert_ne!(app.run_id, old_run_id);
    assert!(app.session.messages.is_empty());

    app.update(agent_msg_with_run_id(event, old_run_id));
    assert_eq!(app.session.messages.len(), 2);
}

#[test]
fn stale_non_terminal_event_does_not_save_session() {
    let mut app = streaming_app_with_history();
    let old_run_id = app.run_id;
    cancel_app(&mut app);

    app.update(agent_msg_with_run_id(
        AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::user(String::new()),
            usage: TokenUsage::default(),
            model: "mock".into(),
            context_size: None,
        })),
        old_run_id,
    ));
    assert!(app.session.messages.is_empty());
}

#[test]
fn error_event_matching_run_id_saves_session() {
    let mut app = streaming_app_with_history();
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
    assert_eq!(app.session.messages.len(), 2);
}

// --- Plan form integration tests ---

fn done_event() -> Msg {
    agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    })
}

fn plan_app() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.mode = Mode::Plan;
    app.plan = PlanState::with_path(PathBuf::from("test-plan.md"), true);
    app
}

#[test_case(Mode::Plan,  true,  true  ; "plan_mode_written_opens_form")]
#[test_case(Mode::Plan,  false, false ; "plan_mode_unwritten_no_form")]
#[test_case(Mode::Build, false, false ; "build_mode_no_form")]
fn done_plan_form_visibility(mode: Mode, written: bool, expect_form: bool) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.mode = mode;
    if mode == Mode::Plan {
        app.plan = PlanState::with_path(PathBuf::from("test-plan.md"), written);
    }
    app.update(done_event());
    assert_eq!(app.plan_form.is_visible(), expect_form);
}

#[test]
fn done_with_queued_messages_no_form() {
    let mut app = plan_app();
    app.queue.push(queued_msg("queued"));
    app.update(done_event());
    assert!(!app.plan_form.is_visible());
    assert_eq!(app.status, Status::Streaming);
}

#[test]
fn plan_form_clear_and_implement() {
    let mut app = plan_app();
    app.update(done_event());
    assert!(app.plan_form.is_visible());

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.plan_form.is_visible());
    assert_eq!(app.mode, Mode::Build);
    assert!(
        matches!(&actions[..], [Action::NewSession, Action::SendMessage(input)] if input.message == "Implement the plan at `test-plan.md`.")
    );
}

#[test]
fn plan_form_implement_keeps_context() {
    let mut app = plan_app();
    app.update(done_event());

    app.update(Msg::Key(key(KeyCode::Down)));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.plan_form.is_visible());
    assert_eq!(app.mode, Mode::Build);
    assert_eq!(actions.len(), 1);
    assert!(
        matches!(&actions[0], Action::SendMessage(input) if input.message == "Implement the plan at `test-plan.md`.")
    );
}

#[test]
fn plan_form_continue_closes_form() {
    let mut app = plan_app();
    app.update(done_event());

    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Down)));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.plan_form.is_visible());
    assert_eq!(app.mode, Mode::Plan);
    assert!(actions.is_empty());
}

#[test]
fn plan_form_open_editor() {
    let mut app = plan_app();
    app.update(done_event());

    let actions = app.update(Msg::Key(kb::OPEN_EDITOR.to_key_event()));
    assert!(app.plan_form.is_visible());
    assert!(matches!(&actions[..], [Action::OpenEditor(p)] if p == Path::new("test-plan.md")));
}

#[test]
fn plan_form_dismiss_on_esc() {
    let mut app = plan_app();
    app.update(done_event());

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.plan_form.is_visible());
    assert!(actions.is_empty());
}

#[test]
fn close_all_overlays_closes_plan_form() {
    let mut app = test_app();
    app.plan_form.open();
    assert!(app.plan_form.is_visible());

    app.close_all_overlays();
    assert!(!app.plan_form.is_visible());
}

#[test]
fn reset_session_closes_plan_form() {
    let mut app = plan_app();
    app.update(done_event());
    assert!(app.plan_form.is_visible());

    app.reset_session();
    assert!(!app.plan_form.is_visible());
}

#[test]
fn ctrl_c_closes_overlay_instead_of_quitting() {
    let mut app = test_app();
    app.help_modal.toggle();
    assert!(app.help_modal.is_open());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(!app.should_quit);
    assert!(!app.help_modal.is_open());
    assert!(actions.is_empty());
}

#[test]
fn bash_prefix_overrides_mode() {
    let mut app = test_app();

    app.input_box.set_input("! ls".into());
    assert_eq!(&*app.mode_label().0, "[BASH]");

    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(
        app.mode,
        Mode::Build,
        "tab must not toggle while bash prefix present"
    );

    app.input_box.set_input("ls".into());
    assert_eq!(&*app.mode_label().0, "[BUILD]");
}
