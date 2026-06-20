use super::*;
use crate::agent::shared_queue;
use crate::chat::{CANCELLED_TEXT, DONE_TEXT, ERROR_TEXT};
use crate::components::command::ParsedCommand;
use crate::components::keybindings::{KeybindContext, key as kb};
use crate::components::{ExitRequest, key, test_model};
use crate::selection::{SelectableZone, SelectionState, SelectionZone};
use arc_swap::ArcSwap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    ImageMediaType, McpConfigErrors, McpServerInfo, McpServerStatus, McpSnapshot,
    McpSnapshotReader, ToolDoneEvent, ToolOutput, ToolStartEvent, TurnCompleteEvent,
};
use maki_config::{PermissionsConfig, UiConfig};
use maki_lua::{HintReader, KeymapReader, LuaCommandReader};
use maki_providers::{ContentBlock, Role, TokenUsage};
use maki_storage::sessions::StoredThinking;
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
    let writer = Arc::new(StorageWriter::new(StateDir::from_path(env::temp_dir())));
    let permissions = Arc::new(PermissionManager::new(
        PermissionsConfig {
            allow_all: false,
            rules: vec![],
        },
        PathBuf::from("/tmp"),
    ));
    let model = test_model();
    let mut app = App::new(
        &model,
        AppSession::new("test-model", "/tmp/test"),
        StateDir::from_path(env::temp_dir()),
        Arc::new(ArcSwapOption::empty()),
        McpSnapshotReader::empty(),
        McpConfigErrors::new(PathBuf::new()),
        LuaCommandReader::empty(),
        KeymapReader::empty(),
        HintReader::empty(),
        writer,
        UiConfig::default(),
        100,
        permissions,
        Arc::from([]),
    );
    let (shared_queue, _rx) = shared_queue::queue();
    app.queue.set_shared(shared_queue);
    app
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
    subagent_info_with_tx(parent_id, name, None)
}

fn subagent_info_with_tx(
    parent_id: &str,
    name: &str,
    answer_tx: Option<flume::Sender<String>>,
) -> SubagentInfo {
    SubagentInfo {
        parent_tool_use_id: parent_id.into(),
        name: name.into(),
        prompt: None,
        model: None,
        answer_tx,
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
    assert!(matches!(&actions[0], Action::SendMessage(s) if s.message == "hi"));
    assert_eq!(app.status, Status::Streaming);
    // Regression check: the bubble has to be on screen the same frame we
    // submit, otherwise it briefly sits one row too high before snapping down.
    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::User),
    );
    assert_eq!(app.main_chat().last_message_text(), "hi");
}

fn with_text(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    app.update(Msg::Key(key(KeyCode::Char('i'))));
}

fn with_image(app: &mut App) {
    let img = ImageSource::new(ImageMediaType::Png, Arc::from("dGVzdA=="));
    app.input_box.attach_image(img);
}

#[test_case(with_text as fn(&mut App)  ; "clears_text")]
#[test_case(with_image as fn(&mut App) ; "clears_image")]
fn ctrl_c_clears_nonempty_input(setup: fn(&mut App)) {
    let mut app = test_app();
    setup(&mut app);
    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(actions.is_empty());
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(app.input_box.is_empty());
}

#[test]
fn ctrl_c_quits_when_input_empty() {
    let mut app = test_app();
    app.status = Status::Idle;
    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::Success);
    assert!(matches!(&actions[0], Action::Quit));
}

#[test_case(AgentEvent::Done { usage: TokenUsage::default(), num_turns: 1, stop_reason: None }, ExitRequest::Success ; "done_exits_success")]
#[test_case(AgentEvent::Error { message: "boom".into() }, ExitRequest::Error ; "error_exits_error")]
fn exit_on_done_flag_triggers_exit(event: AgentEvent, expected: ExitRequest) {
    let mut app = test_app();
    app.exit_on_done = true;
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(event));
    assert_eq!(app.exit_request, expected);
}

#[test]
fn toggle_mode_state_machine() {
    let tab = |app: &mut App| app.update(Msg::Key(key(KeyCode::Tab)));

    let mut app = test_app();
    assert_eq!(app.state.mode, Mode::Build);

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    let first_path = app.state.plan.path().unwrap().to_path_buf();
    assert!(first_path.to_str().unwrap().contains("plans"));

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Build);
    assert!(!app.state.plan.is_ready());

    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.state.plan.path().unwrap(), first_path);

    app.state.plan.mark_ready();
    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Build);
    assert!(app.state.plan.is_ready());
    assert_eq!(app.state.plan.path().unwrap(), first_path);

    app.state.mode = Mode::Build;
    app.status = Status::Streaming;
    app.run_id = 1;
    tab(&mut app);
    assert_eq!(app.state.mode, Mode::Plan);
    assert_eq!(app.state.plan.path().unwrap(), first_path);
}

#[test_case(ToolOutput::Plain("wrote 100 bytes to /tmp/plans/test.md".into()), Some("/tmp/plans/test.md".into()), true  ; "write_matching")]
#[test_case(ToolOutput::Diff { path: "/tmp/plans/test.md".into(), before: String::new(), after: String::new(), summary: String::new() }, None, true  ; "edit_matching")]
#[test_case(ToolOutput::Plain("wrote 100 bytes to /tmp/other.rs".into()), Some("/tmp/other.rs".into()), false ; "write_non_matching")]
fn tool_done_transitions_plan_to_ready(
    output: ToolOutput,
    written_path: Option<String>,
    expect_ready: bool,
) {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("/tmp/plans/test.md"));
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output,
        is_error: false,
        annotation: None,
        written_path,
    }))));

    assert_eq!(app.state.plan.is_ready(), expect_ready);
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

#[test_case(Status::Idle      ; "idle")]
#[test_case(Status::Streaming ; "streaming")]
fn paste_works_regardless_of_status(status: Status) {
    let mut app = test_app();
    app.status = status;
    app.update(Msg::Paste("pasted".into()));
    assert_eq!(app.input_box.buffer.value(), "pasted");
}

