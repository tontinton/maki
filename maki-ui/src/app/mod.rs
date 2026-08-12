//! Elm-style `update(Msg) -> Vec<Action>`; side effects are dispatched by the caller.
//! Double-esc: first esc flashes a hint, second within `flash_duration` cancels/rewinds.
//! `run_id` invalidates in-flight agent events. It bumps in exactly three
//! places, one per transition: `start_run`, `handle_cancel`, and
//! `AgentHandles::respawn`. Everything else only reads it.

mod btw;
mod image_paste;
pub(crate) mod mode;
mod mouse;
mod queue;
mod session;
pub(crate) mod session_state;
pub(crate) mod shell;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod view;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::components::login_picker::{LoginPicker, LoginPickerAction};
use crate::components::lua_float::FloatManager;
use crate::components::mcp_picker::{McpPicker, McpPickerAction};
use crate::components::model_picker::{ModelPicker, ModelPickerAction};
use crate::components::permission_prompt::PermissionPrompt;
use crate::components::plan_form::{PlanForm, PlanFormAction};
use crate::components::rewind_picker::{RewindPicker, RewindPickerAction};
use crate::components::scrollbar;
use crate::components::search_modal::{SearchAction, SearchModal};
use crate::components::status_bar::StatusBar;
use crate::components::theme_picker::{ThemePicker, ThemePickerAction};
use crate::components::usage_modal::{UsageFetchState, UsageModal};
use crate::components::{
    Action, DisplayMessage, DisplayRole, ExitRequest, Overlay, RetryInfo, Status, is_ctrl,
};
use crate::image;
use crate::selection::{SelectionState, SelectionZone, ZoneRegistry};
use arc_swap::{ArcSwap, ArcSwapOption};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    AgentEvent, Envelope, ImageSource, McpConfigErrors, McpPromptInfo, McpSnapshotReader,
    SharedMessages, SubagentInfo,
};
use maki_config::{ModelPolicy, UiConfig};
use maki_lua::{BuiltinAction, EventHandle, HintReader, KeymapReader, LuaCommandReader, WinView};
use maki_providers::{Model, ThinkingConfig, add_cost};
use maki_storage::StateDir;
use maki_storage::input_history::InputHistory;
use maki_storage::model::persist_model;

use crate::storage_writer::StorageWriter;
use ratatui::layout::Position;

pub(crate) use crate::agent::QueuedMessage;
pub(crate) use mode::{Mode, PlanState, PlanTrigger};
#[cfg(test)]
use mouse::EDGE_SCROLL_LINES;
pub(crate) use queue::{MessageQueue, SubmitOutcome};
use session::Sent;
pub(crate) use session::session_has_content;
use session_state::SessionState;

const CANCEL_MSG: &str = "Cancelled.";
/// Bypasses the per-run staleness filter because re-bake replies
/// don't belong to any real agent run.
pub(crate) const RESTORE_RUN_ID: u64 = u64::MAX;
const FLASH_CANCEL: &str = "Press esc again to stop...";
const FLASH_REWIND: &str = "Press esc again to rewind...";
const AUTH_EXPIRED_MSG: &str =
    "Token expired. Run `maki auth login` in another terminal, then press Enter to retry.";
const FLASH_NO_PLAN: &str = "No plan file";
const FAST_UNSUPPORTED_MSG: &str = "Fast mode requires an Anthropic Opus 4.6+ model (API only)";
const FAST_ON_MSG: &str = "Fast mode: on";
const FAST_OFF_MSG: &str = "Fast mode: off";
const WORKFLOW_ON_MSG: &str = "Workflow mode: on";
const WORKFLOW_OFF_MSG: &str = "Workflow mode: off";
const IMPLEMENT_MSG_PREFIX: &str = "Implement the plan";
const IMPLEMENT_PARALLEL_HINT: &str = "Use batch+task to parallelize, assign each subagent a separate module and restrict its tests to that module to avoid interference.";

const TASK_DONE_DETAIL: &str = "✓ ";
const MISSING_TOOL_COMPLETION: &str = "Tool did not report completion before the turn ended";

