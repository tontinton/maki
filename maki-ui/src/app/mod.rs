//! Elm-style `update(Msg) -> Vec<Action>`; side effects are dispatched by the caller.
//! Double-esc: first esc flashes a hint, second within `flash_duration` cancels/rewinds.
//! `run_id` increments each run so stale events from previous agent runs are ignored.

mod btw;
mod image_paste;
mod mode;
mod mouse;
mod queue;
mod session;
pub(crate) mod session_state;
pub(crate) mod shell;
#[cfg(test)]
mod tests;
mod view;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::AppSession;
use crate::chat::Chat;
use crate::chat::{CANCELLED_TEXT, ChatEventResult, DONE_TEXT, ERROR_TEXT};
use crate::clipboard::ClipboardState;
use crate::components::btw_modal::BtwModal;
use crate::components::command::{CommandAction, CommandPalette, ParsedCommand};
use crate::components::file_picker::{FilePickerModal, FilePickerModalAction};
use crate::components::help_modal::HelpModal;
use crate::components::input::{InputAction, InputBox, Submission};
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::mcp_picker::{McpPicker, McpPickerAction};
use crate::components::memory_modal::{MemoryEntry, MemoryModal, MemoryModalAction};
use crate::components::model_picker::{ModelPicker, ModelPickerAction};
use crate::components::permission_prompt::PermissionPrompt;
use crate::components::plan_form::{PlanForm, PlanFormAction};
use crate::components::question_form::{QuestionForm, QuestionFormAction};
use crate::components::rewind_picker::{RewindPicker, RewindPickerAction};
use crate::components::search_modal::{SearchAction, SearchModal};
use crate::components::session_picker::{SessionPicker, SessionPickerAction};
use crate::components::status_bar::StatusBar;
use crate::components::theme_picker::{ThemePicker, ThemePickerAction};
use crate::components::tool_display::format_turn_usage;
use crate::components::{
    Action, DisplayMessage, DisplayRole, ExitRequest, Overlay, RetryInfo, Status, is_ctrl,
};
use crate::event_loop::BufClickHandler;
use crate::image;
use crate::selection::{SelectionState, SelectionZone, ZoneRegistry};
use arc_swap::{ArcSwap, ArcSwapOption};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
#[cfg(feature = "demo")]
use maki_agent::QuestionInfo;
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    AgentEvent, Envelope, ImageSource, McpPromptInfo, McpSnapshotReader, SubagentInfo, ToolOutput,
};
use maki_config::UiConfig;
use maki_providers::{Message, Model, ThinkingConfig};
use maki_storage::StateDir;
use maki_storage::input_history::InputHistory;
use maki_storage::model::persist_model;

use crate::storage_writer::StorageWriter;
use ratatui::layout::Position;

pub(crate) use crate::agent::QueuedMessage;
pub(crate) use mode::{Mode, PlanState, PlanTrigger};
#[cfg(test)]
use mouse::EDGE_SCROLL_LINES;
pub(crate) use queue::MessageQueue;
use session_state::SessionState;

const CANCEL_MSG: &str = "Cancelled.";
const FLASH_CANCEL: &str = "Press esc again to stop...";
const FLASH_REWIND: &str = "Press esc again to rewind...";
const AUTH_EXPIRED_MSG: &str =
    "Token expired. Run `maki auth login` in another terminal, then press Enter to retry.";
const FLASH_NO_PLAN: &str = "No plan file";
const IMPLEMENT_MSG_PREFIX: &str = "Implement the plan";
const IMPLEMENT_PARALLEL_HINT: &str = "For multi-step (3+) plans: use batch+task to parallelize, assign each subagent a separate module and restrict its tests to that module to avoid interference.";

const TASK_DONE_DETAIL: &str = "✓ ";

/// `Option<bool>` lets us distinguish the main chat (None, no status indicator)
/// from subagents (Some, with spinner or checkmark).
#[derive(Clone)]
pub(super) struct TaskEntry {
    name: String,
    finished: Option<bool>,
}

impl PickerItem for TaskEntry {
    fn label(&self) -> &str {
        &self.name
    }
    fn detail(&self) -> Option<&str> {
        matches!(self.finished, Some(true)).then_some(TASK_DONE_DETAIL)
    }
    fn is_spinning(&self) -> bool {
        matches!(self.finished, Some(false))
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingInput {
    #[default]
    None,
    Question,
    AuthRetry,
}

pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Scroll { column: u16, row: u16, delta: i32 },
    Agent(Box<Envelope>),
}

pub struct App {
    pub(super) chats: Vec<Chat>,
    pub(super) active_chat: usize,
    pub(super) chat_index: HashMap<String, usize>,
    pub(crate) input_box: InputBox,
    pub(super) command_palette: CommandPalette,
    pub(super) task_picker: ListPicker<TaskEntry>,
    pub(super) task_picker_original: Option<usize>,
    pub(super) theme_picker: ThemePicker,
    pub(super) model_picker: ModelPicker,
    pub(super) mcp_picker: McpPicker,
    pub(super) session_picker: SessionPicker,
    pub(super) rewind_picker: RewindPicker,
    pub(super) help_modal: HelpModal,
    pub(super) btw_modal: BtwModal,
    pub(super) memory_modal: MemoryModal,
    pub(super) search_modal: SearchModal,
    pub(super) file_picker: FilePickerModal,
    pub(super) permission_prompt: PermissionPrompt,
    pub(super) question_form: QuestionForm,
    pub(super) plan_form: PlanForm,
    pub(super) status_bar: StatusBar,
    pub status: Status,
    pub(crate) state: session_state::SessionState,
    pub exit_request: ExitRequest,
    pub(crate) exit_on_done: bool,
    pub(crate) queue: MessageQueue,
    pub answer_tx: Option<flume::Sender<String>>,
    pub(crate) cmd_tx: Option<flume::Sender<super::AgentCommand>>,
    pub(super) pending_input: PendingInput,
    pub(crate) run_id: u64,
    pub(super) retry_info: Option<RetryInfo>,
    #[cfg(feature = "demo")]
    demo_questions: Option<(usize, Vec<QuestionInfo>)>,
    pub(super) zones: ZoneRegistry,
    pub(super) selection_state: Option<SelectionState>,
    pub(super) clipboard: ClipboardState,
    pub(super) last_esc: Option<Instant>,