#[test_case("a\rb\rc",       "a\nb\nc"       ; "bare_cr")]
#[test_case("a\r\nb\r\nc",   "a\nb\nc"       ; "crlf")]
#[test_case("a\r\nb\rc\nd",  "a\nb\nc\nd"    ; "mixed")]
fn paste_normalizes_line_endings(input: &str, expected: &str) {
    let mut app = test_app();
    app.update(Msg::Paste(input.into()));
    assert_eq!(app.input_box.buffer.value(), expected);
}

#[test]
fn paste_file_path_triggers_image_load() {
    let mut app = test_app();
    app.update(Msg::Paste("file:///tmp/nonexistent.png".into()));
    assert!(!app.image_paste_rx.is_empty());
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
}

#[test]
fn queue_item_consumed_pushes_deferred_user_message() {
    let mut app = test_app();
    type_and_submit(&mut app, "first");
    assert_eq!(app.main_chat().message_count(), 1);

    app.queue_and_notify(queued_msg("queued"));
    assert_eq!(
        app.main_chat().message_count(),
        1,
        "queueing while streaming must not render the bubble yet",
    );

    app.update(agent_msg_with_run_id(
        AgentEvent::QueueItemConsumed {
            text: "queued".into(),
            image_count: 0,
        },
        app.run_id,
    ));

    assert_eq!(app.main_chat().message_count(), 2);
    assert_eq!(app.main_chat().last_message_text(), "queued");
    assert_eq!(
        app.main_chat().last_message_role(),
        Some(&DisplayRole::User),
    );
}

#[test_case(error_app as fn(&mut App) ; "error")]
#[test_case(cancel_app as fn(&mut App) ; "cancel")]
fn clears_queue(terminate: fn(&mut App)) {
    let mut app = app_with_queued_message();
    terminate(&mut app);
    assert!(app.queue.is_empty());
}

fn queued_msg(text: &str) -> QueuedMessage {
    QueuedMessage {
        text: text.into(),
        images: vec![],
    }
}

fn app_with_queued_message() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("queued"));
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

fn cmd(name: &str) -> ParsedCommand {
    ParsedCommand {
        name: name.to_string(),
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
fn reset_session_clears_plan() {
    let mut app = test_app();
    app.state.token_usage.input = 500;
    app.chats[0].context_size = 1000;
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Ready(PathBuf::from("plan.md"));
    app.queue_and_notify(queued_msg("q"));
    app.queue.set_focus_at(0);
    app.help_modal.toggle();
    let (_tx, rx) = flume::bounded::<crate::components::btw_modal::BtwEvent>(1);
    app.btw_modal.open("q", rx);
    let actions = app.reset_session();
    assert!(matches!(&actions[0], Action::NewSession));
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.state.token_usage.input, 0);
    assert_eq!(app.chats[0].context_size, 0);
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan, PlanState::None);
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
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("old-plan.md"));
    app.reset_session();
    assert_eq!(app.state.mode, Mode::Plan);
    assert!(app.state.plan.path().is_some());
    assert_ne!(app.state.plan.path(), Some(Path::new("old-plan.md")));
}

#[test]
fn reset_session_clears_drafting_plan_in_build_mode() {
    let mut app = test_app();
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Drafting(PathBuf::from("leftover.md"));
    app.reset_session();
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan, PlanState::None);
}

#[test]
fn load_session_clears_plan() {
    let tmp = TempDir::new().unwrap();
    let dir = StateDir::from_path(tmp.path().to_path_buf());
    let writer = Arc::new(StorageWriter::new(StateDir::from_path(
        tmp.path().to_path_buf(),
    )));
    let model = test_model();
    let mut app = App::new(
        &model,
        AppSession::new("test-model", "/tmp/test"),
        dir,
        Arc::new(ArcSwapOption::empty()),
        McpSnapshotReader::empty(),
        McpConfigErrors::new(PathBuf::new()),
        LuaCommandReader::empty(),
        KeymapReader::empty(),
        HintReader::empty(),
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
        Arc::from([]),
    );
    app.state
        .session
        .messages
        .push(Message::user("test".into()));
    app.state.session.save(&app.storage).unwrap();
    let id = app.state.session.id.clone();
    app.state.mode = Mode::Build;
    app.state.plan = PlanState::Ready(PathBuf::from("old-plan.md"));
    app.load_session(id);
    assert_eq!(app.state.mode, Mode::Build);
    assert_eq!(app.state.plan.path(), None);
}