#[derive(Clone)]
pub(super) struct TaskEntry {
    name: String,
    finished: Option<bool>,
    chat_index: usize,
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) enum PendingInput {
    #[default]
    None,
    AuthRetry {
        subagent_id: Option<String>,
    },
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
    pub(super) login_picker: LoginPicker,
    pub(super) mcp_picker: McpPicker,
    pub(super) rewind_picker: RewindPicker,
    pub(super) help_modal: HelpModal,
    pub(super) usage_modal: UsageModal,
    pub(super) btw_modal: BtwModal,
    pub(super) float_mgr: FloatManager,
    pub(super) search_modal: SearchModal,
    pub(super) file_picker: FilePickerModal,
    pub(super) permission_prompt: PermissionPrompt,
    pub(super) plan_form: PlanForm,
    pub(super) status_bar: StatusBar,
    pub status: Status,
    pub(crate) state: session_state::SessionState,
    pub exit_request: ExitRequest,
    pub(crate) exit_on_done: bool,
    pub(crate) queue: MessageQueue,
    recoverable_queue: Vec<String>,
    pub answer_tx: Option<flume::Sender<String>>,
    pub(crate) cmd_tx: Option<flume::Sender<super::AgentCommand>>,
    pub(super) pending_input: PendingInput,
    pub(crate) run_id: u64,
    pub(super) retry_info: Option<RetryInfo>,
    pub(super) zones: ZoneRegistry,
    pub(super) selection_state: Option<SelectionState>,
    pub(super) clipboard: ClipboardState,
    pub(super) last_esc: Option<Instant>,

    pub(crate) storage: StateDir,
    pub(crate) usage_slot: Arc<ArcSwapOption<UsageFetchState>>,
    pub(crate) shared_history: Option<SharedMessages>,
    pub(crate) btw_system: Option<Arc<ArcSwap<String>>>,
    pub(crate) image_paste_rx: Vec<flume::Receiver<Result<ImageSource, String>>>,
    storage_writer: Arc<StorageWriter>,
    last_sent: Option<Sent>,
    pub(crate) shell: shell::ShellState,
    pub(crate) ui_config: UiConfig,
    pub(crate) permissions: Arc<PermissionManager>,
    pub(crate) model_policy: Arc<ModelPolicy>,
    pub(crate) lua_event_handle: EventHandle,
    pub(super) keymap_reader: KeymapReader,
    pub(super) hint_reader: HintReader,
    pub(crate) restore_event_tx: Option<maki_agent::EventSender>,
    pub(super) restoring: Arc<AtomicBool>,
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
        mcp_config_errors: McpConfigErrors,
        lua_command_reader: LuaCommandReader,
        keymap_reader: KeymapReader,
        hint_reader: HintReader,
        storage_writer: Arc<StorageWriter>,
        ui_config: UiConfig,
        input_history_size: usize,
        permissions: Arc<PermissionManager>,
        custom_commands: Arc<[maki_agent::command::CustomCommand]>,
        lua_event_handle: EventHandle,
        model_policy: Arc<ModelPolicy>,
    ) -> Self {
        scrollbar::set_enabled(ui_config.scrollbar);
        let state = SessionState::from_session(session, model, &storage, &model_policy);
        let typewriter = ui_config.typewriter_ms_per_char;
        let flash = ui_config.flash_duration();
        let input_box = InputBox::new(
            InputHistory::load(&storage, input_history_size),
            ui_config.max_input_lines,
        );
        let mut app = Self {
            chats: vec![Chat::new(
                "Main".into(),
                ui_config.clone(),
                lua_event_handle.clone(),
            )],
            active_chat: 0,
            chat_index: HashMap::new(),
            input_box,
            command_palette: CommandPalette::new(
                custom_commands,
                mcp_reader.clone(),
                lua_command_reader,
            ),
            task_picker: ListPicker::new(),
            task_picker_original: None,
            theme_picker: ThemePicker::new(),
            model_picker: ModelPicker::new(available_models),
            login_picker: LoginPicker::new(),
            mcp_picker: McpPicker::new(mcp_reader, mcp_config_errors),
            rewind_picker: RewindPicker::new(),
            help_modal: HelpModal::new(),
            usage_modal: UsageModal::new(),
            btw_modal: BtwModal::new(typewriter),
            float_mgr: FloatManager::new(),
            search_modal: SearchModal::new(),
            file_picker: FilePickerModal::new(),
            permission_prompt: PermissionPrompt::new(),
            plan_form: PlanForm::new(),
            status_bar: StatusBar::new(flash),
            status: Status::Idle,
            state,
            exit_request: ExitRequest::None,
            exit_on_done: false,
            queue: MessageQueue::default(),
            recoverable_queue: Vec::new(),
            answer_tx: None,
            cmd_tx: None,
            pending_input: PendingInput::None,
            run_id: 0,
            retry_info: None,
            zones: ZoneRegistry::new(),
            selection_state: None,
            clipboard: ClipboardState::new(),
            last_esc: None,
            storage,
            usage_slot: Arc::new(ArcSwapOption::empty()),
            shared_history: None,
            btw_system: None,
            image_paste_rx: vec![],
            storage_writer,
            last_sent: None,
            shell: shell::ShellState::default(),
            ui_config,
            permissions,
            model_policy: Arc::clone(&model_policy),
            lua_event_handle,
            keymap_reader,
            hint_reader,
            restore_event_tx: None,
            restoring: Arc::new(AtomicBool::new(false)),
            subagent_answers: HashMap::new(),
        };
        app.model_picker.set_recents(
            maki_storage::model::read_recents(&app.storage)
                .into_iter()
                .filter(|spec| model_policy.allows(spec))
                .collect(),
        );
        app
    }

    pub(crate) fn main_chat(&mut self) -> &mut Chat {
        &mut self.chats[0]
    }

    fn is_main_chat(&self) -> bool {
        self.active_chat == 0
    }

    fn plan_form_active(&self) -> bool {
        self.state.mode == Mode::Plan && self.plan_form.is_visible()
    }

    pub(crate) fn update_model(&mut self, model: &Model) {
        self.state.update_model(model);
        persist_model(&self.storage, &self.state.session.model);
    }

    pub(crate) fn record_recent_model(&mut self, spec: &str) {
        let recents = maki_storage::model::push_recent(&self.storage, spec)
            .into_iter()
            .filter(|spec| self.model_policy.allows(spec))
            .collect();
        self.model_picker.set_recents(recents);
    }

    pub(crate) fn flash(&mut self, msg: String) {
        self.status_bar.flash(msg);
    }

    pub(crate) fn fire_session_autocmd(&self, event: &str, mut data: serde_json::Value) {
        if let Some(map) = data.as_object_mut() {
            map.insert(
                "session_id".into(),
                serde_json::Value::String(self.state.session.id.to_string()),
            );
        }
        self.lua_event_handle.fire_autocmd(event, data);
    }

    pub fn tick_error_expiry(&mut self) {
        if self.status.is_error_expired() {
            self.status = Status::Idle;
        }
    }

    fn active_chat(&mut self) -> &mut Chat {
        &mut self.chats[self.active_chat]
    }

    pub(crate) fn win_view(&self) -> WinView {
        self.chats[self.active_chat].win_view()
    }

    pub(crate) fn set_scroll_top(&mut self, top: u16) {
        self.active_chat().set_scroll_top(top);
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
                    if self.is_main_chat() && self.image_paste_rx.is_empty() {
                        self.start_image_paste();
                    }
                } else {
                    let mut any_image = false;
                    if self.is_main_chat() {
                        for line in text.lines() {
                            if let Some((path, mt)) = image::try_parse_image_path(line) {
                                self.start_file_image_paste(path, mt);
                                any_image = true;
                            }
                        }
                    }
                    if !any_image {
                        self.route_text_paste(&text);
                    }
                }
                vec![]
            }
            Msg::Mouse(event) => {
                self.handle_mouse(event);
                vec![]
            }
            Msg::Scroll { column, row, delta } => {
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

    fn send_to_agent(&self, subagent_id: Option<&str>, answer: String) {
        let routed = subagent_id.and_then(|id| self.subagent_answers.get(id));
        if let Some(tx) = routed {
            let _ = tx.try_send(answer);
        } else {
            self.send_answer(answer);
        }
    }

    fn scroll_at(&mut self, column: u16, row: u16, delta: i32) -> Option<SelectionZone> {
        if self.btw_modal.is_open() {
            self.btw_modal.scroll(delta);
            return None;
        }
        if self.help_modal.is_open() {
            self.help_modal.scroll(delta);
            return None;
        }
        if self.usage_modal.is_open() {
            self.usage_modal.scroll(delta);
            return None;
        }
        let pos = Position::new(column, row);
        if self.float_mgr.is_open() && self.float_mgr.contains(pos) {
            self.float_mgr.scroll(delta);
            return None;
        }
        macro_rules! try_picker {
            ($picker:expr) => {
                if $picker.is_open() {
                    if $picker.contains(pos) {
                        $picker.scroll(delta);
                    }
                    return None;
                }
            };
        }
        try_picker!(self.rewind_picker);
        try_picker!(self.task_picker);
        try_picker!(self.model_picker);
        try_picker!(self.file_picker);
        let zone = self.zone_at(row, column)?.zone;
        self.scroll_zone(zone, delta);
        Some(zone)
    }

    fn task_entries(&self) -> Vec<TaskEntry> {
        self.chats
            .iter()
            .enumerate()
            .map(|(chat_index, chat)| TaskEntry {
                name: chat.name.clone(),
                finished: (chat_index > 0).then_some(chat.is_finished()),
                chat_index,
            })
            .collect()
    }

    fn open_tasks(&mut self) {
        self.task_picker_original = Some(self.active_chat);
        self.task_picker.open(self.task_entries(), " Tasks ");
        self.task_picker.select(self.active_chat);
    }

    fn sync_task_picker(&mut self) {
        if !self.task_picker.is_open() {
            return;
        }
        let selected = self
            .task_picker
            .selected_item()
            .map(|entry| entry.chat_index);
        self.task_picker.replace_items(self.task_entries());
        if let Some(chat_index) = selected {
            self.task_picker
                .select_item_by(|entry| entry.chat_index == chat_index);
        }
    }

    fn handle_ctrl(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if !is_ctrl(&key) {
            return None;
        }
        if key::QUIT.matches(key) {
            self.command_palette.close();
            return Some(if !self.is_main_chat() || self.input_box.is_empty() {
                if self.status == Status::Streaming {
                    return Some(self.handle_cancel());
                }
                self.quit()
            } else {
                self.input_box.discard();
                vec![]
            });
        }
        if key::HELP.matches(key) {
            return Some(self.run_builtin(BuiltinAction::Help));
        }
        if key::TASKS.matches(key) {
            return Some(self.run_builtin(BuiltinAction::Tasks));
        }
        if key::PREV_CHAT.matches(key) {
            return Some(self.run_builtin(BuiltinAction::PrevChat));
        }
        if key::NEXT_CHAT.matches(key) {
            return Some(self.run_builtin(BuiltinAction::NextChat));
        }
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
        if key::SCROLL_TOP.matches(key) {
            self.active_chat().scroll_to_top();
            return Some(vec![]);
        }
        if key::SCROLL_BOTTOM.matches(key) {
            self.active_chat().enable_auto_scroll();
            return Some(vec![]);
        }
        None
    }

    fn dispatch_overlay(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if self.permission_prompt.is_open() {
            if let Some(answer) = self.permission_prompt.handle_key(key) {
                let subagent_id = self.permission_prompt.subagent_id().map(str::to_owned);
                let encoded = answer.encode();
                self.permission_prompt.close();
                self.send_to_agent(subagent_id.as_deref(), encoded);
            }
            return Some(vec![]);
        }

        // plan_form is non-modal: Passthrough falls through to the rest of dispatch
        if self.plan_form_active() {
            let action = self.plan_form.handle_key(key);
            if action != PlanFormAction::Passthrough {
                return Some(self.handle_plan_form_action(action));
            }
        }

        if self.help_modal.is_open() {
            self.help_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.usage_modal.is_open() {
            if key::REFRESH.matches(key) {
                return Some(vec![Action::RefreshUsage]);
            }
            self.usage_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.btw_modal.is_open() {
            self.btw_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.float_mgr.handle_key(key) {
            return Some(vec![]);
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
            return Some(vec![]);
        }

        if self.file_picker.is_open() {
            return Some(match self.file_picker.handle_key(key) {
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
            });
        }

        if self.queue.focus().is_some() {
            match key.code {
                KeyCode::Up => self.queue.move_focus_up(),
                KeyCode::Down => self.queue.move_focus_down(),
                KeyCode::Enter => {
                    self.queue.remove_focused();
                }
                KeyCode::Esc => self.queue.unfocus(),
                _ if key::QUIT.matches(key) => self.queue.unfocus(),
                _ if key::POP_QUEUE.matches(key) => {
                    self.queue.remove(0);
                }
                _ => {}
            }
            return Some(vec![]);
        }

        if self.task_picker.is_open() {
            if key::TASKS.matches(key) {
                self.task_picker.close();
                return Some(vec![]);
            }
            return Some(match self.task_picker.handle_key(key) {
                PickerAction::Consumed | PickerAction::Toggle(..) => vec![],
                PickerAction::Select(entry) => {
                    self.task_picker_original = None;
                    self.active_chat = entry.chat_index;
                    vec![]
                }
                PickerAction::Close => {
                    self.active_chat = self.task_picker_original.take().unwrap_or(0);
                    vec![]
                }
            });
        }

        if self.rewind_picker.is_open() {
            return Some(match self.rewind_picker.handle_key(key) {
                RewindPickerAction::Consumed => vec![],
                RewindPickerAction::Select(entry) => self.rewind_to(entry),
                RewindPickerAction::Close => vec![],
            });
        }

        if self.theme_picker.is_open() {
            return Some(match self.theme_picker.handle_key(key) {
                ThemePickerAction::Consumed => vec![],
                ThemePickerAction::Closed => vec![],
            });
        }

        if self.model_picker.is_open() {
            return Some(match self.model_picker.handle_key(key) {
                ModelPickerAction::Consumed => vec![],
                ModelPickerAction::Select(spec) => {
                    vec![Action::ChangeModel(spec)]
                }
                ModelPickerAction::AssignTier(spec, tier) => {
                    vec![Action::AssignTier(spec, tier)]
                }
                ModelPickerAction::UnassignTier(spec, tier) => {
                    vec![Action::UnassignTier(spec, tier)]
                }
                ModelPickerAction::Close => vec![],
            });
        }

        if self.login_picker.is_open() {
            return Some(match self.login_picker.handle_key(key) {
                LoginPickerAction::Consumed => vec![],
                LoginPickerAction::Close => vec![],
                LoginPickerAction::Authenticated { model_spec } => {
                    vec![Action::ChangeModel(model_spec), Action::RefreshModels]
                }
                LoginPickerAction::Configured { slug } => {
                    vec![Action::RefreshProvider { slug }, Action::RefreshModels]
                }
            });
        }

        if self.mcp_picker.is_open() {
            return Some(match self.mcp_picker.handle_key(key) {
                McpPickerAction::Consumed => vec![],
                McpPickerAction::Toggle {
                    server_name,
                    enabled,
                } => {
                    vec![Action::ToggleMcp(server_name, enabled)]
                }
                McpPickerAction::Close => vec![],
            });
        }

        if key::PLAN_TOGGLE.matches(key) && self.plan_toggle_ready() {
            return Some(self.run_builtin(BuiltinAction::PlanToggle));
        }

        None
    }

    fn plan_toggle_ready(&self) -> bool {
        self.state.mode == Mode::Plan && self.state.plan.is_ready()
    }

    /// Single implementation behind both the default keybindings and
    /// `maki.ui.action`, so a Lua rebind can never drift from the
    /// original key's behavior.
    pub(crate) fn run_builtin(&mut self, action: BuiltinAction) -> Vec<Action> {
        match action {
            BuiltinAction::FilePicker => {
                self.file_picker.open(&self.state.session.cwd);
            }
            BuiltinAction::Search => {
                let top = self.chats[self.active_chat].scroll_top();
                let auto = self.chats[self.active_chat].auto_scroll();
                self.search_modal.open(top, auto);
            }
            BuiltinAction::Tasks => {
                if self.task_picker.is_open() {
                    self.task_picker.close();
                } else {
                    self.open_tasks();
                }
            }
            BuiltinAction::Help => self.help_modal.toggle(),
            BuiltinAction::PlanToggle => {
                if self.plan_toggle_ready() {
                    self.plan_form.toggle();
                }
            }
            BuiltinAction::PlanEditor => {
                return match self.state.plan.path() {
                    Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
                    None => {
                        self.flash(FLASH_NO_PLAN.into());
                        vec![]
                    }
                };
            }
            BuiltinAction::EditInput => return vec![Action::EditInputInEditor],
            BuiltinAction::PopQueue => {
                self.queue.remove(0);
            }
            BuiltinAction::PrevChat => self.active_chat = self.active_chat.saturating_sub(1),
            BuiltinAction::NextChat => {
                self.active_chat = (self.active_chat + 1).min(self.chats.len() - 1);
            }
        }
        vec![]
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        self.clear_selection_unless_pending_copy();

        if key::SUSPEND.matches(key) && cfg!(unix) {
            return vec![Action::Suspend];
        }

        if let Some(actions) = self.dispatch_overlay(key) {
            return actions;
        }

        if !(self.status == Status::Streaming && is_streaming_stop_key(key))
            && self.dispatch_override(key)
        {
            return vec![];
        }

        if let Some(actions) = self.handle_ctrl(key) {
            return actions;
        }

        if !self.is_main_chat() {
            return match key.code {
                KeyCode::Tab if !self.is_bash_input() => self.toggle_mode(),
                KeyCode::Esc if !self.chats[self.active_chat].is_finished() => {
                    if let Some(t) = self.last_esc.take()
                        && t.elapsed() < self.status_bar.flash_duration
                    {
                        self.handle_subagent_cancel()
                    } else {
                        self.last_esc = Some(Instant::now());
                        self.status_bar.flash(FLASH_CANCEL.into());
                        vec![]
                    }
                }
                _ => vec![],
            };
        }

        self.handle_main_chat_key(key)
    }

    fn dispatch_override(&self, key: KeyEvent) -> bool {
        let snap = self.keymap_reader.load();
        for entry in &snap.entries {
            if entry.key == key.code
                && entry.modifiers == key.modifiers
                && self.lua_event_handle.run_keybind_callback(entry.id)
            {
                return true;
            }
        }
        false
    }

    fn handle_main_chat_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key::EDIT_INPUT.matches(key) {
            return self.run_builtin(BuiltinAction::EditInput);
        }
        if is_ctrl(&key) {
            if key::POP_QUEUE.matches(key) {
                return self.run_builtin(BuiltinAction::PopQueue);
            } else if key::OPEN_EDITOR.matches(key) {
                return self.run_builtin(BuiltinAction::PlanEditor);
            } else if key::SEARCH.matches(key) {
                return self.run_builtin(BuiltinAction::Search);
            } else if key::FILE_PICKER.matches(key) {
                return self.run_builtin(BuiltinAction::FilePicker);
            } else if key.code == KeyCode::Char('v') && self.image_paste_rx.is_empty() {
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
            CommandAction::Complete(text) => {
                self.command_palette.sync(&text);
                self.input_box.set_input(text);
                self.input_box.buffer.move_to_end();
                return vec![];
            }
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
        self.quit_with(ExitRequest::Success)
    }

    fn quit_with(&mut self, req: ExitRequest) -> Vec<Action> {
        self.save_input_history();
        self.exit_request = req;
        vec![]
    }

    pub(crate) fn handle_submit(&mut self, sub: Submission) -> Vec<Action> {
        match std::mem::take(&mut self.pending_input) {
            PendingInput::AuthRetry { subagent_id } => {
                self.send_to_agent(subagent_id.as_deref(), String::new());
                return vec![];
            }
            PendingInput::None => {}
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
            let id = self.shell.reserve_id();
            let sigil = if prefix.visible { "!" } else { "!!" };
            let display = format!("{sigil} {}", prefix.command);
            self.main_chat().show_user_message(display);
            return vec![Action::ShellCommand {
                id,
                command: prefix.command,
                visible: prefix.visible,
            }];
        }
        self.submit_or_queue(sub.into())
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
        self.recoverable_queue.clear();
        self.status = Status::Idle;
        vec![Action::CancelAgent {
            run_id: cancelled_run,
        }]
    }

    fn handle_subagent_cancel(&mut self) -> Vec<Action> {
        let tool_use_id = self
            .chat_index
            .iter()
            .find(|&(_, &idx)| idx == self.active_chat)
            .map(|(id, _)| id.clone());

        let Some(tool_use_id) = tool_use_id else {
            return vec![];
        };

        self.chats[self.active_chat].flush();
        self.chats[self.active_chat].cancel_in_progress();
        self.chats[self.active_chat].mark_finished(DisplayRole::Error, CANCELLED_TEXT);
        self.subagent_answers.remove(&tool_use_id);

        vec![Action::CancelSubagent { tool_use_id }]
    }

    fn handle_agent_event(&mut self, envelope: Envelope) -> Vec<Action> {
        if envelope.run_id == RESTORE_RUN_ID {
            let (id, snapshot, theme_gen, is_header) = match envelope.event {
                AgentEvent::ToolSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, false),
                AgentEvent::ToolHeaderSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, true),
                _ => return vec![],
            };
            for chat in &mut self.chats {
                if is_header {
                    chat.tool_header_snapshot(&id, snapshot.clone(), theme_gen);
                } else {
                    chat.tool_snapshot(&id, snapshot.clone(), theme_gen);
                }
            }
            return vec![];
        }
        if envelope.run_id != self.run_id {
            // A snapshot dropped here degrades the tool body to llm_output.
            if let AgentEvent::ToolSnapshot { id, .. }
            | AgentEvent::ToolHeaderSnapshot { id, .. }
            | AgentEvent::LiveToolBuf { id, .. } = &envelope.event
            {
                tracing::debug!(
                    tool_id = %id,
                    event_run_id = envelope.run_id,
                    current_run_id = self.run_id,
                    "tool render event dropped: stale run_id"
                );
            }
            return vec![];
        }

        if let AgentEvent::SubagentHistory {
            tool_use_id,
            messages,
        } = envelope.event
        {
            // Workflow sessions use synthetic ids that no ToolDone will match,
            // so we finish them here on SubagentHistory.
            if let Some(&sub_idx) = self.chat_index.get(tool_use_id.as_str()) {
                self.chats[sub_idx].mark_finished(DisplayRole::Done, DONE_TEXT);
            }
            self.sync_task_picker();
            self.state
                .session_mut()
                .set_subagent_messages(tool_use_id, messages);
            return vec![];
        }

        match &envelope.event {
            AgentEvent::ToolStart(event) => self.fire_session_autocmd(
                "ToolStart",
                serde_json::json!({
                    "tool_id": event.id,
                    "tool": event.tool,
                }),
            ),
            AgentEvent::ToolDone(event) => self.fire_session_autocmd(
                "ToolDone",
                serde_json::json!({
                    "tool_id": event.id,
                    "tool": event.tool,
                }),
            ),
            _ => {}
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
            self.state
                .session_mut()
                .insert_tool_output(e.id.clone(), e.output.clone());
            if let Some(&sub_idx) = self.chat_index.get(&e.id) {
                let (role, text) = if e.is_error {
                    (DisplayRole::Error, ERROR_TEXT)
                } else {
                    (DisplayRole::Done, DONE_TEXT)
                };
                self.chats[sub_idx].mark_finished(role, text);
            }
            self.sync_task_picker();
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

        if let AgentEvent::TurnComplete(ref tc) = envelope.event {
            self.state.token_usage += tc.usage;
            add_cost(&mut self.chats[chat_idx].cost, tc.cost);
            self.state
                .session_mut()
                .add_model_usage(&tc.model, tc.usage.into());
            let ctx_size = tc.context_size.unwrap_or_else(|| tc.usage.context_tokens());
            self.chats[chat_idx].context_size = ctx_size;
            if chat_idx == 0 {
                self.state.context_size = ctx_size;
            }
            self.chats[chat_idx].set_pending_turn_usage(tc.usage.format(tc.cost));
            if let Some(tool_id) = &subagent_id {
                let formatted = tc.usage.format_sum_cost(self.chats[chat_idx].cost);
                self.chats[0].set_tool_turn_usage(tool_id, formatted);
            }
        }

        let plan_path = if self.state.mode == Mode::Plan {
            self.state.plan.path()
        } else {
            None
        };
        let result = self.chats[chat_idx].handle_event(envelope.event, plan_path);

        if let ChatEventResult::QueueItemConsumed { text, image_count } = result {
            if chat_idx == 0 {
                self.on_queue_item_consumed(&text, image_count);
            }
            return vec![];
        }

        if let ChatEventResult::PermissionRequest { id, tool, scopes } = result {
            self.permission_prompt
                .open(id, tool, scopes, subagent_id.clone());
            return vec![];
        }

        if let ChatEventResult::AuthRequired = result {
            self.chats[chat_idx].push(DisplayMessage::new(
                DisplayRole::Error,
                AUTH_EXPIRED_MSG.into(),
            ));
            if chat_idx != 0 {
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Error,
                    AUTH_EXPIRED_MSG.into(),
                ));
            }
            self.pending_input = PendingInput::AuthRetry { subagent_id };
            return vec![];
        }

        if chat_idx == 0 {
            match result {
                ChatEventResult::Done => {
                    self.status_bar.clear_flash();
                    self.terminalize_turn(MISSING_TOOL_COMPLETION);
                    self.chat_index.clear();
                    self.subagent_answers.clear();
                    self.status = Status::Idle;
                    self.fire_session_autocmd("TurnEnd", serde_json::json!({}));
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Success;
                    }
                }
                ChatEventResult::Error(message) => {
                    self.status = Status::error(message.clone());
                    self.status_bar.clear_flash();
                    self.subagent_answers.clear();
                    self.terminalize_turn(&message);
                    self.recoverable_queue = self.queue.text_messages();
                    self.queue.clear();
                    self.chat_index.clear();
                    self.fire_session_autocmd(
                        "TurnError",
                        serde_json::json!({ "message": message }),
                    );
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Error;
                    }
                }
                ChatEventResult::AuthRequired
                | ChatEventResult::PermissionRequest { .. }
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
        let mut chat = Chat::new(
            subagent.name.clone(),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        chat.set_restore_channel(self.restore_event_tx.clone());
        chat.model_id = subagent.model.clone();
        if let Some(ref prompt) = subagent.prompt {
            chat.push_user_message(prompt);
        }
        self.chats.push(chat);
        self.sync_task_picker();
        self.sync_subagents();
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
            "/usage" => {
                self.usage_modal.toggle();
                if self.usage_modal.is_open() {
                    vec![Action::RefreshUsage]
                } else {
                    vec![]
                }
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
            "/model" => {
                self.model_picker.open(&self.state.model.spec());
                vec![Action::RefreshModels]
            }
            "/theme" => {
                self.theme_picker.open();
                vec![]
            }
            "/mcp" => {
                self.mcp_picker.open();
                vec![]
            }
            "/login" => {
                self.login_picker.open(self.storage.clone());
                vec![]
            }
            "/cd" => self.cmd_cd(&cmd.args),
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
                if !self.state.model.supports_thinking() {
                    self.flash("Thinking requires a model that supports it".into());
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
            "/fast" => {
                if !self.state.model.supports_fast() {
                    self.flash(FAST_UNSUPPORTED_MSG.into());
                    return vec![];
                }
                self.state.fast = !self.state.fast;
                self.flash(
                    if self.state.fast {
                        FAST_ON_MSG
                    } else {
                        FAST_OFF_MSG
                    }
                    .into(),
                );
                vec![]
            }
            "/workflow" => {
                self.state.workflow = !self.state.workflow;
                self.flash(
                    if self.state.workflow {
                        WORKFLOW_ON_MSG
                    } else {
                        WORKFLOW_OFF_MSG
                    }
                    .into(),
                );
                vec![]
            }
            "/exit" => self.quit(),
            "/reload" => self.quit_with(ExitRequest::Reload),
            name if name.starts_with("/project:") || name.starts_with("/user:") => {
                self.execute_custom_command(name, &cmd.args)
            }
            name if self.command_palette.find_mcp_prompt(name).is_some() => {
                self.execute_mcp_prompt(name, &cmd.args)
            }
            name if self.command_palette.find_lua_command(name).is_some() => {
                self.run_lua_command(name, cmd.args);
                vec![]
            }
            _ => vec![],
        }
    }

    fn run_lua_command(&self, name: &str, args: String) {
        let Some(lua_cmd) = self.command_palette.find_lua_command(name) else {
            return;
        };
        self.lua_event_handle.run_command(
            Arc::clone(&lua_cmd.plugin),
            Arc::clone(&lua_cmd.name),
            args,
        );
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
            text: display_text.clone(),
            images: Vec::new(),
        });
        input.prompt = Some(Box::new(prompt_ref));

        if self.status == Status::Streaming {
            self.flash("Agent is busy, try again later".into());
            vec![]
        } else {
            self.start_run(input, display_text)
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
        self.submit_or_queue(QueuedMessage {
            text: cmd.render(args),
            images: Vec::new(),
        })
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
                    self.state
                        .session_mut()
                        .set_cwd(canonical.to_string_lossy().into_owned());
                }
                self.status_bar.refresh_cwd();
                self.flash(format!("cd {}", path.display()))
            }
            Err(e) => self.flash(format!("cd: {e}")),
        }
        vec![]
    }

    fn overlays(&self) -> [&dyn Overlay; 13] {
        [
            &self.help_modal,
            &self.usage_modal,
            &self.btw_modal,
            &self.float_mgr,
            &self.search_modal,
            &self.file_picker,
            &self.task_picker,
            &self.rewind_picker,
            &self.theme_picker,
            &self.model_picker,
            &self.login_picker,
            &self.mcp_picker,
            &self.permission_prompt,
        ]
    }

    fn overlays_mut(&mut self) -> [&mut dyn Overlay; 13] {
        [
            &mut self.help_modal,
            &mut self.usage_modal,
            &mut self.btw_modal,
            &mut self.float_mgr,
            &mut self.search_modal,
            &mut self.file_picker,
            &mut self.task_picker,
            &mut self.rewind_picker,
            &mut self.theme_picker,
            &mut self.model_picker,
            &mut self.login_picker,
            &mut self.mcp_picker,
            &mut self.permission_prompt,
        ]
    }

    pub fn any_overlay_open(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open())
    }

    /// True when the agent is parked on user input. Drives the `needs_input`
    /// session status.
    pub(crate) fn awaiting_input(&self) -> bool {
        self.permission_prompt.is_open()
            || self.pending_input != PendingInput::None
            || self.float_mgr.needs_input()
    }

    /// True while `recoverable_queue` holds user text captured at an agent
    /// error; a background run would wipe it (`start_run` clears the queue).
    pub(crate) fn holds_recovery_text(&self) -> bool {
        !self.recoverable_queue.is_empty()
    }

    pub fn has_modal_overlay(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open() && o.is_modal())
    }

    pub fn close_all_overlays(&mut self) {
        self.overlays_mut().iter_mut().for_each(|o| o.close());
    }

    pub fn is_animating(&self) -> bool {
        !self.image_paste_rx.is_empty()
            || self.btw_modal.is_animating()
            || self.file_picker.is_loading()
            || self.float_mgr.is_open()
            || self
                .selection_state
                .as_ref()
                .is_some_and(|s| s.is_edge_scrolling())
            || self.restoring.load(Ordering::Relaxed)
            || self.chats.iter().any(|c| c.is_animating())
    }

    fn finish_subagents(&mut self, role: DisplayRole, text: &str) {
        self.retain_resolved_subagents(role, text);
        self.chat_index.clear();
    }

    /// Terminalizes every tool left in progress when a turn ends, sparing
    /// shell commands that outlive the agent.
    fn terminalize_turn(&mut self, message: &str) {
        self.retain_resolved_subagents(DisplayRole::Error, ERROR_TEXT);
        self.chats[0].fail_in_progress_except(message.into(), self.shell.active_ids());
        for chat in self.chats.iter_mut().skip(1) {
            chat.fail_in_progress_with_message(message.into());
        }
        self.sync_task_picker();
    }

    /// Marks unfinished subagent chats as ended and drops them from
    /// `chat_index`, so the session records only the children that really
    /// completed.
    fn retain_resolved_subagents(&mut self, role: DisplayRole, text: &str) {
        self.chat_index.retain(|_, &mut sub_idx| {
            if self.chats[sub_idx].is_finished() {
                true
            } else {
                self.chats[sub_idx].mark_finished(role.clone(), text);
                false
            }
        });
        self.sync_subagents();
    }

    pub fn flush_all_chats(&mut self) {
        for chat in &mut self.chats {
            chat.flush();
        }
    }

    fn route_text_paste(&mut self, text: &str) {
        if self.plan_form_active() {
            return;
        }
        if self.permission_prompt.handle_paste(text) {
            return;
        }
        if self.float_mgr.handle_paste(text) {
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
        try_picker!(self.file_picker);
        try_picker!(self.task_picker);
        try_picker!(self.rewind_picker);
        try_picker!(self.theme_picker);
        try_picker!(self.model_picker);
        try_picker!(self.mcp_picker);
        try_picker!(self.login_picker);
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

    fn implement_plan(&mut self, clear_context: bool) -> Vec<Action> {
        let parallel = self.plan_form.parallel();
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
            let text = if parallel {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`. {IMPLEMENT_PARALLEL_HINT}")
            } else {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`.")
            };
            self.main_chat()
                .push(DisplayMessage::plan(content, path_str));
            text
        } else {
            format!("{}.", IMPLEMENT_MSG_PREFIX)
        };
        let msg = QueuedMessage {
            text,
            images: vec![],
        };
        actions.extend(self.start_from_queue(&msg));
        actions
    }
}

fn is_streaming_stop_key(key: KeyEvent) -> bool {
    key::QUIT.matches(key) || key.code == KeyCode::Esc
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