    pub(crate) storage: StateDir,
    pub(crate) shared_history: Option<Arc<ArcSwap<Vec<Message>>>>,
    pub(crate) shared_tool_outputs: Option<Arc<Mutex<HashMap<String, ToolOutput>>>>,
    pub(crate) image_paste_rx: Option<flume::Receiver<Result<ImageSource, String>>>,
    storage_writer: Arc<StorageWriter>,
    pub(crate) shell: shell::ShellState,
    pub(crate) ui_config: UiConfig,
    pub(crate) permissions: Arc<PermissionManager>,
    pub(super) buf_click: Option<BufClickHandler>,
    subagent_answers: HashMap<String, flume::Sender<String>>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &Model,
        session: AppSession,
        storage: StateDir,
        available_models: Arc<ArcSwapOption<Vec<String>>>,
        mcp_reader: McpSnapshotReader,
        storage_writer: Arc<StorageWriter>,
        ui_config: UiConfig,
        input_history_size: usize,
        permissions: Arc<PermissionManager>,
        custom_commands: Arc<[maki_agent::command::CustomCommand]>,
    ) -> Self {
        let state = SessionState::from_session(session, model, &storage);
        Self {
            chats: vec![Chat::new("Main".into(), ui_config)],
            active_chat: 0,
            chat_index: HashMap::new(),
            input_box: InputBox::new(InputHistory::load(&storage, input_history_size)),
            command_palette: CommandPalette::new(custom_commands, mcp_reader.clone()),
            task_picker: ListPicker::new(),
            task_picker_original: None,
            theme_picker: ThemePicker::new(),
            model_picker: ModelPicker::new(available_models),
            mcp_picker: McpPicker::new(mcp_reader),
            session_picker: SessionPicker::new(),
            rewind_picker: RewindPicker::new(),
            help_modal: HelpModal::new(),
            btw_modal: BtwModal::new(ui_config.typewriter_ms_per_char),
            memory_modal: MemoryModal::new(),
            search_modal: SearchModal::new(),
            file_picker: FilePickerModal::new(),
            permission_prompt: PermissionPrompt::new(),
            question_form: QuestionForm::new(),
            plan_form: PlanForm::new(),
            status_bar: StatusBar::new(ui_config.flash_duration()),
            status: Status::Idle,
            state,
            exit_request: ExitRequest::None,
            exit_on_done: false,
            queue: MessageQueue::default(),
            answer_tx: None,
            cmd_tx: None,
            pending_input: PendingInput::None,
            run_id: 0,
            retry_info: None,
            #[cfg(feature = "demo")]
            demo_questions: None,
            zones: [None; SelectionZone::COUNT],
            selection_state: None,
            clipboard: ClipboardState::new(),
            last_esc: None,
            storage,
            shared_history: None,
            shared_tool_outputs: None,
            image_paste_rx: None,
            storage_writer,
            shell: shell::ShellState::default(),
            ui_config,
            permissions,
            buf_click: None,
            subagent_answers: HashMap::new(),
        }
    }

    pub(crate) fn main_chat(&mut self) -> &mut Chat {
        &mut self.chats[0]
    }

    fn is_main_chat(&self) -> bool {
        self.active_chat == 0
    }

    pub(crate) fn update_model(&mut self, model: &Model) {
        self.state.update_model(model);
        persist_model(&self.storage, &self.state.session.model);
    }

    pub(crate) fn flash(&mut self, msg: String) {
        self.status_bar.flash(msg);
    }

    pub(crate) fn refresh_memory_entry(&mut self, path: &std::path::Path) {
        if self.memory_modal.is_open()
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && let Ok(meta) = std::fs::metadata(path)
        {
            self.memory_modal.update_size(name, meta.len());
        }
    }

    pub fn tick_error_expiry(&mut self) {
        if self.status.is_error_expired() {
            self.status = Status::Idle;
        }
    }

    fn active_chat(&mut self) -> &mut Chat {
        &mut self.chats[self.active_chat]
    }