#[test]
fn tab_in_palette_completes_command() {
    let mut app = test_app();
    type_slash(&mut app);
    assert!(app.command_palette.is_active());

    app.update(Msg::Key(key(KeyCode::Tab)));
    let val = app.input_box.buffer.value();
    assert!(val.starts_with('/'));
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
fn subagent_prompt_shown_once_and_not_duplicated() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(subagent_msg_with_prompt(
        AgentEvent::TextDelta { text: "a".into() },
        "task1",
        Some("research"),
        Some("Find all TODO comments"),
    ));
    assert_eq!(app.chats[1].message_count(), 1);
    assert_eq!(app.chats[1].last_message_text(), "Find all TODO comments");

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

    assert_eq!(app.state.token_usage.input, 300);
    assert_eq!(app.state.token_usage.output, 125);
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
            tool: "bash".into(),
            summary: "running".into(),
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
            render_header: None,
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
        tool: "task".into(),
        output: ToolOutput::Plain("result".into()),
        is_error,
        annotation: None,
        written_path: None,
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
fn ctrl_x_toggles_tasks_picker() {
    let mut app = test_app();
    app.update(Msg::Key(kb::TASKS.to_key_event()));
    assert!(app.task_picker.is_open());
    app.update(Msg::Key(kb::TASKS.to_key_event()));
    assert!(!app.task_picker.is_open());
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

const OVERLAY_BLOCKED_KEYS: &[KeyEvent] = &[
    kb::NEXT_CHAT.to_key_event(),
    kb::PREV_CHAT.to_key_event(),
    kb::SCROLL_HALF_UP.to_key_event(),
    kb::SCROLL_HALF_DOWN.to_key_event(),
    kb::HELP.to_key_event(),
];

fn open_help(app: &mut App) {
    app.help_modal.toggle();
}

fn open_search(app: &mut App) {
    app.search_modal.open(0, true);
}

fn focus_queue(app: &mut App) {
    app.status = Status::Streaming;
    app.run_id = 1;
    app.queue_and_notify(queued_msg("q"));
    app.queue.set_focus_at(0);
}

#[test_case(open_tasks_picker as fn(&mut App) ; "task_picker")]
#[test_case(open_help                         ; "help_modal")]
#[test_case(open_search                       ; "search_modal")]
#[test_case(focus_queue                       ; "queue_focus")]
fn overlay_blocks_ctrl_shortcuts(setup: fn(&mut App)) {
    let mut app = app_with_subagent();
    setup(&mut app);
    let before = app.active_chat;
    let scroll_before = app.chats[app.active_chat].scroll_top();

    for k in OVERLAY_BLOCKED_KEYS {
        app.update(Msg::Key(*k));
    }

    assert_eq!(
        app.active_chat, before,
        "active_chat changed through overlay"
    );
    assert_eq!(
        app.chats[app.active_chat].scroll_top(),
        scroll_before,
        "scroll changed through overlay"
    );
}

#[test]
fn compact_command_sets_streaming() {
    let mut app = test_app();
    let actions = app.execute_command(cmd("/compact"));
    assert!(matches!(&actions[0], Action::Compact));
    assert_eq!(app.status, Status::Streaming);
}

#[test]
fn compact_during_streaming_queues_item() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    let actions = app.execute_command(cmd("/compact"));
    assert!(actions.is_empty());
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, "/compact");
}

#[test]
fn cancel_clears_pending_input() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.pending_input = PendingInput::AuthRetry { subagent_id: None };
    cancel_app(&mut app);
    assert_eq!(app.pending_input, PendingInput::None);
}

#[test]
fn scroll_disables_auto_scroll() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().enable_auto_scroll();

    app.update(Msg::Scroll {
        column: 10,
        row: 10,
        delta: 3,
    });
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

#[test]
fn scroll_shortcuts_toggle_auto_scroll() {
    let mut app = test_app();
    app.active_chat().enable_auto_scroll();
    app.update(Msg::Key(kb::SCROLL_TOP.to_key_event()));
    assert!(!app.chats[0].auto_scroll());
    app.update(Msg::Key(kb::SCROLL_BOTTOM.to_key_event()));
    assert!(app.chats[0].auto_scroll());
}

#[test]
fn mouse_drag_updates_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 20, 10));

    let state = app.selection_state.as_ref().unwrap();
    let (_, end) = state.sel().normalized();
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
    let (_, end) = state.sel().normalized();
    assert_eq!(end.col, 79);
    assert_eq!(end.row, 19, "clamped to area bottom");
    assert!(
        app.selection_state.as_ref().unwrap().is_edge_scrolling(),
        "outside area triggers edge scroll"
    );
}

#[test_case(Rect::new(0, 2, 80, 20), (10, 12), (10, 1),  Some(EDGE_SCROLL_LINES)  ; "top_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (10, 22), Some(-EDGE_SCROLL_LINES) ; "bottom_edge")]
#[test_case(Rect::new(0, 2, 80, 20), (10, 10), (20, 15), None                     ; "middle_no_scroll")]
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
    let edge_dir = match state {
        SelectionState::Dragging { edge_scroll, .. } => edge_scroll.as_ref().map(|es| es.dir),
        _ => None,
    };
    assert_eq!(edge_dir, expected);
}

#[test]
fn mouse_up_clears_edge_scroll() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 2, 80, 20));
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    assert!(app.selection_state.as_ref().unwrap().is_edge_scrolling());

    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 1));
    let state = app.selection_state.as_ref().unwrap();
    assert!(state.is_pending_copy());
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
        tool: "bash".into(),
        summary: "running".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));

    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(actions.is_empty());

    app.last_esc = Some(Instant::now());
    let actions = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert_eq!(app.status, Status::Idle);
    assert_eq!(app.chats[0].in_progress_count(), 0);
}

#[test]
fn double_esc_idle_opens_rewind_picker() {
    let mut app = test_app();
    type_and_submit(&mut app, "hello");
    app.status = Status::Idle;
    app.run_id = 1;
    app.state
        .session
        .messages
        .push(Message::user("hello".into()));

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
fn ctrl_c_while_streaming_cancels_instead_of_quitting() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert!(matches!(&actions[0], Action::CancelAgent { .. }));
    assert_eq!(app.status, Status::Idle);
    assert_ne!(app.exit_request, ExitRequest::Success);
}

#[test]
fn edge_scroll_makes_app_animating() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta { text: "x".into() }));
    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    }));
    assert!(!app.is_animating());
    let zone = Rect::new(0, 2, 80, 20);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 1));
    assert!(app.is_animating());
}

#[test]
fn empty_click_clears_selection() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 5, 5));
    assert!(app.selection_state.is_none());
}

fn make_pending_copy(app: &mut App) {
    set_zone(app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 10));
    app.update(mouse_event(MouseEventKind::Up(MouseButton::Left), 10, 10));
}

#[test]
fn key_clears_dragging_but_preserves_pending_copy() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert!(app.selection_state.is_none(), "key clears dragging");

    make_pending_copy(&mut app);
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert!(
        app.selection_state.as_ref().unwrap().is_pending_copy(),
        "key preserves pending copy"
    );
}

