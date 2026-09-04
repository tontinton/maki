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
pub(crate) mod tasks;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod view;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::AppSession;
use crate::app::tasks::TaskOutcome;
use crate::chat::Chat;
use crate::chat::{CANCELLED_TEXT, ChatEventResult, DONE_TEXT, ERROR_TEXT};
use crate::clipboard::ClipboardState;
use crate::components::btw_modal::BtwModal;
use crate::components::command::{CommandAction, CommandPalette, ParsedCommand};
use crate::components::file_picker::{FilePickerModal, FilePickerModalAction};
use crate::components::help_modal::HelpModal;
use crate::components::input::{InputAction, InputBox, Submission};
use crate::components::keybindings::key;
use crate::components::login_picker::{LoginPicker, LoginPickerAction};
use crate::components::lua_float::FloatManager;
use crate::components::mcp_picker::{McpPicker, McpPickerAction};
use crate::components::model_picker::{ModelPicker, ModelPickerAction};
use crate::components::pack_review::{PackReview, PackReviewAction};
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
use crate::repaint::{Cadence, Dirty, Watch};
use crate::selection::{SelectionState, SelectionZone, ZoneRegistry};
use arc_swap::{ArcSwap, ArcSwapOption};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    AgentEvent, Envelope, ImageSource, McpConfigErrors, McpPromptInfo, McpSnapshotReader,
    ReviewerVerdictEvent, SharedMessages, SubagentInfo,
};
use maki_config::{ModelPolicy, UiConfig};
use maki_lua::{
    BuiltinAction, EventHandle, HintReader, HintSnapshot, KeymapReader, LuaCommandReader,
    PackCommand, PackPreparation, PlanActionReader, WinView,
};
use maki_providers::{ContentBlock, Message, Model, ThinkingConfig, add_cost};
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
const THINKING_UNSUPPORTED_MSG: &str = "Thinking requires a model that supports it";
const FAST_ON_MSG: &str = "Fast mode: on";
const FAST_OFF_MSG: &str = "Fast mode: off";
const WORKFLOW_ON_MSG: &str = "Workflow mode: on";
const WORKFLOW_OFF_MSG: &str = "Workflow mode: off";
const PACK_CHANGES_DECLINED: &str = "Package changes declined";
const PACK_USER_ONLY_SUFFIX: &str = " can only be run by you";
const IMPLEMENT_MSG_PREFIX: &str = "Implement the plan";
const IMPLEMENT_PARALLEL_HINT: &str = "Use batch+task to parallelize, assign each subagent a separate module and restrict its tests to that module to avoid interference.";

const MISSING_TOOL_COMPLETION: &str = "Tool did not report completion before the turn ended";
const NOTIFICATION_PREVIEW_CHARS: usize = 200;

/// Depth budget for `maki.api.run_command` chains. Aliases nest a level or two
/// in practice; the cap only exists so a command aliasing itself reports an
/// error instead of ping-ponging with the Lua thread forever.
pub(crate) const MAX_COMMAND_DEPTH: u8 = 8;
pub(crate) const COMMAND_DEPTH_MSG: &str = "slash command nested too deeply (alias cycle?)";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notification {
    TurnComplete { response: Option<String> },
    PermissionRequested { tool: Option<String> },
    AuthenticationRequired,
    QuestionRequested,
    PlanReady,
}

impl Notification {
    /// Prompts blocking the agent outrank turn completions.
    pub(crate) fn is_urgent(&self) -> bool {
        !matches!(self, Self::TurnComplete { .. })
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::TurnComplete { response } => response
                .clone()
                .unwrap_or_else(|| "Agent turn complete".into()),
            Self::PermissionRequested { tool: Some(tool) } => {
                format!("Permission requested: {tool}")
            }
            Self::PermissionRequested { tool: None } => "Permission requested".into(),
            Self::AuthenticationRequired => "Authentication required".into(),
            Self::QuestionRequested => "Question requested".into(),
            Self::PlanReady => "Plan ready".into(),
        }
    }

    pub(crate) fn error_completion() -> Self {
        Self::TurnComplete {
            response: Some("Agent stopped with an error".into()),
        }
    }
}