    fn clear_selection_unless_pending_copy(&mut self) {
        if !self
            .selection_state
            .as_ref()
            .is_some_and(|s| s.is_pending_copy())
        {
            self.selection_state = None;
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        match msg {
            Msg::Key(key) => self.handle_key(key),
            Msg::Paste(text) => {
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                if text.is_empty() {
                    if self.is_main_chat() && self.image_paste_rx.is_none() {
                        self.start_image_paste();
                    }
                } else if let Some((path, media_type)) = image::try_parse_image_path(&text) {
                    if self.is_main_chat() && self.image_paste_rx.is_none() {
                        self.start_file_image_paste(path, media_type);
                    }
                } else {
                    self.route_text_paste(&text);
                }
                vec![]
            }
            Msg::Mouse(event) => {
                self.handle_mouse(event);
                vec![]
            }
            Msg::Scroll { column, row, delta } => {
                self.clear_selection_unless_pending_copy();
                self.handle_scroll(column, row, delta);
                vec![]
            }
            Msg::Agent(envelope) => self.handle_agent_event(*envelope),
        }
    }

    fn send_answer(&self, answer: String) {
        if let Some(tx) = &self.answer_tx {
            let _ = tx.try_send(answer);
        }
    }

    fn handle_scroll(&mut self, column: u16, row: u16, delta: i32) {
        if self.btw_modal.is_open() {
            self.btw_modal.scroll(delta);
            return;
        }
        if self.help_modal.is_open() {
            self.help_modal.scroll(delta);
            return;
        }
        if self.memory_modal.is_open() {
            let pos = Position::new(column, row);
            if self.memory_modal.contains(pos) {
                self.memory_modal.scroll(delta);
            }
            return;
        }
        let pos = Position::new(column, row);
        macro_rules! try_picker {
            ($picker:expr) => {
                if $picker.is_open() {
                    if $picker.contains(pos) {
                        $picker.scroll(delta);
                    }
                    return;
                }
            };
        }
        try_picker!(self.session_picker);
        try_picker!(self.rewind_picker);
        try_picker!(self.task_picker);
        try_picker!(self.model_picker);
        try_picker!(self.file_picker);
        if let Some(zone) = self.zone_at(row, column) {
            self.scroll_zone(zone.zone, delta);
        }
    }

    fn open_tasks(&mut self) {
        let entries: Vec<TaskEntry> = self
            .chats
            .iter()
            .enumerate()
            .map(|(i, c)| TaskEntry {
                name: c.name.clone(),
                finished: (i > 0).then_some(c.is_finished()),
            })
            .collect();
        self.task_picker_original = Some(self.active_chat);
        self.task_picker.open(entries, " Tasks ");
        self.task_picker.select(self.active_chat);
    }

    /// Ctrl shortcuts that work regardless of form/overlay state.
    fn handle_global_ctrl(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if !is_ctrl(&key) {
            return None;
        }
        if key::SUSPEND.matches(key) && cfg!(unix) {
            return Some(self.suspend());
        }
        if key::QUIT.matches(key) {
            if self.any_overlay_open() || self.queue.focus().is_some() {
                return None;
            }
            self.command_palette.close();
            return Some(if !self.is_main_chat() || self.input_box.is_empty() {
                self.quit()
            } else {
                self.input_box.discard();
                vec![]
            });
        }
        if key::PREV_CHAT.matches(key) {
            self.active_chat = self.active_chat.saturating_sub(1);
            #[cfg(feature = "demo")]
            self.check_demo_questions();
            return Some(vec![]);
        }
        if key::NEXT_CHAT.matches(key) {
            self.active_chat = (self.active_chat + 1).min(self.chats.len() - 1);
            #[cfg(feature = "demo")]
            self.check_demo_questions();
            return Some(vec![]);
        }
        if key::TASKS.matches(key) {
            if self.task_picker.is_open() {
                self.task_picker.close();
            } else if !self.has_modal_overlay() {
                self.open_tasks();
            }
            return Some(vec![]);
        }
        if !self.has_modal_overlay() {
            if key::SCROLL_HALF_UP.matches(key) {
                let half = self.chats[self.active_chat].half_page();
                self.active_chat().scroll(half);
                return Some(vec![]);
            }
            if key::SCROLL_HALF_DOWN.matches(key) {
                let half = self.chats[self.active_chat].half_page();
                self.active_chat().scroll(-half);
                return Some(vec![]);
            }
            if key::SCROLL_LINE_UP.matches(key) {
                self.active_chat().scroll(1);
                return Some(vec![]);
            }
            if key::SCROLL_LINE_DOWN.matches(key) {
                self.active_chat().scroll(-1);
                return Some(vec![]);
            }
            if key::SCROLL_TOP.matches(key) {
                self.active_chat().scroll_to_top();
                return Some(vec![]);
            }
            if key::SCROLL_BOTTOM.matches(key) {
                self.active_chat().enable_auto_scroll();
                return Some(vec![]);
            }
        }
        if key::HELP.matches(key) {
            self.help_modal.toggle();
            return Some(vec![]);
        }
        None
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if let Some(actions) = self.handle_global_ctrl(key) {
            return actions;
        }

        if self.permission_prompt.is_open() {
            if let Some(answer) = self.permission_prompt.handle_key(key) {
                let subagent_id = self.permission_prompt.subagent_id().map(str::to_owned);
                let encoded = answer.encode();
                self.permission_prompt.close();
                if let Some(tx) = subagent_id.and_then(|id| self.subagent_answers.get(&id)) {
                    let _ = tx.try_send(encoded);
                } else {
                    self.send_answer(encoded);
                }
            }
            return vec![];
        }

        if self.question_form.is_visible() {
            let action = self.question_form.handle_key(key);
            let answer = match action {
                QuestionFormAction::Submit(a) => {
                    let display = self.question_form.format_answers_display();
                    self.main_chat().push_user_message(display);
                    a
                }
                QuestionFormAction::Dismiss => String::new(),
                QuestionFormAction::Consumed => return vec![],
            };
            self.question_form.close();
            self.send_answer(answer);
            return vec![];
        }

        if self.state.mode == Mode::Plan && self.plan_form.is_visible() {
            let action = self.plan_form.handle_key(key);
            if action != PlanFormAction::Passthrough {
                return self.handle_plan_form_action(action);
            }
        }

        self.clear_selection_unless_pending_copy();

        if self.help_modal.is_open() {
            self.help_modal.handle_key(key);
            return vec![];
        }

        if self.btw_modal.is_open() {
            self.btw_modal.handle_key(key);
            return vec![];
        }

        if self.memory_modal.is_open() {
            match self.memory_modal.handle_key(key) {
                MemoryModalAction::OpenFile(filename) => {
                    return self.open_memory_file(&filename);
                }
                MemoryModalAction::DeleteFile(filename) => {
                    return self.delete_memory_file(&filename);
                }
                MemoryModalAction::ConfirmDelete => {
                    self.status_bar.flash(format!(
                        "Press {} again to confirm delete",
                        key::DELETE.label
                    ));
                }
                MemoryModalAction::Close | MemoryModalAction::Consumed => {}
            }
            return vec![];
        }

        if self.search_modal.is_open() {
            match self.search_modal.handle_key(key) {
                SearchAction::Consumed => {
                    let chat = &mut self.chats[self.active_chat];
                    let texts = chat.segment_search_texts();
                    self.search_modal.update_matches(&texts);
                    sync_search_highlight(&self.search_modal, chat);
                }
                SearchAction::Navigate => {
                    sync_search_highlight(&self.search_modal, &mut self.chats[self.active_chat]);
                }
                SearchAction::Select(idx) => {
                    let chat = &mut self.chats[self.active_chat];
                    chat.scroll_to_segment(idx);
                    chat.set_highlight_segment(None);
                    self.search_modal.close();
                }
                SearchAction::Close(saved) => {
                    let chat = &mut self.chats[self.active_chat];
                    chat.set_highlight_segment(None);
                    if let Some((top, auto)) = saved {
                        chat.restore_scroll(top, auto);
                    }
                    self.search_modal.close();
                }
            }
            return vec![];
        }

        if self.file_picker.is_open() {
            return match self.file_picker.handle_key(key) {
                FilePickerModalAction::Consumed => vec![],
                FilePickerModalAction::Select(path) => {
                    self.file_picker.close();
                    if let InputAction::PaletteSync(val) =
                        self.input_box.handle_paste_with_spaces(&path)
                    {
                        self.command_palette.sync(&val);
                    }
                    vec![]
                }
                FilePickerModalAction::Close => {
                    self.file_picker.close();
                    vec![]
                }
            };
        }

        if self.queue.focus().is_some() {
            match key.code {
                KeyCode::Up => {
                    self.queue.move_focus_up();
                    return vec![];
                }
                KeyCode::Down => {
                    self.queue.move_focus_down();
                    return vec![];
                }
                KeyCode::Enter => {
                    self.queue.remove_focused();
                    return vec![];
                }
                KeyCode::Esc => {
                    self.queue.unfocus();
                    return vec![];
                }
                _ if key::QUIT.matches(key) => {
                    self.queue.unfocus();
                    return vec![];
                }
                _ => {}
            }
        }

        if self.task_picker.is_open() {
            return match self.task_picker.handle_key(key) {
                PickerAction::Consumed | PickerAction::Toggle(..) => vec![],
                PickerAction::Select(idx, _) => {
                    self.task_picker_original = None;
                    self.active_chat = idx;
                    #[cfg(feature = "demo")]
                    self.check_demo_questions();
                    vec![]
                }
                PickerAction::Close => {
                    self.active_chat = self.task_picker_original.take().unwrap_or(0);
                    vec![]
                }
            };
        }

        if self.session_picker.is_open() {
            return match self.session_picker.handle_key(key) {
                SessionPickerAction::Consumed => vec![],
                SessionPickerAction::Select(id) => self.load_session(id),
                SessionPickerAction::ConfirmDelete => {
                    self.status_bar.flash(format!(
                        "Press {} again to confirm delete",
                        key::DELETE.label
                    ));
                    vec![]
                }
                SessionPickerAction::Delete(id) => self.delete_session(id),
                SessionPickerAction::Close => vec![],
            };
        }

        if self.rewind_picker.is_open() {
            return match self.rewind_picker.handle_key(key) {
                RewindPickerAction::Consumed => vec![],
                RewindPickerAction::Select(entry) => self.rewind_to(entry),
                RewindPickerAction::Close => vec![],
            };
        }

        if self.theme_picker.is_open() {
            return match self.theme_picker.handle_key(key) {
                ThemePickerAction::Consumed => vec![],
                ThemePickerAction::Closed => vec![],
            };
        }

        if self.model_picker.is_open() {
            return match self.model_picker.handle_key(key) {
                ModelPickerAction::Consumed => vec![],
                ModelPickerAction::Select(spec) => {
                    vec![Action::ChangeModel(spec)]
                }
                ModelPickerAction::AssignTier(spec, tier) => {
                    vec![Action::AssignTier(spec, tier)]
                }
                ModelPickerAction::Close => vec![],
            };
        }

        if self.mcp_picker.is_open() {
            return match self.mcp_picker.handle_key(key) {
                McpPickerAction::Consumed => vec![],
                McpPickerAction::Toggle {
                    server_name,
                    enabled,
                } => {
                    vec![Action::ToggleMcp(server_name, enabled)]
                }
                McpPickerAction::Close => vec![],
            };
        }

        if !self.is_main_chat() {
            return match key.code {
                KeyCode::Tab if !self.is_bash_input() => self.toggle_mode(),
                _ if key::TODO_PANEL.matches(key) => {
                    self.chats[self.active_chat].todo_panel.toggle();
                    vec![]
                }
                _ => vec![],
            };
        }

        self.handle_main_chat_key(key)
    }

    fn handle_main_chat_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key::EDIT_INPUT.matches(key) {
            return vec![Action::EditInputInEditor];
        }
        if is_ctrl(&key) {
            if key::POP_QUEUE.matches(key) {
                self.queue.remove(0);
            } else if key::OPEN_EDITOR.matches(key) {
                return match self.state.plan.path() {
                    Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
                    None => {
                        self.flash(FLASH_NO_PLAN.into());
                        vec![]
                    }
                };
            } else if key::TODO_PANEL.matches(key) {
                match self.state.mode {
                    Mode::Plan => self.plan_form.toggle(),
                    Mode::Build => self.chats[self.active_chat].todo_panel.toggle(),
                }
            } else if key::SEARCH.matches(key) {
                let top = self.chats[self.active_chat].scroll_top();
                let auto = self.chats[self.active_chat].auto_scroll();
                self.search_modal.open(top, auto);
            } else if key::FILE_PICKER.matches(key) {
                self.file_picker.open(&self.state.session.cwd);
            } else if key.code == KeyCode::Char('v') && self.image_paste_rx.is_none() {
                self.start_image_paste();
            } else if let InputAction::PaletteSync(val) = self.input_box.handle_key(key) {
                self.command_palette.sync(&val);
            }
            return vec![];
        }

        match self
            .command_palette
            .handle_key(key, &self.input_box.buffer.value())
        {
            CommandAction::Consumed => return vec![],
            CommandAction::Execute(cmd) => return self.execute_command(cmd),
            CommandAction::Close => {}
            CommandAction::Passthrough => {}
        }

        let streaming = self.status == Status::Streaming;
        match self.input_box.handle_key(key) {
            InputAction::Submit(sub) => self.handle_submit(sub),
            InputAction::PaletteSync(val) => {
                self.command_palette.sync(&val);
                vec![]
            }
            InputAction::Passthrough(key) => {
                if key.code != KeyCode::Esc {
                    self.last_esc = None;
                }
                match key.code {
                    KeyCode::Up if streaming => {
                        self.active_chat().scroll(1);
                        vec![]
                    }
                    KeyCode::Down if streaming => {
                        self.active_chat().scroll(-1);
                        vec![]
                    }
                    KeyCode::Tab if !self.is_bash_input() => self.toggle_mode(),
                    KeyCode::Esc => {
                        if let Some(t) = self.last_esc.take()
                            && t.elapsed() < self.status_bar.flash_duration
                        {
                            if streaming {
                                self.handle_cancel()
                            } else {
                                self.open_rewind_picker()
                            }
                        } else {
                            self.last_esc = Some(Instant::now());
                            self.status_bar.flash(
                                if streaming {
                                    FLASH_CANCEL
                                } else {
                                    FLASH_REWIND
                                }
                                .into(),
                            );
                            vec![]
                        }
                    }
                    _ => vec![],
                }
            }
            InputAction::ContinueLine | InputAction::None => vec![],
        }
    }