#[test]
fn scroll_clears_dragging_but_preserves_pending_copy() {
    let scroll = || Msg::Scroll {
        column: 10,
        row: 10,
        delta: 3,
    };

    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 80, 20));
    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 5, 5));
    app.update(scroll());
    assert!(app.selection_state.is_none(), "scroll clears dragging");

    make_pending_copy(&mut app);
    app.update(scroll());
    assert!(
        app.selection_state.as_ref().unwrap().is_pending_copy(),
        "scroll preserves pending copy"
    );
}

#[test]
fn new_mouse_down_replaces_pending_copy_with_dragging() {
    let mut app = test_app();
    make_pending_copy(&mut app);

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 15, 15));
    assert!(matches!(
        app.selection_state.as_ref().unwrap(),
        SelectionState::Dragging { .. }
    ));
}

#[test]
fn drag_on_pending_copy_is_noop() {
    let mut app = test_app();
    make_pending_copy(&mut app);

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 50, 50));
    assert!(app.selection_state.as_ref().unwrap().is_pending_copy());
}

#[test]
fn tick_edge_scroll_noop_on_pending_copy() {
    let mut app = test_app();
    make_pending_copy(&mut app);

    app.tick_edge_scroll();
    assert!(app.selection_state.as_ref().unwrap().is_pending_copy());
}

#[test]
fn pending_copy_not_animating() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::TextDelta { text: "x".into() }));
    app.update(agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    }));
    make_pending_copy(&mut app);
    assert!(!app.is_animating());
}

#[test]
fn edge_scroll_direction_switches_on_drag_reversal() {
    let mut app = test_app();
    let zone = Rect::new(0, 5, 80, 10);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 8));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 4));

    if let Some(SelectionState::Dragging { edge_scroll, .. }) = &app.selection_state {
        assert!(
            edge_scroll.as_ref().unwrap().dir > 0,
            "scrolling up (positive dir)"
        );
    } else {
        panic!("expected Dragging");
    }

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 16));
    if let Some(SelectionState::Dragging { edge_scroll, .. }) = &app.selection_state {
        assert!(
            edge_scroll.as_ref().unwrap().dir < 0,
            "scrolling down after reversal"
        );
    } else {
        panic!("expected Dragging");
    }
}

#[test]
fn drag_back_into_area_clears_edge_scroll() {
    let mut app = test_app();
    let zone = Rect::new(0, 5, 80, 10);
    set_zone(&mut app, SelectionZone::Messages, zone);
    app.active_chat().scroll_to_top();

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 10, 8));
    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 4));
    assert!(app.selection_state.as_ref().unwrap().is_edge_scrolling());

    app.update(mouse_event(MouseEventKind::Drag(MouseButton::Left), 10, 10));
    assert!(
        !app.selection_state.as_ref().unwrap().is_edge_scrolling(),
        "dragging back into area must stop edge scroll"
    );
}

#[test]
fn mouse_down_outside_all_zones_ignored() {
    let mut app = test_app();
    set_zone(&mut app, SelectionZone::Messages, Rect::new(0, 0, 40, 10));

    app.update(mouse_event(MouseEventKind::Down(MouseButton::Left), 50, 15));
    assert!(
        app.selection_state.is_none(),
        "click outside zones must not create selection"
    );
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

#[test]
fn queue_boundary_clamps() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.queue.set_focus_at(0);
    app.update(Msg::Key(key(KeyCode::Up)));
    assert_eq!(app.queue.focus(), Some(0), "up at top clamps");
    app.queue.set_focus_at(1);
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.queue.focus(), Some(1), "down at bottom clamps");
}

#[test]
fn queue_enter_removes_selected() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.queue.set_focus_at(0);

    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, "second");
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

#[test]
fn ctrl_q_pops_front() {
    let mut app = app_with_queued_message();
    app.queue_and_notify(queued_msg("second"));
    app.update(Msg::Key(kb::POP_QUEUE.to_key_event()));
    assert_eq!(app.queue.len(), 1);
    assert_eq!(app.queue.panel_entries()[0].text, "second");
    assert!(app.queue.focus().is_none(), "unfocused stays unfocused");

    app.queue_and_notify(queued_msg("third"));
    app.queue.set_focus_at(1);
    app.update(Msg::Key(kb::POP_QUEUE.to_key_event()));
    assert_eq!(
        app.queue.focus(),
        Some(0),
        "focus adjusted when item removed"
    );
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
fn stale_events_ignored_after_run_id_increment() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    cancel_app(&mut app);
    let current_run = app.run_id;
    let actions = type_and_submit(&mut app, "new prompt");
    assert!(matches!(&actions[0], Action::SendMessage(i) if i.message == "new prompt"));
    let active_run = app.run_id;

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "stale text".into(),
        },
        current_run,
    ));
    assert_eq!(app.chats[0].last_message_text(), "new prompt");

    app.update(agent_msg_with_run_id(
        AgentEvent::TextDelta {
            text: "new text".into(),
        },
        active_run,
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
    app.queue_and_notify(queued_msg("next"));

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
    assert_eq!(state.sel().zone, SelectionZone::Input);
    assert_eq!(state.sel().area, input);
}

#[test]
fn resolve_or_create_chat_sets_model_id_and_annotation() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "task1".into(),
        tool: "task".into(),
        summary: "research".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
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

#[test]
fn help_toggles_modal() {
    let mut app = test_app();
    assert!(!app.help_modal.is_open());
    app.update(Msg::Key(kb::HELP.to_key_event()));
    assert!(app.help_modal.is_open());
    app.execute_command(cmd("/help"));
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
    |app: &mut App| { app.state.mode = Mode::Plan; app.plan_form.on_plan_ready(); },
    &[KeybindContext::FormInput],
    &[KeybindContext::Editing]
    ; "plan_form"
)]
#[test_case(
    |app: &mut App| { app.status = Status::Streaming; app.run_id = 1; app.queue_and_notify(queued_msg("q")); app.queue.set_focus_at(0); },
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
        app.state.session.messages.push(Message::user("test".into()));
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
    assert_eq!(app.exit_request, ExitRequest::Success);
    assert!(matches!(&actions[0], Action::Quit));
}

