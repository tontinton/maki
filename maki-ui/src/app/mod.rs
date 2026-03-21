//! Elm-style `update(Msg) -> Vec<Action>`; side effects are dispatched by the caller.
//! Double-esc: first esc flashes a hint, second within `flash_duration` cancels/rewinds.
//! `run_id` increments each run so stale events from previous agent runs are ignored.

mod btw;
mod image_paste;
mod mode;
mod mouse;
mod queue;
mod session;
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
use crate::components::btw_modal::BtwModal;
use crate::components::command::{CommandAction, CommandPalette, ParsedCommand};
use crate::components::help_modal::HelpModal;
use crate::components::input::{InputAction, InputBox, Submission};
use crate::components::keybindings::key;
use crate::components::list_picker::{ListPicker, PickerAction};
use crate::components::mcp_picker::{McpPicker, McpPickerAction};
use crate::components::memory_modal::{MemoryEntry, MemoryModal, MemoryModalAction};
use crate::components::model_picker::{ModelPicker, ModelPickerAction};
use crate::components::permission_prompt::{PermissionAction, PermissionPrompt};
use crate::components::plan_form::{PlanForm, PlanFormAction};
use crate::components::question_form::{QuestionForm, QuestionFormAction};
use crate::components::rewind_picker::{RewindPicker, RewindPickerAction};
use crate::components::search_modal::{SearchAction, SearchModal};
use crate::components::session_picker::{SessionPicker, SessionPickerAction};
use crate::components::status_bar::StatusBar;
use crate::components::theme_picker::{ThemePicker, ThemePickerAction};
use crate::components::todo_panel::TodoPanel;
use crate::components::tool_display::format_turn_usage;
use crate::components::{Action, DisplayMessage, DisplayRole, Overlay, RetryInfo, Status, is_ctrl};
use crate::image;
use crate::selection::{SelectionState, SelectionZone, ZoneRegistry};
use arboard::Clipboard;
use arc_swap::{ArcSwap, ArcSwapOption};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
#[cfg(feature = "demo")]
use maki_agent::QuestionInfo;
use maki_agent::permissions::{PermissionDecision, PermissionManager};
use maki_agent::{AgentEvent, Envelope, ImageSource, McpServerInfo, SubagentInfo, ToolOutput};
use maki_config::UiConfig;
use maki_providers::{Message, Model, ModelPricing, TokenUsage};
use maki_storage::DataDir;
use maki_storage::input_history::InputHistory;
use maki_storage::model::persist_model;

use crate::storage_writer::StorageWriter;
use ratatui::layout::Position;

pub(crate) use mode::{Mode, PlanState};
#[cfg(test)]
use mouse::{EDGE_SCROLL_INTERVAL, EDGE_SCROLL_LINES};
pub(crate) use queue::{MessageQueue, QueuedItem, QueuedMessage};

const CANCEL_MSG: &str = "Cancelled.";
const FLASH_CANCEL: &str = "Press esc again to stop...";
const FLASH_REWIND: &str = "Press esc again to rewind...";
const AUTH_EXPIRED_MSG: &str =
    "Token expired. Run `maki auth login` in another terminal, then press Enter to retry.";