    fn suspend(&mut self) -> Vec<Action> {
        vec![Action::Suspend]
    }

    fn quit(&mut self) -> Vec<Action> {
        self.save_session();
        self.save_input_history();
        self.exit_request = ExitRequest::Success;
        vec![Action::Quit]
    }

    pub(crate) fn handle_submit(&mut self, sub: Submission) -> Vec<Action> {
        if self.pending_input == PendingInput::AuthRetry {
            self.pending_input = PendingInput::None;
            self.send_answer(String::new());
            return vec![];
        }
        if self.pending_input == PendingInput::Question {
            self.pending_input = PendingInput::None;
            self.main_chat().push_user_message(&sub.text);
            self.send_answer(sub.text);
            return vec![];
        }
        if sub.is_empty() {
            return vec![];
        }
        if sub.text.trim() == "exit" {
            return self.quit();
        }

        if let Some(prefix) = shell::parse_shell_prefix(&sub.text) {
            let cmd = prefix.command.trim();
            if cmd == "cd" || cmd.starts_with("cd ") {
                self.flash("Only /cd can change the working directory".into());
            }
            let id = self.shell.next_id();
            let sigil = if prefix.visible { "!" } else { "!!" };
            let display = format!("{sigil} {}", prefix.command);
            self.main_chat().flush();
            self.main_chat().push_user_message(display);
            self.main_chat().enable_auto_scroll();
            return vec![Action::ShellCommand {
                id,
                command: prefix.command,
                visible: prefix.visible,
            }];
        }
        let msg: QueuedMessage = sub.into();
        if self.status == Status::Streaming {
            self.queue_and_notify(msg);
            vec![]
        } else {
            self.run_id += 1;
            self.start_from_queue(&msg)
        }
    }