#[test_case(0, "hello"            ; "no_images")]
#[test_case(1, "hello [1 image]"  ; "single_image")]
#[test_case(2, "hello [2 images]" ; "multiple_images")]
fn format_with_images_label(count: usize, expected: &str) {
    assert_eq!(format_with_images("hello", count), expected);
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

#[test]
fn cd_command_behavior() {
    let mut app = test_app();
    app.execute_command(ParsedCommand {
        name: "/cd".into(),
        args: "/tmp".into(),
    });
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.starts_with("cd /tmp"), "flash={flash:?}");
    let canonical = std::fs::canonicalize("/tmp").unwrap();
    assert_eq!(app.state.session.cwd, canonical.to_string_lossy());

    app.execute_command(ParsedCommand {
        name: "/cd".into(),
        args: "/nonexistent_path_12345".into(),
    });
    let flash = app.status_bar.flash_text().unwrap();
    assert!(flash.starts_with("cd: "), "error flash={flash:?}");
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

    app.state.session.messages = vec![
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
    app.state
        .session
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

    assert_eq!(app.state.session.messages.len(), 2);
    assert!(app.state.session.tool_outputs.contains_key("tool-1"));
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
    app.state.token_usage.input = 500;
    app.state.token_usage.output = 200;
    let entry = crate::components::rewind_picker::RewindEntry {
        turn_index: 0,
        prompt_preview: "1: first".into(),
        prompt_text: "first prompt".into(),
    };
    let actions = app.rewind_to(entry);

    assert!(app.state.session.messages.is_empty());
    assert!(!app.state.session.tool_outputs.contains_key("tool-1"));
    assert_eq!(app.state.token_usage.input, 500);
    assert_eq!(app.state.token_usage.output, 200);
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

fn auth_retry_enter(app: &mut App) -> Vec<Action> {
    app.update(Msg::Key(key(KeyCode::Enter)))
}

fn auth_retry_type_then_enter(app: &mut App) -> Vec<Action> {
    type_and_submit(app, "ignored")
}

#[test_case(auth_retry_enter          ; "bare_enter")]
#[test_case(auth_retry_type_then_enter ; "typed_text_then_enter")]
fn auth_retry_sends_empty_answer(submit: fn(&mut App) -> Vec<Action>) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    let (tx, rx) = flume::unbounded();
    app.answer_tx = Some(tx);

    app.update(agent_msg(AgentEvent::AuthRequired));
    assert!(matches!(
        app.pending_input,
        PendingInput::AuthRetry { subagent_id: None }
    ));

    let actions = submit(&mut app);
    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(rx.try_recv().unwrap(), "");
}

fn app_with_subagent_tx(id: &str) -> (App, flume::Receiver<String>, flume::Receiver<String>) {
    let (sub_tx, sub_rx) = flume::unbounded();
    let (main_tx, main_rx) = flume::unbounded();
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.answer_tx = Some(main_tx);
    app.update(Msg::Agent(Box::new(Envelope {
        event: AgentEvent::TextDelta { text: "x".into() },
        subagent: Some(subagent_info_with_tx(id, "research", Some(sub_tx))),
        run_id: 1,
    })));
    (app, sub_rx, main_rx)
}

#[test]
fn auth_required_in_subagent_shows_in_both_chats() {
    let mut app = app_with_subagent_id("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    assert_eq!(app.chats[1].last_message_text(), AUTH_EXPIRED_MSG);
    assert_eq!(app.chats[0].last_message_text(), AUTH_EXPIRED_MSG);
    assert!(matches!(
        app.pending_input,
        PendingInput::AuthRetry { subagent_id: Some(ref id) } if id == "sub1"
    ));
}

#[test]
fn auth_retry_in_subagent_routes_to_subagent_channel() {
    let (mut app, sub_rx, main_rx) = app_with_subagent_tx("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(actions.is_empty());
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(sub_rx.try_recv().unwrap(), "");
    assert!(main_rx.try_recv().is_err());
}

#[test]
fn cancel_clears_subagent_auth_retry() {
    let (mut app, sub_rx, _main_rx) = app_with_subagent_tx("sub1");
    app.update(subagent_msg(
        AgentEvent::AuthRequired,
        "sub1",
        Some("research"),
    ));

    cancel_app(&mut app);

    assert_eq!(app.pending_input, PendingInput::None);
    assert!(sub_rx.try_recv().is_err());
}

#[test]
fn stale_auth_required_after_cancel_is_dropped() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 2;
    let count_before = app.chats[0].message_count();
    app.update(Msg::Agent(Box::new(Envelope {
        event: AgentEvent::AuthRequired,
        subagent: None,
        run_id: 1,
    })));
    assert_eq!(app.pending_input, PendingInput::None);
    assert_eq!(app.chats[0].message_count(), count_before);
}

#[test]
fn send_to_agent_unknown_subagent_falls_back_to_main() {
    let (main_tx, main_rx) = flume::unbounded();
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.answer_tx = Some(main_tx);

    app.pending_input = PendingInput::AuthRetry {
        subagent_id: Some("nonexistent".into()),
    };
    app.update(Msg::Key(key(KeyCode::Enter)));

    assert_eq!(main_rx.try_recv().unwrap(), "");
    assert_eq!(app.pending_input, PendingInput::None);
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

#[test]
fn mcp_command_opens_picker() {
    let mut app = test_app();
    app.execute_command(cmd("/mcp"));
    assert!(app.mcp_picker.is_open());
}

#[test]
fn mcp_toggle_dispatches_action() {
    let mut app = test_app();
    app.mcp_picker = McpPicker::new(
        McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![McpServerInfo {
                name: "test-srv".into(),
                transport_kind: "stdio",
                tool_count: 2,
                prompt_count: 0,
                status: McpServerStatus::Running,
                config_path: PathBuf::from("/tmp/config.toml"),
                url: None,
            }],
            prompts: vec![],
            pids: vec![],
            generation: 0,
        }),
        McpConfigErrors::new(PathBuf::new()),
    );
    app.execute_command(cmd("/mcp"));

    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(matches!(
        &actions[0],
        Action::ToggleMcp(name, false) if name == "test-srv"
    ));
}

#[test_case(
    |app: &mut App| { app.state.mode = Mode::Plan; app.plan_form.on_plan_ready(); },
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

#[test_case(PlanState::None,                                       true  ; "no_plan")]
#[test_case(PlanState::Drafting(PathBuf::from("/tmp/plan.md")),     false ; "plan_drafting")]
#[test_case(PlanState::Ready(PathBuf::from("/tmp/plan.md")),       false ; "plan_ready")]
fn open_editor(plan: PlanState, expect_flash: bool) {
    let mut app = test_app();
    let plan_path = plan.path().map(Path::to_path_buf);
    app.state.plan = plan;
    let actions = app.update(Msg::Key(kb::OPEN_EDITOR.to_key_event()));
    if expect_flash {
        assert!(actions.is_empty());
        assert_eq!(app.status_bar.flash_text().unwrap(), FLASH_NO_PLAN);
        assert!(!app.plan_form.is_visible());
    } else {
        let expected = plan_path.unwrap();
        assert!(matches!(&actions[..], [Action::OpenEditor(p)] if p == &expected));
        assert!(!app.plan_form.is_visible());
    }
}

#[test]
fn alt_o_opens_editor_for_input() {
    let mut app = test_app();
    app.input_box.buffer.insert_text("hello");
    let actions = app.update(Msg::Key(kb::EDIT_INPUT.to_key_event()));
    assert!(matches!(&actions[..], [Action::EditInputInEditor]));
}

#[test]
fn btw_empty_flashes_error() {
    let mut app = test_app();
    let actions = app.execute_command(ParsedCommand {
        name: "/btw".into(),
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
        name: "/btw".into(),
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
    assert_eq!(state.sel().zone, SelectionZone::Overlay);
}

fn streaming_app_with_history() -> App {
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
    assert!(app.state.session.messages.is_empty());

    app.update(agent_msg_with_run_id(event, old_run_id));
    assert_eq!(app.state.session.messages.len(), 2);
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
    assert!(app.state.session.messages.is_empty());
}

#[test]
fn error_event_matching_run_id_saves_session() {
    let mut app = streaming_app_with_history();
    app.update(agent_msg(AgentEvent::Error {
        message: "boom".into(),
    }));
    assert_eq!(app.state.session.messages.len(), 2);
}

// --- Plan form integration tests ---

fn done_event() -> Msg {
    agent_msg(AgentEvent::Done {
        usage: TokenUsage::default(),
        num_turns: 1,
        stop_reason: None,
    })
}

fn implement_msg(parallel: bool) -> String {
    if parallel {
        format!("{IMPLEMENT_MSG_PREFIX} at `test-plan.md`. {IMPLEMENT_PARALLEL_HINT}")
    } else {
        format!("{IMPLEMENT_MSG_PREFIX} at `test-plan.md`.")
    }
}

fn plan_app() -> App {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("test-plan.md"));
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 42 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
    }))));
    app
}