/// Lazy, so a huge response only costs the first `NOTIFICATION_PREVIEW_CHARS`
/// characters.
fn notification_preview<'a>(chunks: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut preview: String = chunks
        .flat_map(str::split_whitespace)
        .enumerate()
        .flat_map(|(i, word)| (i > 0).then_some(' ').into_iter().chain(word.chars()))
        .take(NOTIFICATION_PREVIEW_CHARS)
        .collect();
    if preview.ends_with(' ') {
        preview.pop();
    }
    (!preview.is_empty()).then_some(preview)
}

fn normalize_preview(text: &str) -> Option<String> {
    notification_preview(std::iter::once(text))
}

pub(crate) fn turn_response(message: &Message) -> Option<String> {
    if message.has_tool_calls() {
        return None;
    }

    notification_preview(message.content.iter().filter_map(|block| match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    }))
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
    pub(super) pack_review: PackReview,
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
    /// The spec Lua was last told about. Seeded with the live model rather
    /// than the session's stored one: a restored session may name another
    /// model, and the event loop swaps the live one in on the first tick.
    announced_model_spec: String,
    pub(super) keymap_reader: KeymapReader,
    pub(super) hint_reader: HintReader,
    hints: Watch<HintSnapshot>,
    pub(super) plan_action_reader: PlanActionReader,
    /// Last snapshot generation applied to `plan_form`; skips a rebuild
    /// when no plugin actions have changed.
    plan_actions_generation: u64,
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
        plan_action_reader: PlanActionReader,
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
            theme_picker: ThemePicker::new(),
            model_picker: ModelPicker::new(available_models),
            login_picker: LoginPicker::new(),
            mcp_picker: McpPicker::new(mcp_reader, mcp_config_errors),
            rewind_picker: RewindPicker::new(),
            help_modal: HelpModal::new(),
            usage_modal: UsageModal::new(),
            btw_modal: BtwModal::new(typewriter, ui_config.show_thinking),
            float_mgr: FloatManager::new(),
            search_modal: SearchModal::new(),
            file_picker: FilePickerModal::new(),
            pack_review: PackReview::new(),
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
            announced_model_spec: model.spec(),
            hints: Watch::seeded(hint_reader.load_full()),
            keymap_reader,
            hint_reader,
            plan_action_reader,
            plan_actions_generation: 0,
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
        // The manager arrives forked from the prototype the process was
        // started with, so a tab that resumes or spawns blank runs on
        // `--yolo` until its own meta is read back here.
        app.apply_stored_permissions(&app.state.session.meta);
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

    /// One diff per frame covers every way a model can change (the picker,
    /// `/model`, `maki.model.set`, the provider fallback, loading another
    /// session, which swaps `state` wholesale), so no path has to remember to
    /// speak up. The spec alone decides: the background catalog fetch
    /// re-stores the running model once it learns its context window, and a
    /// hint has nothing new to draw for that.
    pub(crate) fn emit_model_change(&mut self) {
        let spec = self.state.model.spec();
        if spec == self.announced_model_spec {
            return;
        }
        let previous_spec = std::mem::replace(&mut self.announced_model_spec, spec);
        self.fire_session_autocmd(
            "ModelChanged",
            serde_json::json!({ "model": self.model_state(), "previous_spec": previous_spec }),
        );
    }

    /// Takes the spelling both `/thinking` and `maki.model.set` accept; a
    /// blank {input} toggles.
    pub(crate) fn set_thinking(&mut self, input: &str) -> Result<ThinkingConfig, String> {
        if !self.state.model.supports_thinking() {
            return Err(THINKING_UNSUPPORTED_MSG.into());
        }
        self.state.thinking =
            ThinkingConfig::parse(input.trim(), self.state.thinking).map_err(str::to_owned)?;
        Ok(self.state.thinking)
    }

    pub(crate) fn set_fast(&mut self, fast: bool) -> Result<(), String> {
        if fast && !self.state.model.supports_fast() {
            return Err(FAST_UNSUPPORTED_MSG.into());
        }
        self.state.fast = fast;
        Ok(())
    }

    /// What `maki.model.get` hands to Lua.
    pub(crate) fn model_state(&self) -> serde_json::Value {
        let model = &self.state.model;
        serde_json::json!({
            "spec": model.spec(),
            "id": model.id,
            "provider": model.provider.to_string(),
            "thinking": self.state.thinking.to_string(),
            "fast": self.state.fast,
            "supports_thinking": model.supports_thinking(),
            "supports_fast": model.supports_fast(),
        })
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

    // Accounting only; the `ToolReviewed` autocmd fires from
    // `agent_autocmd::dispatch` so `-p` and sdk mode see it too.
    fn handle_reviewer_verdict(&mut self, event: &ReviewerVerdictEvent) {
        self.state.token_usage += event.usage;
        add_cost(&mut self.state.cost, event.billed_cost);
        add_cost(&mut self.state.list_cost, event.list_cost);
        if !event.model.is_empty() {
            self.state.session_mut().add_model_usage(
                &event.model,
                event
                    .usage
                    .billed_with_list_cost(event.billed_cost, event.list_cost),
            );
        }
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

    pub fn tick_error_expiry(&mut self) -> Dirty {
        if !self.status.is_error_expired() {
            return Dirty::NO;
        }
        self.status = Status::Idle;
        Dirty::YES
    }

    fn active_chat(&mut self) -> &mut Chat {
        &mut self.chats[self.active_chat]
    }

    pub(crate) fn win_view(&self) -> WinView {
        self.chats[self.active_chat].win_view()
    }

    pub(crate) fn scroll_to_row(&mut self, doc_row: u32) {
        self.active_chat().scroll_to_row(doc_row);
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
        try_picker!(self.model_picker);
        try_picker!(self.file_picker);
        let zone = self.zone_at(row, column)?.zone;
        self.scroll_zone(zone, delta);
        Some(zone)
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
        // With both up the permission prompt goes first: a tool is blocked on
        // it and it owns the bottom panel. The pack review waits on nothing.
        if self.permission_prompt.is_open() {
            if let Some(answer) = self.permission_prompt.handle_key(key) {
                let subagent_id = self.permission_prompt.subagent_id().map(str::to_owned);
                let encoded = answer.encode();
                self.permission_prompt.close();
                self.send_to_agent(subagent_id.as_deref(), encoded);
            }
            return Some(vec![]);
        }

        if self.pack_review.is_open() {
            return Some(match self.pack_review.handle_key(key) {
                Some(PackReviewAction::Accept(plan)) => self.quit_with(ExitRequest::Pack(plan)),
                Some(PackReviewAction::Decline) => {
                    self.flash(PACK_CHANGES_DECLINED.to_owned());
                    Vec::new()
                }
                None => Vec::new(),
            });
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
                    if let Some((pos, auto)) = saved {
                        chat.restore_scroll(pos, auto);
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
                ModelPickerAction::Refresh => vec![Action::RefreshModelsLive],
                ModelPickerAction::Close => vec![],
            });
        }

        if self.login_picker.is_open() {
            return Some(match self.login_picker.handle_key(key) {
                LoginPickerAction::Consumed => vec![],
                LoginPickerAction::Close => vec![],
                LoginPickerAction::Authenticated { model_spec } => {
                    vec![Action::ChangeModel(model_spec), Action::RefreshModelsLive]
                }
                LoginPickerAction::Configured { slug } => {
                    vec![Action::RefreshProvider { slug }, Action::RefreshModelsLive]
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
                let pos = self.chats[self.active_chat].scroll_pos();
                let auto = self.chats[self.active_chat].auto_scroll();
                self.search_modal.open(pos, auto);
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
            BuiltinAction::ModelPicker => {
                self.model_picker.open(&self.state.model.spec());
                return vec![Action::RefreshModels];
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
        if key::MODEL_PICKER.matches(key) {
            return self.run_builtin(BuiltinAction::ModelPicker);
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
            CommandAction::Execute(cmd) => {
                self.input_box.discard();
                return self.execute_command(cmd, 0);
            }
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
        vec![Action::ManualExit]
    }

    pub(crate) fn clear_exit_request(&mut self) {
        self.exit_request = ExitRequest::None;
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
        self.finish_subagents(TaskOutcome::Error, CANCELLED_TEXT);
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
        self.chats[self.active_chat].mark_finished(TaskOutcome::Error, CANCELLED_TEXT);
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
            // so we finish them here on SubagentHistory. This event only knows
            // that the transcript closed, so say Unknown and leave the verdict
            // to the ToolDone that follows elsewhere.
            if let Some(&sub_idx) = self.chat_index.get(tool_use_id.as_str()) {
                self.chats[sub_idx].mark_finished(TaskOutcome::Unknown, DONE_TEXT);
            }
            self.state
                .session_mut()
                .set_subagent_messages(tool_use_id, messages);
            return vec![];
        }

        maki_lua::agent_autocmd::dispatch(
            &self.lua_event_handle,
            &self.state.session.id,
            &envelope,
            envelope.subagent.is_some(),
        );
        if let AgentEvent::ReviewerVerdict(event) = &envelope.event {
            self.handle_reviewer_verdict(event);
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
                let (outcome, text) = if e.is_error {
                    (TaskOutcome::Error, ERROR_TEXT)
                } else {
                    (TaskOutcome::Done, DONE_TEXT)
                };
                self.chats[sub_idx].mark_finished(outcome, text);
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

        if let AgentEvent::TurnComplete(ref tc) = envelope.event {
            self.state.token_usage += tc.usage;
            add_cost(&mut self.state.cost, tc.cost);
            add_cost(&mut self.chats[chat_idx].cost, tc.cost);
            add_cost(&mut self.state.list_cost, tc.list_cost);
            add_cost(&mut self.chats[chat_idx].list_cost, tc.list_cost);
            self.state.session_mut().add_model_usage(
                &tc.model,
                tc.usage.billed_with_list_cost(tc.cost, tc.list_cost),
            );
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
        let mut chat = Chat::subagent(
            id,
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
        self.sync_subagents();
        idx
    }

    /// Entry point for `maki.api.run_command`: splits a command line into the
    /// name and args the input bar would hand over, leading slash optional.
    /// `Err` means nothing ran at all, so the Lua caller can say why.
    pub(crate) fn run_cmdline(&mut self, cmdline: &str, depth: u8) -> Result<Vec<Action>, String> {
        if depth > MAX_COMMAND_DEPTH {
            return Err(COMMAND_DEPTH_MSG.to_string());
        }
        let trimmed = cmdline.trim();
        let (name, args) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        let resolved = self
            .command_palette
            .resolve(&format!("/{}", name.trim_start_matches('/')))
            .ok_or_else(|| format!("unknown command '{name}'"))?;
        Ok(self.execute_command(
            ParsedCommand {
                name: resolved,
                args: args.trim().to_string(),
                bang: false,
            },
            depth,
        ))
    }

    /// {depth} is the `maki.api.run_command` hop count, forwarded to a Lua
    /// handler so an alias cycle keeps counting. 0 when the user typed it.
    fn execute_command(&mut self, cmd: ParsedCommand, depth: u8) -> Vec<Action> {
        match cmd.name.as_str() {
            "/compact" => {
                let instructions = (!cmd.args.is_empty()).then(|| cmd.args.clone());
                if self.status == Status::Streaming {
                    self.queue_compact(instructions);
                    return vec![];
                }
                self.status = Status::Streaming;
                vec![Action::Compact(instructions)]
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
                match self.set_thinking(&cmd.args) {
                    Ok(thinking) => self.flash(format!("Thinking: {thinking}")),
                    Err(msg) => self.flash(msg),
                }
                vec![]
            }
            "/fast" => {
                let fast = !self.state.fast;
                match self.set_fast(fast) {
                    Ok(()) => self.flash(if fast { FAST_ON_MSG } else { FAST_OFF_MSG }.into()),
                    Err(msg) => self.flash(msg),
                }
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
            name @ ("/packupdate" | "/packdel") => {
                if depth > 0 {
                    self.flash(format!("{name}{PACK_USER_ONLY_SUFFIX}"));
                    return vec![];
                }
                match PackCommand::parse(name, &cmd.args, cmd.bang) {
                    Ok(command) => vec![Action::PreparePack(command)],
                    Err(message) => {
                        self.flash(message);
                        vec![]
                    }
                }
            }
            name if name.starts_with("/project:") || name.starts_with("/user:") => {
                self.execute_custom_command(name, &cmd.args)
            }
            name if self.command_palette.find_mcp_prompt(name).is_some() => {
                self.execute_mcp_prompt(name, &cmd.args)
            }
            name if self.command_palette.find_lua_command(name).is_some() => {
                self.run_lua_command(name, cmd.args, depth);
                vec![]
            }
            _ => vec![],
        }
    }

    fn run_lua_command(&self, name: &str, args: String, depth: u8) {
        let Some(lua_cmd) = self.command_palette.find_lua_command(name) else {
            return;
        };
        self.lua_event_handle.run_command(
            Arc::clone(&lua_cmd.plugin),
            Arc::clone(&lua_cmd.name),
            args,
            depth,
        );
    }

    pub(crate) fn handle_pack_preparation(&mut self, preparation: PackPreparation) -> Vec<Action> {
        match preparation {
            PackPreparation::Complete(report) => {
                self.flash(report.message());
                Vec::new()
            }
            PackPreparation::Ready(plan) => self.quit_with(ExitRequest::Pack(plan)),
            PackPreparation::Review { prompt, plan } => {
                self.pack_review.open(prompt, plan);
                Vec::new()
            }
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
            &self.rewind_picker,
            &self.theme_picker,
            &self.model_picker,
            &self.login_picker,
            &self.mcp_picker,
            &self.pack_review,
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
            &mut self.rewind_picker,
            &mut self.theme_picker,
            &mut self.model_picker,
            &mut self.login_picker,
            &mut self.mcp_picker,
            &mut self.pack_review,
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

    pub(crate) fn attention(&self) -> Option<Notification> {
        if let Some(tool) = self.permission_prompt.tool() {
            let tool = (!matches!(tool, maki_config::ToolKey::Wildcard))
                .then(|| normalize_preview(&tool.to_string()))
                .flatten();
            return Some(Notification::PermissionRequested { tool });
        }
        if matches!(self.pending_input, PendingInput::AuthRetry { .. }) {
            return Some(Notification::AuthenticationRequired);
        }
        if self.status != Status::Streaming && self.plan_form_active() {
            return Some(Notification::PlanReady);
        }
        self.float_mgr
            .needs_input()
            .then_some(Notification::QuestionRequested)
    }

    pub fn has_modal_overlay(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open() && o.is_modal())
    }

    pub fn close_all_overlays(&mut self) {
        self.overlays_mut().iter_mut().for_each(|o| o.close());
    }

    /// Every poller that feeds the screen, in one place and never in `view`;
    /// see [`crate::repaint`] for why.
    pub fn tick(&mut self) -> Dirty {
        // `|` never short-circuits: every poller must run on every tick.
        self.float_mgr.tick()
            | self.tick_edge_scroll()
            | self.tick_error_expiry()
            | self.poll_image_paste()
            | self.btw_modal.poll()
            | self.status_bar.poll_branch_update()
            | self.status_bar.clear_expired_hint()
            | self.mcp_picker.refresh()
            | self.model_picker.refresh()
            | self.usage_modal.poll(&self.usage_slot)
            | self.hints.poll(self.hint_reader.load_full())
            | self.tick_plan_actions()
            | self.tick_file_picker()
            | Dirty::any(self.chats.iter_mut().map(Chat::tick))
    }

    /// Mirror the plan-action snapshot into the plan form when a plugin
    /// (un)registered since the last tick.
    fn tick_plan_actions(&mut self) -> Dirty {
        let snapshot = self.plan_action_reader.load();
        if snapshot.generation == self.plan_actions_generation {
            return Dirty::NO;
        }
        let rows = snapshot
            .actions
            .iter()
            .map(|a| crate::components::plan_form::PluginPlanRow {
                plugin: a.plugin.clone(),
                name: a.name.clone(),
                label: a.label.clone(),
                desc: a.desc.clone(),
                order: a.order,
            })
            .collect();
        self.plan_form.set_plugin_rows(rows, snapshot.generation);
        self.plan_actions_generation = snapshot.generation;
        Dirty::YES
    }

    fn tick_file_picker(&mut self) -> Dirty {
        let (dirty, flash) = self.file_picker.tick();
        if let Some(flash) = flash {
            self.status_bar.flash(flash);
        }
        dirty
    }

    /// What moves with the clock alone; changes that come from arriving data
    /// are reported by [`Self::tick`] instead. Overlays answer as a group, so
    /// adding one to [`Self::overlays`] is enough.
    pub fn cadence(&self) -> Cadence {
        Cadence::any([
            Cadence::any(self.overlays().into_iter().map(Overlay::cadence)),
            StatusBar::cadence(
                &self.status,
                self.restoring.load(Ordering::Relaxed),
                self.retry_info.is_some(),
            ),
            self.selection_state
                .as_ref()
                .map_or(Cadence::IDLE, SelectionState::cadence),
            Cadence::any(self.chats.iter().map(Chat::cadence)),
        ])
    }

    fn finish_subagents(&mut self, outcome: TaskOutcome, text: &str) {
        self.retain_resolved_subagents(outcome, text);
        self.chat_index.clear();
    }

    /// Terminalizes every tool left in progress when a turn ends, sparing
    /// shell commands that outlive the agent.
    fn terminalize_turn(&mut self, message: &str) {
        self.retain_resolved_subagents(TaskOutcome::Error, ERROR_TEXT);
        self.chats[0].fail_in_progress_except(message.into(), self.shell.active_ids());
        for chat in self.chats.iter_mut().skip(1) {
            chat.fail_in_progress_with_message(message.into());
        }
    }

    /// Marks unfinished subagent chats as ended and drops them from
    /// `chat_index`, so the session records only the children that really
    /// completed.
    fn retain_resolved_subagents(&mut self, outcome: TaskOutcome, text: &str) {
        self.chat_index.retain(|_, &mut sub_idx| {
            if self.chats[sub_idx].is_finished() {
                true
            } else {
                self.chats[sub_idx].mark_finished(outcome, text);
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
            PlanFormAction::OpenEditor => self.open_plan_editor_action(),
            PlanFormAction::Implement => self.implement_plan(false),
            PlanFormAction::ClearAndImplement => self.implement_plan(true),
            PlanFormAction::Plugin { plugin, name } => {
                // Snapshot the form's parallel + selected before we reset,
                // since the handler may want them and reset() runs after.
                let parallel = self.plan_form.parallel();
                let selected = self.plan_form.selected();
                let path = self
                    .state
                    .plan
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.plan_form.reset();
                self.lua_event_handle
                    .run_plan_action(plugin, name, path, parallel, selected);
                vec![]
            }
        }
    }

    pub(crate) fn open_plan_editor_action(&mut self) -> Vec<Action> {
        match self.state.plan.path() {
            Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
            None => {
                self.flash(FLASH_NO_PLAN.into());
                vec![]
            }
        }
    }

    /// Snapshot of the current plan for `maki.plan.read()`. `content`
    /// stays `None` when the plan is not ready or the file cannot be
    /// read, so a caller can tell the two apart from an empty plan.
    pub(crate) fn plan_snapshot(&self) -> serde_json::Value {
        let mode = if self.state.mode == Mode::Plan {
            "plan"
        } else {
            "build"
        };
        let path = self.state.plan.path().map(|p| p.display().to_string());
        let ready = self.state.plan.is_ready();
        let content = if ready {
            self.state
                .plan
                .path()
                .and_then(|p| std::fs::read_to_string(p).ok())
        } else {
            None
        };
        serde_json::json!({
            "mode": mode,
            "path": path,
            "ready": ready,
            "content": content,
        })
    }

    /// Read the plan-form suppression flag, optionally mutating it first;
    /// returns the previous value so the Lua caller can restore state.
    pub(crate) fn set_plan_form_suppressed(&mut self, hidden: Option<bool>) -> bool {
        match hidden {
            Some(v) => self.plan_form.set_suppressed(v),
            None => self.plan_form.is_suppressed(),
        }
    }

    /// Fire the same code path a built-in Implement / Clear-and-implement
    /// row would. Used by `maki.plan.implement` and by plugin plan
    /// actions that decide the plan is ready to execute. Silently no-op
    /// when there is no ready plan, matching the built-in Hide branch.
    pub(crate) fn implement_plan_from_lua(&mut self, clear_context: bool) -> Vec<Action> {
        if self.state.plan.path().is_none() {
            return vec![];
        }
        self.implement_plan(clear_context)
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