    fn handle_cancel(&mut self) -> Vec<Action> {
        let cancelled_run = self.run_id;
        self.run_id += 1;
        self.retry_info = None;
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.finish_subagents(DisplayRole::Error, CANCELLED_TEXT);
        self.subagent_answers.clear();
        self.shell.cancel_all();
        for chat in &mut self.chats {
            chat.flush();
            chat.cancel_in_progress();
        }
        self.main_chat()
            .push(DisplayMessage::new(DisplayRole::Error, CANCEL_MSG.into()));
        self.queue.clear();
        self.status = Status::Idle;
        vec![Action::CancelAgent {
            run_id: cancelled_run,
        }]
    }

    fn handle_agent_event(&mut self, envelope: Envelope) -> Vec<Action> {
        if envelope.run_id != self.run_id {
            // Stale run_id after cancel: agent updates shared_history before sending
            // Done/Error, so this is the first moment the full conversation is available.
            if matches!(
                envelope.event,
                AgentEvent::Done { .. } | AgentEvent::Error { .. }
            ) {
                self.save_session();
            }
            return vec![];
        }

        if let AgentEvent::SubagentHistory {
            tool_use_id,
            messages,
        } = envelope.event
        {
            self.state
                .session
                .subagent_messages
                .insert(tool_use_id, messages);
            return vec![];
        }

        let subagent_id = envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.clone());

        let chat_idx = match envelope.subagent {
            Some(ref subagent) => self.resolve_or_create_chat(subagent),
            None => 0,
        };

        if let AgentEvent::ToolDone(ref e) = envelope.event {
            if self.state.mode == Mode::Plan
                && self.state.plan.path().is_some_and(|pp| e.wrote_to(pp))
            {
                self.transition_plan(PlanTrigger::WriteDone);
            }
            if let Some(ref outputs) = self.shared_tool_outputs {
                outputs
                    .lock()
                    .unwrap()
                    .insert(e.id.clone(), e.output.clone());
            }
            if let ToolOutput::TodoList(ref items) = e.output {
                self.chats[chat_idx].todo_panel.on_todowrite(items);
            }
            if let Some(&sub_idx) = self.chat_index.get(&e.id) {
                let (role, text) = if e.is_error {
                    (DisplayRole::Error, ERROR_TEXT)
                } else {
                    (DisplayRole::Done, DONE_TEXT)
                };
                self.chats[sub_idx].mark_finished(role, text);
            }
        }