#[test_case(Mode::Plan,  true  ; "plan_mode_tooldone_opens_form")]
#[test_case(Mode::Build, false ; "build_mode_tooldone_no_form")]
fn tool_done_write_opens_plan_form(mode: Mode, expect_form: bool) {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = mode;
    app.state.plan = PlanState::Drafting(PathBuf::from("/tmp/plans/test.md"));
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t1".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 42 bytes to /tmp/plans/test.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("/tmp/plans/test.md".into()),
    }))));
    assert_eq!(app.plan_form.is_visible(), expect_form);
    if expect_form {
        assert!(app.state.plan.is_ready());
    }
}

#[test]
fn done_event_does_not_open_plan_form() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Ready(PathBuf::from("test-plan.md"));
    app.update(done_event());
    assert!(!app.plan_form.is_visible());
}

#[test]
fn re_edit_keeps_plan_form_visible() {
    let mut app = plan_app();
    assert!(app.state.plan.is_ready());
    assert!(app.plan_form.is_visible());

    // Agent edits the plan again (second write to same path) — idempotent, stays Ready
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t2".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 50 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
    }))));
    assert!(matches!(app.state.plan, PlanState::Ready(_)));
    assert!(app.plan_form.is_visible());
}

#[test_case(0, Mode::Build, true,  true  ; "clear_and_implement")]
#[test_case(1, Mode::Build, false, true  ; "implement_keeps_context")]
fn plan_form_menu_options(
    downs: usize,
    expected_mode: Mode,
    has_new_session: bool,
    has_send_message: bool,
) {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    for _ in 0..downs {
        app.update(Msg::Key(key(KeyCode::Down)));
    }
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.plan_form.is_visible());
    assert_eq!(app.state.mode, expected_mode);
    assert_eq!(app.state.plan, PlanState::None);
    assert_eq!(
        actions.iter().any(|a| matches!(a, Action::NewSession)),
        has_new_session
    );
    let expected_msg = implement_msg(PlanForm::new().parallel());
    assert_eq!(
        actions
            .iter()
            .any(|a| matches!(a, Action::SendMessage(i) if i.message == expected_msg)),
        has_send_message
    );
}

#[test]
fn plan_form_implement_toggled_parallel() {
    let mut app = plan_app();
    app.update(Msg::Key(key(KeyCode::Char(' '))));
    let actions = app.update(Msg::Key(key(KeyCode::Enter)));
    let expected_msg = implement_msg(!PlanForm::new().parallel());
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::SendMessage(i) if i.message == expected_msg))
    );
}