const FLASH_NO_PLAN: &str = "No plan file";
const IMPLEMENT_MSG_PREFIX: &str = "Implement the plan";

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
    pub(super) task_picker: ListPicker<String>,
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
    pub(super) todo_panel: TodoPanel,
    pub(super) permission_prompt: PermissionPrompt,
    pub(super) question_form: QuestionForm,
    pub(super) plan_form: PlanForm,
    pub(super) status_bar: StatusBar,
    pub status: Status,
    pub token_usage: TokenUsage,
    pub(crate) mode: Mode,
    pub(crate) plan: PlanState,
    pub(super) model_id: String,
    pub(super) pricing: ModelPricing,
    pub(super) context_window: u32,
    pub should_quit: bool,
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
    pub(super) clipboard: Option<Clipboard>,
    pub(super) last_esc: Option<Instant>,
    pub(crate) session: AppSession,
    pub(crate) storage: DataDir,
    pub(crate) shared_history: Option<Arc<ArcSwap<Vec<Message>>>>,
    pub(crate) shared_tool_outputs: Option<Arc<Mutex<HashMap<String, ToolOutput>>>>,
    pub(crate) image_paste_rx: Option<flume::Receiver<Result<ImageSource, String>>>,
    storage_writer: Arc<StorageWriter>,
    pub(crate) shell: shell::ShellState,
    pub(crate) ui_config: UiConfig,
    pub(crate) permissions: Arc<PermissionManager>,
    subagent_answers: HashMap<String, flume::Sender<String>>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_id: String,
        pricing: ModelPricing,
        context_window: u32,
        session: AppSession,
        storage: DataDir,
        available_models: Arc<ArcSwapOption<Vec<String>>>,
        mcp_infos: Arc<ArcSwap<Vec<McpServerInfo>>>,
        storage_writer: Arc<StorageWriter>,
        ui_config: UiConfig,
        input_history_size: usize,
        permissions: Arc<PermissionManager>,
    ) -> Self {
        Self {
            chats: vec![Chat::new("Main".into(), ui_config)],
            active_chat: 0,
            chat_index: HashMap::new(),
            input_box: InputBox::new(InputHistory::load(&storage, input_history_size)),
            command_palette: CommandPalette::new(),
            task_picker: ListPicker::new(),
            task_picker_original: None,
            theme_picker: ThemePicker::new(),
            model_picker: ModelPicker::new(available_models),
            mcp_picker: McpPicker::new(mcp_infos),
            session_picker: SessionPicker::new(),
            rewind_picker: RewindPicker::new(),
            help_modal: HelpModal::new(),
            btw_modal: BtwModal::new(ui_config.typewriter_ms_per_char),
            memory_modal: MemoryModal::new(),
            search_modal: SearchModal::new(),
            todo_panel: TodoPanel::new(),
            permission_prompt: PermissionPrompt::new(),
            question_form: QuestionForm::new(),
            plan_form: PlanForm::new(),
            status_bar: StatusBar::new(ui_config.flash_duration()),
            status: Status::Idle,
            token_usage: TokenUsage::default(),
            mode: Mode::Build,
            plan: PlanState::new(),
            model_id,
            pricing,
            context_window,
            should_quit: false,
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
            clipboard: Clipboard::new().ok(),
            last_esc: None,
            session,
            storage,
            shared_history: None,
            shared_tool_outputs: None,
            image_paste_rx: None,
            storage_writer,
            shell: shell::ShellState::default(),
            ui_config,
            permissions,
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
        self.session.model = model.spec();
        self.model_id = model.spec();
        self.pricing = model.pricing.clone();
        self.context_window = model.context_window;
        persist_model(&self.storage, &self.model_id);
    }

    pub(crate) fn flash(&mut self, msg: String) {
        self.status_bar.flash(msg);
    }

    pub fn tick_error_expiry(&mut self) {
        if self.status.is_error_expired() {
            self.status = Status::Idle;
        }
    }

    fn active_chat(&mut self) -> &mut Chat {
        &mut self.chats[self.active_chat]
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        match msg {
            Msg::Key(key) => self.handle_key(key),
            Msg::Paste(text) => {
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
                self.selection_state = None;
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
        if let Some(zone) = self.zone_at(row, column) {
            self.scroll_zone(zone.zone, delta);
        }
    }

    /// Ctrl shortcuts that work regardless of form/overlay state.
    fn handle_global_ctrl(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if !is_ctrl(&key) {
            return None;
        }
        if key::QUIT.matches(key) {
            if self.any_overlay_open() || self.queue.focus().is_some() {
                return None;
            }
            self.command_palette.close();
            return Some(
                if !self.is_main_chat() || self.input_box.buffer.value().trim().is_empty() {
                    self.quit()
                } else {
                    self.input_box.discard();
                    vec![]
                },
            );
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
            if let Some(action) = self.permission_prompt.handle_key(key) {
                let subagent_id = self.permission_prompt.subagent_id().map(str::to_owned);
                let decision = match action {
                    PermissionAction::AllowOnce => PermissionDecision::AllowOnce,
                    PermissionAction::AllowSession => PermissionDecision::AllowSession,
                    PermissionAction::AllowAlwaysLocal => PermissionDecision::AllowAlwaysLocal,
                    PermissionAction::AllowAlwaysGlobal => PermissionDecision::AllowAlwaysGlobal,
                    PermissionAction::Deny => PermissionDecision::DenyOnce,
                    PermissionAction::DenyAlwaysLocal => PermissionDecision::DenyAlwaysLocal,
                    PermissionAction::DenyAlwaysGlobal => PermissionDecision::DenyAlwaysGlobal,
                };
                let answer = decision.to_answer_str().to_string();
                self.permission_prompt.close();
                if let Some(tx) = subagent_id.and_then(|id| self.subagent_answers.get(&id)) {
                    let _ = tx.try_send(answer);
                } else {
                    self.send_answer(answer);
                }
            }
            return vec![];
        }

        if self.question_form.is_visible() {
            let action = self.question_form.handle_key(key);
            let answer = match action {
                QuestionFormAction::Submit(a) => {
                    let display = self.question_form.format_answers_display();
                    self.main_chat().push_user_message(&display);
                    a
                }
                QuestionFormAction::Dismiss => String::new(),
                QuestionFormAction::Consumed => return vec![],
            };
            self.question_form.close();
            self.send_answer(answer);
            return vec![];
        }

        if self.plan_form.is_visible() {
            let action = self.plan_form.handle_key(key);
            return self.handle_plan_form_action(action);
        }

        self.selection_state = None;

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
                MemoryModalAction::Close | MemoryModalAction::Consumed => {}
            }
            return vec![];
        }

        if self.search_modal.is_open() {
            match self.search_modal.handle_key(key) {
                SearchAction::Consumed => {
                    let chat = &mut self.chats[self.active_chat];
                    let texts = chat.segment_copy_texts();
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
                    self.status_bar
                        .flash("Press Ctrl+D again to confirm delete".into());
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
            return vec![];
        }

        self.handle_main_chat_key(key)
    }

    fn handle_main_chat_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if is_ctrl(&key) {
            if key::POP_QUEUE.matches(key) {
                self.queue.remove(0);
            } else if key::OPEN_EDITOR.matches(key) {
                return match self.plan.pending_plan() {
                    Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
                    None => {
                        self.flash(FLASH_NO_PLAN.into());
                        vec![]
                    }
                };
            } else if key::TODO_PANEL.matches(key) {
                self.todo_panel.toggle();
            } else if key::SEARCH.matches(key) {
                let top = self.chats[self.active_chat].scroll_top();
                let auto = self.chats[self.active_chat].auto_scroll();
                self.search_modal.open(top, auto);
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

    fn quit(&mut self) -> Vec<Action> {
        self.save_input_history();
        self.should_quit = true;
        vec![Action::Quit]
    }

    fn handle_submit(&mut self, sub: Submission) -> Vec<Action> {
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
        if sub.text.trim() == "exit" {
            return self.quit();
        }

        if let Some(prefix) = shell::parse_shell_prefix(&sub.text) {
            let id = self.shell.next_id();
            let sigil = if prefix.visible { "!" } else { "!!" };
            let display = format!("{sigil} {}", prefix.command);
            self.main_chat().flush();
            self.main_chat().push_user_message(&display);
            self.main_chat().enable_auto_scroll();
            return vec![Action::ShellCommand {
                id,
                command: prefix.command,
                visible: prefix.visible,
            }];
        }
        let msg: QueuedMessage = sub.into();
        if self.status == Status::Streaming {
            self.queue_and_notify(QueuedItem::Message(msg));
            vec![]
        } else {
            self.run_id += 1;
            self.start_from_queue(&msg)
        }
    }

    fn handle_cancel(&mut self) -> Vec<Action> {
        self.run_id += 1;
        self.retry_info = None;
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        self.finish_subagents(DisplayRole::Error, CANCELLED_TEXT);
        self.subagent_answers.clear();
        self.shell.cancel_all();
        for chat in &mut self.chats {
            chat.flush();
            chat.fail_in_progress();
        }
        self.main_chat()
            .push(DisplayMessage::new(DisplayRole::Error, CANCEL_MSG.into()));
        self.queue.clear();
        self.status = Status::Idle;
        vec![Action::CancelAgent]
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

        let subagent_id = envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.clone());

        let chat_idx = match envelope.subagent {
            Some(ref subagent) => self.resolve_or_create_chat(subagent),
            None => 0,
        };

        if let AgentEvent::ToolDone(ref e) = envelope.event {
            if self.mode == Mode::Plan && self.plan.path().is_some_and(|pp| e.wrote_to(pp)) {
                self.plan.mark_written();
            }
            if let Some(ref outputs) = self.shared_tool_outputs {
                outputs
                    .lock()
                    .unwrap()
                    .insert(e.id.clone(), e.output.clone());
            }
            if let ToolOutput::TodoList(ref items) = e.output {
                self.todo_panel.on_todowrite(items);
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

        let plan_path = if self.mode == Mode::Plan {
            self.plan.path()
        } else {
            None
        };

        if let AgentEvent::TurnComplete(ref tc) = envelope.event {
            self.token_usage += tc.usage;
            self.chats[chat_idx].token_usage += tc.usage;
            self.chats[chat_idx].context_size =
                tc.context_size.unwrap_or_else(|| tc.usage.context_tokens());
            let formatted = format_turn_usage(&tc.usage, &self.pricing);
            self.chats[chat_idx].set_pending_turn_usage(formatted);
        }

        let result = self.chats[chat_idx].handle_event(envelope.event, plan_path);

        if matches!(result, ChatEventResult::QueueItemConsumed) && chat_idx == 0 {
            self.on_queue_item_consumed();
            return vec![];
        }

        if let ChatEventResult::PermissionRequest { id, tool, scope } = result {
            self.permission_prompt.open(id, tool, scope, subagent_id);
            return vec![];
        }

        if chat_idx == 0 {
            match result {
                ChatEventResult::Done => {
                    self.todo_panel.on_turn_done();
                    self.status_bar.clear_flash();
                    self.save_session();
                    self.chat_index.clear();
                    self.subagent_answers.clear();
                    if let Some(actions) = self.on_agent_done() {
                        return actions;
                    }
                    self.status = Status::Idle;
                    if self.mode == Mode::Plan && self.plan.pending_plan().is_some() {
                        self.plan_form.open();
                    }
                }
                ChatEventResult::Error(message) => {
                    self.status = Status::error(message);
                    self.status_bar.clear_flash();
                    self.save_session();
                    self.queue.clear();
                    self.subagent_answers.clear();
                    self.finish_subagents(DisplayRole::Error, ERROR_TEXT);
                    for chat in &mut self.chats {
                        chat.fail_in_progress();
                    }
                }
                ChatEventResult::QuestionPrompt { questions } => {
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
                ChatEventResult::PermissionRequest { .. } => unreachable!(),
                ChatEventResult::Continue | ChatEventResult::QueueItemConsumed => {}
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
        match cmd.name {
            "/tasks" => {
                let names: Vec<String> = self.chats.iter().map(|c| c.name.clone()).collect();
                self.task_picker_original = Some(self.active_chat);
                self.task_picker.open(names, " Tasks ");
                self.task_picker.select(self.active_chat);
                vec![]
            }
            "/compact" => {
                if self.status == Status::Streaming {
                    self.queue_and_notify(QueuedItem::Compact);
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
                self.model_picker.open();
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
            "/exit" => self.quit(),
            _ => vec![],
        }
    }

    fn cmd_cd(&mut self, args: &str) -> Vec<Action> {
        let path = if args.is_empty() {
            dirs::home_dir().unwrap_or_default()
        } else {
            match args.strip_prefix('~') {
                Some(rest) => {
                    let home = dirs::home_dir().unwrap_or_default();
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
                    self.session.cwd = canonical.to_string_lossy().into_owned();
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
            &self.task_picker,
            &self.session_picker,
            &self.rewind_picker,
            &self.theme_picker,
            &self.model_picker,
            &self.mcp_picker,
            &self.permission_prompt,
            &self.question_form,
            &self.plan_form,
        ]
    }

    fn overlays_mut(&mut self) -> [&mut dyn Overlay; 13] {
        [
            &mut self.help_modal,
            &mut self.btw_modal,
            &mut self.memory_modal,
            &mut self.search_modal,
            &mut self.task_picker,
            &mut self.session_picker,
            &mut self.rewind_picker,
            &mut self.theme_picker,
            &mut self.model_picker,
            &mut self.mcp_picker,
            &mut self.permission_prompt,
            &mut self.question_form,
            &mut self.plan_form,
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
            || self
                .selection_state
                .as_ref()
                .is_some_and(|s| s.edge_scroll.is_some())
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
        if self.plan_form.is_visible() {
            return;
        }
        if self.question_form.handle_paste(text) {
            return;
        }
        if self.search_modal.is_open() {
            self.search_modal.handle_paste(text);
            let chat = &mut self.chats[self.active_chat];
            let texts = chat.segment_copy_texts();
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
        if !matches!(action, PlanFormAction::Consumed) {
            self.plan_form.close();
        }
        match action {
            PlanFormAction::Consumed | PlanFormAction::Dismiss | PlanFormAction::Continue => {
                vec![]
            }
            PlanFormAction::OpenEditor => match self.plan.path() {
                Some(p) => {
                    self.plan_form.open();
                    vec![Action::OpenEditor(p.to_path_buf())]
                }
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
        let mut actions = if clear_context {
            self.reset_session()
        } else {
            vec![]
        };
        if let Some(pp) = self.plan.path() {
            let content = std::fs::read_to_string(pp).unwrap_or_default();
            let path_str = pp.display().to_string();
            self.main_chat()
                .push(DisplayMessage::plan(content, path_str));
        }
        let text = match self.plan.path() {
            Some(p) => format!("{} at `{}`.", IMPLEMENT_MSG_PREFIX, p.display()),
            None => format!("{}.", IMPLEMENT_MSG_PREFIX),
        };
        self.mode = Mode::Build;
        self.run_id += 1;
        let msg = QueuedMessage {
            text,
            images: vec![],
        };
        actions.extend(self.start_from_queue(&msg));
        self.plan = PlanState::new();
        actions
    }
}

fn sync_search_highlight(modal: &SearchModal, chat: &mut Chat) {
    let seg_idx = modal.current_segment_index();
    if let Some(idx) = seg_idx {
        chat.scroll_to_segment(idx);
    }
    chat.set_highlight_segment(seg_idx);
}

fn format_with_images(text: &str, image_count: usize) -> String {
    match image_count {
        0 => text.to_string(),
        1 => format!("{text} [1 image]"),
        n => format!("{text} [{n} images]"),
    }
}