        if let AgentEvent::Retry {
            attempt,
            message,
            delay_ms,
        } = envelope.event
        {
            self.chats[chat_idx].stream_reset();
            if chat_idx == 0 {
                self.retry_info = Some(RetryInfo {
                    attempt,
                    message,
                    deadline: Instant::now() + Duration::from_millis(delay_ms),
                });
            }
            return vec![];
        }

        self.retry_info = None;

        let plan_path = if self.state.mode == Mode::Plan {
            self.state.plan.path()
        } else {
            None
        };

        if let AgentEvent::TurnComplete(ref tc) = envelope.event {
            self.state.token_usage += tc.usage;
            self.chats[chat_idx].token_usage += tc.usage;
            let ctx_size = tc.context_size.unwrap_or_else(|| tc.usage.context_tokens());
            self.chats[chat_idx].context_size = ctx_size;
            if chat_idx == 0 {
                self.state.context_size = ctx_size;
            }
            let formatted = format_turn_usage(&tc.usage, &self.state.model.pricing);
            self.chats[chat_idx].set_pending_turn_usage(formatted);
        }

        let result = self.chats[chat_idx].handle_event(envelope.event, plan_path);

        if let ChatEventResult::QueueItemConsumed { text, image_count } = result {
            if chat_idx == 0 {
                self.on_queue_item_consumed(&text, image_count);
            }
            return vec![];
        }

        if let ChatEventResult::PermissionRequest { id, tool, scopes } = result {
            self.permission_prompt.open(id, tool, scopes, subagent_id);
            return vec![];
        }