#[test]
fn plan_form_open_editor() {
    let mut app = plan_app();

    let actions = app.update(Msg::Key(kb::OPEN_EDITOR.to_key_event()));
    assert!(app.plan_form.is_visible());
    assert!(matches!(&actions[..], [Action::OpenEditor(p)] if p == Path::new("test-plan.md")));
}

fn rewrite_plan(app: &mut App) {
    app.update(agent_msg(AgentEvent::ToolDone(Box::new(ToolDoneEvent {
        id: "t2".into(),
        tool: "write".into(),
        output: ToolOutput::Plain("wrote 99 bytes to test-plan.md".into()),
        is_error: false,
        annotation: None,
        written_path: Some("test-plan.md".into()),
    }))));
}

fn dismiss_plan_esc(app: &mut App) {
    app.update(Msg::Key(key(KeyCode::Esc)));
}

#[test]
fn rewrite_does_not_reopen_after_dismiss() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    dismiss_plan_esc(&mut app);
    assert!(!app.plan_form.is_visible());
    assert!(app.state.plan.is_ready());

    rewrite_plan(&mut app);
    assert!(!app.plan_form.is_visible());
    assert!(app.state.plan.is_ready());
}

#[test]
fn ctrl_t_toggles_plan_form_in_plan_mode() {
    let mut app = plan_app();
    assert!(app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(!app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(app.plan_form.is_visible());
}

#[test]
fn ctrl_t_noop_when_plan_not_ready() {
    let mut app = test_app();
    app.state.mode = Mode::Plan;
    app.state.plan = PlanState::Drafting(PathBuf::from("test-plan.md"));
    assert!(!app.plan_form.is_visible());

    app.update(Msg::Key(kb::PLAN_TOGGLE.to_key_event()));
    assert!(!app.plan_form.is_visible());
}

#[test]
fn reset_session_closes_plan_form() {
    let mut app = plan_app();
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
    assert_eq!(app.exit_request, ExitRequest::None);
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
        app.state.mode,
        Mode::Build,
        "tab must not toggle while bash prefix present"
    );

    app.input_box.set_input("ls".into());
    assert_eq!(&*app.mode_label().0, "[BUILD]");
}

#[test]
fn thinking_toggle_cycles_off_adaptive() {
    let mut app = test_app();
    assert_eq!(app.state.thinking, ThinkingConfig::Off);

    app.execute_command(cmd("/thinking"));
    assert_eq!(app.state.thinking, ThinkingConfig::Adaptive);

    app.execute_command(cmd("/thinking"));
    assert_eq!(app.state.thinking, ThinkingConfig::Off);
}

#[test]
fn thinking_explicit_args() {
    let mut app = test_app();

    app.execute_command(ParsedCommand {
        name: "/thinking".into(),
        args: "8192".into(),
    });
    assert_eq!(app.state.thinking, ThinkingConfig::Budget(8192));
}

#[test]
fn thinking_non_anthropic_flashes_error() {
    let mut app = test_app();
    app.state.model.provider = maki_providers::provider::ProviderKind::Ollama;

    app.execute_command(cmd("/thinking"));
    assert_eq!(app.state.thinking, ThinkingConfig::Off);
    assert!(app.status_bar.flash_text().is_some());
}

#[test]
fn thinking_restored_from_session_meta() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    let mut session = AppSession::new("test-model", "/tmp/test");
    session.meta.thinking = Some(StoredThinking::Budget { tokens: 4096 });

    let state = SessionState::from_session(session, &test_model(), &storage);
    assert_eq!(state.thinking, ThinkingConfig::Budget(4096));
}

fn set_opus_model(app: &mut App) {
    app.state.model = maki_providers::Model::from_spec("anthropic/claude-opus-4-8").unwrap();
}

#[test]
fn fast_toggle_on_off_on_opus() {
    let mut app = test_app();
    set_opus_model(&mut app);
    assert!(!app.state.fast);

    app.execute_command(cmd("/fast"));
    assert!(app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_ON_MSG));

    app.execute_command(cmd("/fast"));
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_OFF_MSG));
}

#[test_case("anthropic/claude-sonnet-4-5" ; "non_opus_anthropic")]
#[test_case("openai/gpt-5.5" ; "non_anthropic")]
fn fast_flashes_error_on_ineligible_model(spec: &str) {
    let mut app = test_app();
    app.state.model = maki_providers::Model::from_spec(spec).unwrap();

    app.execute_command(cmd("/fast"));
    assert!(!app.state.fast);
    assert_eq!(app.status_bar.flash_text(), Some(FAST_UNSUPPORTED_MSG));
}

#[test]
fn fast_restored_from_session_meta() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    let mut session = AppSession::new("anthropic/claude-opus-4-8", "/tmp/test");
    session.meta.fast = true;

    let state = SessionState::from_session(session, &test_model(), &storage);
    assert!(state.fast);
}

#[test]
fn fast_normalized_off_when_restored_onto_ineligible_model() {
    let tmp = TempDir::new().unwrap();
    let storage = StateDir::from_path(tmp.path().to_path_buf());
    // Saved as fast=true, but sonnet cannot do fast mode, so restoring must drop
    // it to false or the UI would show a phantom [fast] badge.
    let mut session = AppSession::new("anthropic/claude-sonnet-4-5", "/tmp/test");
    session.meta.fast = true;

    let state = SessionState::from_session(session, &test_model(), &storage);
    assert!(!state.fast);
}

#[test]
fn update_model_to_ineligible_resets_fast() {
    let mut app = test_app();
    set_opus_model(&mut app);
    app.state.fast = true;

    let sonnet = maki_providers::Model::from_spec("anthropic/claude-sonnet-4-5").unwrap();
    app.state.update_model(&sonnet);
    assert!(!app.state.fast);
}