        if chat_idx == 0 {
            match result {
                ChatEventResult::Done => {
                    self.chats[0].todo_panel.on_turn_done();
                    self.status_bar.clear_flash();
                    self.save_session();
                    self.chat_index.clear();
                    self.subagent_answers.clear();
                    self.status = Status::Idle;
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Success;
                    }
                }
                ChatEventResult::Error(message) => {
                    self.status = Status::error(message.clone());
                    self.status_bar.clear_flash();
                    self.save_session();
                    self.queue.clear();
                    self.subagent_answers.clear();
                    self.finish_subagents(DisplayRole::Error, ERROR_TEXT);
                    for chat in &mut self.chats {
                        chat.fail_in_progress_with_message(message.clone());
                    }
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Error;
                    }
                }
                ChatEventResult::QuestionPrompt { questions } => {
                    self.transition_plan(PlanTrigger::QuestionAsked);
                    if QuestionForm::is_form_suitable(&questions) {
                        self.question_form.open(questions);
                    } else {
                        let text = QuestionForm::format_questions_as_text(&questions);
                        self.main_chat()
                            .push(DisplayMessage::new(DisplayRole::Assistant, text));
                        self.pending_input = PendingInput::Question;
                    }
                }
                ChatEventResult::AuthRequired => {
                    self.main_chat().push(DisplayMessage::new(
                        DisplayRole::Error,
                        AUTH_EXPIRED_MSG.into(),
                    ));
                    self.pending_input = PendingInput::AuthRetry;
                }
                ChatEventResult::PermissionRequest { .. }
                | ChatEventResult::QueueItemConsumed { .. } => unreachable!(),
                ChatEventResult::Continue => {}
            }
        }
        vec![]
    }

    fn resolve_or_create_chat(&mut self, subagent: &SubagentInfo) -> usize {
        let id = &subagent.parent_tool_use_id;
        if let Some(&idx) = self.chat_index.get(id.as_str()) {
            return idx;
        }
        let idx = self.chats.len();
        self.chat_index.insert(id.clone(), idx);
        if let Some(ref tx) = subagent.answer_tx {
            self.subagent_answers.insert(id.clone(), tx.clone());
        }
        self.chats[0].update_tool_summary(id, &subagent.name);
        if let Some(ref model) = subagent.model {
            self.chats[0].update_tool_model(id, model);
        }
        let mut chat = Chat::new(subagent.name.clone(), self.ui_config);
        chat.model_id = subagent.model.clone();
        if let Some(ref prompt) = subagent.prompt {
            chat.push_user_message(prompt);
        }
        self.chats.push(chat);
        idx
    }

    fn execute_command(&mut self, cmd: ParsedCommand) -> Vec<Action> {
        self.input_box.discard();
        match cmd.name.as_str() {
            "/tasks" => {
                self.open_tasks();
                vec![]
            }
            "/compact" => {
                if self.status == Status::Streaming {
                    self.queue_compact();
                    return vec![];
                }
                self.status = Status::Streaming;
                vec![Action::Compact]
            }
            "/help" => {
                self.help_modal.toggle();
                vec![]
            }
            "/btw" => {
                let question = cmd.args.trim().to_string();
                if question.is_empty() {
                    self.flash("Usage: /btw <question>".into());
                    vec![]
                } else {
                    vec![Action::Btw(question)]
                }
            }
            "/new" => self.reset_session(),
            "/queue" => {
                self.queue.set_focus();
                vec![]
            }
            "/sessions" => self.open_session_picker(),
            "/model" => {
                self.model_picker.open(&self.state.model.spec());
                vec![]
            }
            "/theme" => {
                self.theme_picker.open();
                vec![]
            }
            "/mcp" => {
                self.mcp_picker.open();
                vec![]
            }
            "/cd" => self.cmd_cd(&cmd.args),
            "/memory" => self.cmd_memory(),
            "/yolo" => {
                let enabled = self.permissions.toggle_yolo();
                let msg = if enabled {
                    "YOLO mode enabled"
                } else {
                    "YOLO mode disabled"
                };
                self.flash(msg.into());
                vec![]
            }
            "/thinking" => {
                if !self.state.model.provider.supports_thinking() {
                    self.flash(
                        "Thinking requires Anthropic, Mistral, DeepSeek or Synthetic provider"
                            .into(),
                    );
                    return vec![];
                }
                match ThinkingConfig::parse(cmd.args.trim(), self.state.thinking) {
                    Ok(thinking) => {
                        self.state.thinking = thinking;
                        self.flash(format!("Thinking: {thinking}"));
                    }
                    Err(msg) => self.flash(msg.into()),
                }
                vec![]
            }
            "/exit" => self.quit(),
            name if name.starts_with("/project:") || name.starts_with("/user:") => {
                self.execute_custom_command(name, &cmd.args)
            }
            name if self.command_palette.find_mcp_prompt(name).is_some() => {
                self.execute_mcp_prompt(name, &cmd.args)
            }
            _ => vec![],
        }
    }

    fn execute_mcp_prompt(&mut self, name: &str, args: &str) -> Vec<Action> {
        let prompt = self.command_palette.find_mcp_prompt(name).unwrap().clone();

        let arguments = Self::parse_prompt_args(&prompt, args);
        let missing: Vec<_> = prompt
            .arguments
            .iter()
            .filter(|a| a.required && !arguments.contains_key(&a.name))
            .map(|a| format!("<{}>", a.name))
            .collect();
        if !missing.is_empty() {
            self.flash(format!("Usage: {} {}", name, missing.join(" ")));
            return vec![];
        }

        let prompt_ref = maki_agent::McpPromptRef {
            qualified_name: prompt.qualified_name.clone(),
            arguments,
        };
        let display_text = if args.trim().is_empty() {
            name.to_string()
        } else {
            format!("{name} {args}")
        };
        let mut input = self.build_agent_input(&QueuedMessage {
            text: display_text,
            images: Vec::new(),
        });
        input.prompt = Some(Box::new(prompt_ref));

        if self.status == Status::Streaming {
            self.flash("Agent is busy, try again later".into());
            vec![]
        } else {
            self.run_id += 1;
            self.status = Status::Streaming;
            vec![Action::SendMessage(Box::new(input))]
        }
    }

    fn parse_prompt_args(prompt: &McpPromptInfo, args: &str) -> HashMap<String, String> {
        let mut result = HashMap::new();
        let mut remaining = args.trim();
        if remaining.is_empty() || prompt.arguments.is_empty() {
            return result;
        }
        let last_idx = prompt.arguments.len() - 1;
        for (i, arg) in prompt.arguments.iter().enumerate() {
            if remaining.is_empty() {
                break;
            }
            if i == last_idx {
                result.insert(arg.name.clone(), remaining.to_string());
            } else if let Some((word, rest)) = remaining.split_once(char::is_whitespace) {
                result.insert(arg.name.clone(), word.to_string());
                remaining = rest.trim_start();
            } else {
                result.insert(arg.name.clone(), remaining.to_string());
                break;
            }
        }
        result
    }

    fn execute_custom_command(&mut self, name: &str, args: &str) -> Vec<Action> {
        let Some(cmd) = self.command_palette.find_custom_command(name) else {
            self.flash(format!("Unknown command: {name}"));
            return vec![];
        };
        let rendered = cmd.render(args);
        let msg = QueuedMessage {
            text: rendered,
            images: Vec::new(),
        };
        if self.status == Status::Streaming {
            self.queue_and_notify(msg);
            vec![]
        } else {
            self.run_id += 1;
            self.start_from_queue(&msg)
        }
    }

    fn cmd_cd(&mut self, args: &str) -> Vec<Action> {
        let path = if args.is_empty() {
            maki_storage::paths::home().unwrap_or_default()
        } else {
            match args.strip_prefix('~') {
                Some(rest) => {
                    let home = maki_storage::paths::home().unwrap_or_default();
                    if rest.is_empty() {
                        home
                    } else {
                        home.join(rest.trim_start_matches('/'))
                    }
                }
                None => PathBuf::from(args),
            }
        };
        match std::env::set_current_dir(&path) {
            Ok(()) => {
                if let Ok(canonical) = std::env::current_dir() {
                    self.state.session.cwd = canonical.to_string_lossy().into_owned();
                }
                self.status_bar.refresh_cwd();
                self.flash(format!("cd {}", path.display()))
            }
            Err(e) => self.flash(format!("cd: {e}")),
        }
        vec![]
    }

    fn cmd_memory(&mut self) -> Vec<Action> {
        let entries: Vec<MemoryEntry> = maki_agent::tools::memory::list_memory_entries()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, size)| MemoryEntry::new(name, size))
            .collect();
        self.memory_modal.open(entries);
        vec![]
    }

    fn overlays(&self) -> [&dyn Overlay; 13] {
        [
            &self.help_modal,
            &self.btw_modal,
            &self.memory_modal,
            &self.search_modal,
            &self.file_picker,
            &self.task_picker,
            &self.session_picker,
            &self.rewind_picker,
            &self.theme_picker,
            &self.model_picker,
            &self.mcp_picker,
            &self.permission_prompt,
            &self.question_form,
        ]
    }

    fn overlays_mut(&mut self) -> [&mut dyn Overlay; 13] {
        [
            &mut self.help_modal,
            &mut self.btw_modal,
            &mut self.memory_modal,
            &mut self.search_modal,
            &mut self.file_picker,
            &mut self.task_picker,
            &mut self.session_picker,
            &mut self.rewind_picker,
            &mut self.theme_picker,
            &mut self.model_picker,
            &mut self.mcp_picker,
            &mut self.permission_prompt,
            &mut self.question_form,
        ]
    }

    pub fn any_overlay_open(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open())
    }

    pub fn has_modal_overlay(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open() && o.is_modal())
    }

    pub fn close_all_overlays(&mut self) {
        self.overlays_mut().iter_mut().for_each(|o| o.close());
    }

    pub fn is_animating(&self) -> bool {
        self.image_paste_rx.is_some()
            || self.btw_modal.is_animating()
            || self.session_picker.is_loading()
            || self.file_picker.is_loading()
            || self
                .selection_state
                .as_ref()
                .is_some_and(|s| s.is_edge_scrolling())
            || self.chats.iter().any(|c| c.is_animating())
    }

    fn finish_subagents(&mut self, role: DisplayRole, text: &str) {
        for &sub_idx in self.chat_index.values() {
            self.chats[sub_idx].mark_finished(role.clone(), text);
        }
        self.chat_index.clear();
    }

    pub fn flush_all_chats(&mut self) {
        for chat in &mut self.chats {
            chat.flush();
        }
    }

    #[cfg(feature = "demo")]
    pub fn chat_index_for(&self, tool_id: &str) -> Option<usize> {
        self.chat_index.get(tool_id).copied()
    }

    #[cfg(feature = "demo")]
    pub fn set_demo_questions(&mut self, chat_idx: usize, questions: Vec<QuestionInfo>) {
        self.demo_questions = Some((chat_idx, questions));
    }

    #[cfg(feature = "demo")]
    fn check_demo_questions(&mut self) {
        if let Some((idx, _)) = &self.demo_questions
            && self.active_chat == *idx
        {
            let (_, questions) = self.demo_questions.take().unwrap();
            self.question_form.open(questions);
        }
    }

    fn route_text_paste(&mut self, text: &str) {
        if self.state.mode == Mode::Plan && self.plan_form.is_visible() {
            return;
        }
        if self.permission_prompt.handle_paste(text) {
            return;
        }
        if self.question_form.handle_paste(text) {
            return;
        }
        if self.search_modal.is_open() {
            self.search_modal.handle_paste(text);
            let chat = &mut self.chats[self.active_chat];
            let texts = chat.segment_search_texts();
            self.search_modal.update_matches(&texts);
            sync_search_highlight(&self.search_modal, chat);
            return;
        }
        macro_rules! try_picker {
            ($picker:expr) => {
                if $picker.handle_paste(text) {
                    return;
                }
            };
        }
        try_picker!(self.memory_modal);
        try_picker!(self.file_picker);
        try_picker!(self.task_picker);
        try_picker!(self.session_picker);
        try_picker!(self.rewind_picker);
        try_picker!(self.theme_picker);
        try_picker!(self.model_picker);
        try_picker!(self.mcp_picker);
        if !self.is_main_chat() {
            return;
        }
        if let InputAction::PaletteSync(val) = self.input_box.handle_paste(text) {
            self.command_palette.sync(&val);
        }
    }

    fn handle_plan_form_action(&mut self, action: PlanFormAction) -> Vec<Action> {
        match action {
            PlanFormAction::Consumed | PlanFormAction::Passthrough => vec![],
            PlanFormAction::Hide => {
                self.plan_form.hide();
                vec![]
            }
            PlanFormAction::OpenEditor => match self.state.plan.path() {
                Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
                None => {
                    self.flash(FLASH_NO_PLAN.into());
                    vec![]
                }
            },
            PlanFormAction::Implement => self.implement_plan(false),
            PlanFormAction::ClearAndImplement => self.implement_plan(true),
        }
    }

    fn open_memory_file(&mut self, filename: &str) -> Vec<Action> {
        match maki_agent::tools::memory::resolve_memories_dir() {
            Ok(dir) => vec![Action::OpenEditor(dir.join(filename))],
            Err(e) => {
                self.flash(e);
                vec![]
            }
        }
    }

    fn delete_memory_file(&mut self, filename: &str) -> Vec<Action> {
        match maki_agent::tools::memory::resolve_memories_dir() {
            Ok(dir) => match std::fs::remove_file(dir.join(filename)) {
                Ok(()) => {
                    let owned = filename.to_owned();
                    self.memory_modal.retain(|e| e.name != owned);
                    self.flash(format!("Deleted memory file: {filename}"));
                }
                Err(e) => self.flash(format!("Failed to delete {filename}: {e}")),
            },
            Err(e) => self.flash(e),
        }
        vec![]
    }

    fn implement_plan(&mut self, clear_context: bool) -> Vec<Action> {
        self.plan_form.reset();
        let plan_snapshot = match std::mem::take(&mut self.state.plan) {
            PlanState::Ready(p) => Some((
                std::fs::read_to_string(&p).unwrap_or_default(),
                p.display().to_string(),
            )),
            _ => None,
        };

        self.state.mode = Mode::Build;

        let mut actions = if clear_context {
            self.reset_session()
        } else {
            vec![]
        };

        let text = if let Some((content, path_str)) = plan_snapshot {
            let text = format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`. {IMPLEMENT_PARALLEL_HINT}");
            self.main_chat()
                .push(DisplayMessage::plan(content, path_str));
            text
        } else {
            format!("{}.", IMPLEMENT_MSG_PREFIX)
        };
        self.run_id += 1;
        let msg = QueuedMessage {
            text,
            images: vec![],
        };
        actions.extend(self.start_from_queue(&msg));
        actions
    }
}

fn sync_search_highlight(modal: &SearchModal, chat: &mut Chat) {
    let idx = modal.current_segment_index();
    if let Some(i) = idx {
        chat.scroll_to_segment(i);
    }
    chat.set_highlight_segment(idx);
}

fn format_with_images(text: &str, image_count: usize) -> String {
    match image_count {
        0 => text.to_string(),
        1 => format!("{text} [1 image]"),
        n => format!("{text} [{n} images]"),
    }
}