#[test]
fn agent_error_creates_synthetic_tool_done_with_message() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.run_id = 1;

    app.update(agent_msg(AgentEvent::ToolStart(Box::new(ToolStartEvent {
        id: "t1".into(),
        tool: "bash".into(),
        summary: "echo hello".into(),
        annotation: None,
        input: None,
        raw_input: None,
        output: None,
        render_header: None,
    }))));
    assert_eq!(app.main_chat().in_progress_count(), 1);

    let error_msg = "Provider is overloaded";
    app.update(agent_msg(AgentEvent::Error {
        message: error_msg.into(),
    }));

    assert_eq!(app.main_chat().in_progress_count(), 0);
    let text = app.main_chat().last_message_text();
    assert!(
        text.contains(error_msg),
        "tool output should contain error: {text}"
    );
}

#[test]
fn ctrl_c_denies_permission_prompt() {
    let mut app = test_app();
    app.permission_prompt
        .open("id".into(), "bash".into(), vec!["execute".into()], None);
    assert!(app.permission_prompt.is_open());

    let actions = app.update(Msg::Key(kb::QUIT.to_key_event()));
    assert_eq!(app.exit_request, ExitRequest::None);
    assert!(!app.permission_prompt.is_open());
    assert!(actions.is_empty());
}

#[test_case(false, false => false ; "neither")]
#[test_case(true,  false => true  ; "messages only")]
#[test_case(false, true  => true  ; "ephemeral only")]
#[test_case(true,  true  => true  ; "both")]
fn has_content(messages: bool, ephemeral: bool) -> bool {
    let mut app = test_app();
    app.state.session.meta.mode = Some(maki_storage::sessions::StoredMode::Build);
    if messages {
        app.state
            .session
            .messages
            .push(maki_providers::Message::user("hello".into()));
    }
    if ephemeral {
        app.state.session.meta.input_draft = Some("wip".into());
    }
    app.has_content()
}

const TEST_AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 40,
};
const SPLIT_EXTENT: u16 = 8;

fn open_split_window(app: &mut App, dir: maki_lua::Split) {
    let buf = Arc::new(maki_agent::SharedBuf::new());
    let config = maki_lua::FloatConfig {
        width: maki_lua::Dimension::Abs(SPLIT_EXTENT),
        height: maki_lua::Dimension::Abs(SPLIT_EXTENT),
        border: maki_lua::Border::None,
        split: dir,
        ..maki_lua::FloatConfig::default()
    };
    let (event_tx, _event_rx) = flume::bounded::<maki_lua::WinEvent>(8);
    let (_cmd_tx, cmd_rx) = flume::bounded::<maki_lua::WinCommand>(8);
    app.float_mgr.open(buf, config, true, event_tx, cmd_rx);
}

#[test]
fn below_split_reserves_bottom_and_suppresses_input() {
    let mut app = test_app();
    let (msg_before, _b, _s, input_before, splits_before) = app.layout_geometry(TEST_AREA);
    assert!(
        splits_before.rect(maki_lua::Split::Below).is_none(),
        "no split open yet"
    );
    assert!(input_before.height > 0, "input box visible before split");

    open_split_window(&mut app, maki_lua::Split::Below);
    let (msg_after, _bottom, _s, input_after, splits_after) = app.layout_geometry(TEST_AREA);

    let band = splits_after
        .rect(maki_lua::Split::Below)
        .expect("below split should reserve a bottom band");
    assert_eq!(
        band.height, SPLIT_EXTENT,
        "below band reserves the requested rows",
    );
    assert!(
        msg_after.height < msg_before.height,
        "chat must shrink to make room for the below split",
    );
    assert_eq!(
        input_after.height, 0,
        "input box is suppressed under a below split"
    );
}

/// `carve` already tests the per-direction geometry; this pins the app wiring:
/// a split shrinks the chat while the full-width status bar stays put. Below is
/// tested separately since it also hides the input box.
#[test_case(maki_lua::Split::Above ; "above")]
#[test_case(maki_lua::Split::Left ; "left")]
#[test_case(maki_lua::Split::Right ; "right")]
fn non_below_split_reserves_band_and_keeps_status_full_width(dir: maki_lua::Split) {
    let mut app = test_app();
    let (msg_before, _b, _s, _i, _sp) = app.layout_geometry(TEST_AREA);

    open_split_window(&mut app, dir);
    let (msg_after, _bottom, status_after, _input, splits) = app.layout_geometry(TEST_AREA);

    assert!(splits.rect(dir).is_some(), "split must reserve a band");
    assert!(
        msg_after.area() < msg_before.area(),
        "chat must shrink to make room for the split",
    );
    assert_eq!(
        status_after.width, TEST_AREA.width,
        "status bar stays full width regardless of the split",
    );
}

#[test]
fn closing_split_restores_layout() {
    let mut app = test_app();
    let before = app.layout_geometry(TEST_AREA);

    open_split_window(&mut app, maki_lua::Split::Below);
    app.float_mgr.close_all();

    let after = app.layout_geometry(TEST_AREA);
    assert_eq!(after, before, "closing the split restores the layout");
}

#[test]
fn permission_prompt_takes_bottom_precedence_over_below_split() {
    let mut app = test_app();
    open_split_window(&mut app, maki_lua::Split::Below);
    open_split_window(&mut app, maki_lua::Split::Left);
    open_split_window(&mut app, maki_lua::Split::Above);
    app.permission_prompt
        .open("perm-1".into(), "bash".into(), vec!["ls".into()], None);

    let (_msg, _bottom, _status, _input, splits) = app.layout_geometry(TEST_AREA);
    assert!(
        splits.rect(maki_lua::Split::Below).is_none(),
        "below split must yield the bottom area to an open permission prompt",
    );
    assert!(
        splits.rect(maki_lua::Split::Left).is_some(),
        "the prompt must leave a left split untouched",
    );
    assert!(
        splits.rect(maki_lua::Split::Above).is_some(),
        "the prompt must leave an above split untouched",
    );
}
