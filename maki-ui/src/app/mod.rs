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
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::OpenSession;
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
use crate::components::plan_form::{PlanForm, PlanFormAction, builtin_menu, builtin_rows};
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
use crate::markdown::TRUNCATION_PREFIX;
use crate::repaint::{Cadence, Dirty, Watch};
use crate::selection::{SelectionState, SelectionZone, ZoneRegistry};
use arc_swap::{ArcSwap, ArcSwapOption};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use maki_agent::permissions::{PermissionManager, TaggedAnswer};
use maki_agent::{
    AgentEvent, Envelope, ImageSource, McpConfigErrors, McpPromptInfo, McpSnapshotReader,
    SharedMessages, SubagentInfo,
};
use maki_config::project::{self, GatedFile, TrustQuestion};
use maki_config::{ModelPolicy, UiConfig};
use maki_lua::{
    BuiltinAction, EventHandle, HintReader, HintSnapshot, InputEdit, Key, KeymapReader,
    LuaCommandReader, PLAN_FORM_SLOT_DEADLINE, PLAN_ROW_HANDLER_DEADLINE, PackCommand,
    PackPreparation, PlanActionOutcome, PlanMenu, PlanRowAction, WinView, is_reserved,
};
use maki_providers::models_cache::ModelList;
use maki_providers::{ContentBlock, Message, Model, ThinkingConfig, add_cost};
use maki_storage::StateDir;
use maki_storage::input_history::InputHistory;

use crate::storage_writer::StorageWriter;
use ratatui::layout::{Position, Rect};

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
const FLASH_PLAN_ACTION_LOST: &str = "The plugin host never took that plan action";
const FLASH_PLAN_ACTION_FAILED: &str = "That plan action did not run";
const FLASH_PLAN_FORM_SLOW: &str = "The plugin host was slow, opened the built-in plan form";
/// What both plan waits add on top of the host's own budget. It covers the
/// request queue a `/reload` or a long tool call holds, plus the executor
/// getting round to the answer, neither of which the host's deadline starts
/// counting until it has dequeued the request.
const PLAN_FORM_QUEUE_SLACK_SECS: u64 = 10;
/// How long the form waits on the `ui.plan_form*` chains before it gives up
/// and opens the built-in one. Strictly longer than [`PLAN_FORM_SLOT_DEADLINE`],
/// the whole budget the host gives both chains, so this only ever fires for a
/// host that never answered at all: a legitimate answer that used every second
/// it was allowed still beats it, and the user never sees
/// [`FLASH_PLAN_FORM_SLOW`] for a layer that behaved.
const PLAN_FORM_ANSWER_WAIT: Duration =
    Duration::from_secs(PLAN_FORM_SLOT_DEADLINE.as_secs() + PLAN_FORM_QUEUE_SLACK_SECS);
/// The same bound for a picked row's handler, derived the same way from
/// [`PLAN_ROW_HANDLER_DEADLINE`], the whole budget the host gives one. It
/// covers a pick that never reached the host, since the host cuts a parked
/// handler off itself, and a handler that spent every second it was allowed
/// still beats it.
const PLAN_ACTION_ANSWER_WAIT: Duration =
    Duration::from_secs(PLAN_ROW_HANDLER_DEADLINE.as_secs() + PLAN_FORM_QUEUE_SLACK_SECS);
const FAST_UNSUPPORTED_MSG: &str = "Fast mode needs Anthropic Opus 4.6+ with an API key, or an eligible Codex model with a ChatGPT subscription";
const THINKING_UNSUPPORTED_MSG: &str = "Thinking requires a model that supports it";
const FAST_ON_MSG: &str = "Fast mode: on";
const FAST_PENDING_MSG: &str = "Fast mode: pending model discovery";
const FAST_OFF_MSG: &str = "Fast mode: off";
const WORKFLOW_ON_MSG: &str = "Workflow mode: on";
const WORKFLOW_OFF_MSG: &str = "Workflow mode: off";
pub(crate) const NOTHING_TO_TRUST_MSG: &str = "nothing to trust in this folder";
const TRUSTED_PREFIX: &str = "Trusted this folder: ";
const PACK_CHANGES_DECLINED: &str = "Package changes declined";
const PACK_USER_ONLY_SUFFIX: &str = " can only be run by you";
const IMPLEMENT_MSG_PREFIX: &str = "Implement the plan";
const IMPLEMENT_PARALLEL_HINT: &str = "Use batch+task to parallelize, assign each subagent a separate module and restrict its tests to that module to avoid interference.";

const MISSING_TOOL_COMPLETION: &str = "Tool did not report completion before the turn ended";
const NOTIFICATION_PREVIEW_CHARS: usize = 200;
/// An API error carries the provider's raw response body, sometimes a whole
/// HTML page from a broken proxy, and the bubble stays for the rest of the
/// session. Well above any real error message, small enough to not drown chat.
const ERROR_BUBBLE_MAX_CHARS: usize = 2_000;

/// Depth budget for `maki.api.run_command` chains. Aliases nest a level or two
/// in practice; the cap only exists so a command aliasing itself reports an
/// error instead of ping-ponging with the Lua thread forever.
pub(crate) const MAX_COMMAND_DEPTH: u8 = 8;
pub(crate) const COMMAND_DEPTH_MSG: &str = "slash command nested too deeply (alias cycle?)";

pub(crate) const INPUT_NOT_LIVE_ERR: &str =
    "the chat input is not on screen, so it cannot be edited";

/// Who has moved the chat input since the last tick, and the buffer version
/// that writer left behind.
///
/// `InputChanged` names a plugin only when that plugin was the frame's sole
/// writer. Handlers ignore their own writes, so a frame that also carried a
/// keystroke or another plugin's edit has to reach them unlabelled, or they
/// drop a change they can never see again.
///
/// The version is what holds that rule up without every path to the value
/// having to remember this type exists. Submit, history recall, `$EDITOR` and
/// the rest write the whole value without passing through
/// [`App::input_changed`], and each of them bumps the buffer's version, which
/// strands the name on a value that is gone instead of pinning it on their
/// write.
///
/// A caret the user moved sets no writer at all: only an edit passes through
/// [`App::input_changed`], so a cursor-only frame is reported unlabelled,
/// which is what the rule already says about a change nobody claimed.
#[derive(Default)]
enum InputWriter {
    #[default]
    Untouched,
    Plugin(Arc<str>, u64),
    /// The user, or two writers in one frame: nobody may ignore this one.
    Anyone,
}

impl InputWriter {
    fn merge(self, next: Self) -> Self {
        match (self, next) {
            (Self::Untouched, next) => next,
            (Self::Plugin(name, _), Self::Plugin(next_name, version)) if name == next_name => {
                Self::Plugin(name, version)
            }
            _ => Self::Anyone,
        }
    }

    fn take(&mut self) -> Self {
        mem::take(self)
    }

    fn into_source(self, current_version: u64) -> Option<Arc<str>> {
        match self {
            Self::Plugin(name, version) if version == current_version => Some(name),
            _ => None,
        }
    }
}

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

fn cap_error_text(message: &str) -> String {
    match message.char_indices().nth(ERROR_BUBBLE_MAX_CHARS) {
        Some((end, _)) => format!("{}{TRUNCATION_PREFIX}", &message[..end]),
        None => message.to_owned(),
    }
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

/// A plan form row picked by the user, waiting on the plugin handler behind
/// it. `pick` is the form's pick counter at the moment of the press, and an
/// answer that comes back under a different one belongs to a form the user
/// has already left.
pub(super) struct PlanAction {
    pick: u64,
    /// The built-in outcome the row kept, which runs after the handler.
    then: Option<PlanRowAction>,
    deadline: Instant,
    answer: flume::Receiver<PlanActionOutcome>,
}

/// Everything the plan form has in flight, under one owner so that no reset
/// path can retire half of it. A menu chain and a picked row's handler both
/// answer on the Lua thread long after the key press that started them, and
/// the session they were asked about may be gone by then.
#[derive(Default)]
pub(super) struct PlanAnswers {
    /// In flight answer from the `ui.plan_form*` chains: whether the form
    /// opens for the draft that landed, and with which rows. Given up on
    /// after [`PLAN_FORM_ANSWER_WAIT`].
    pub(super) form: Option<(Instant, flume::Receiver<Option<PlanMenu>>)>,
    /// In flight answer from a picked plugin row.
    pub(super) action: Option<PlanAction>,
    /// Bumped by every pick the plan form makes, and by every path that walks
    /// away from a draft. Without it an outcome from a pick nobody waits on
    /// any more fires a second implement prompt on top of the one already
    /// running, or one against a session the user never picked in.
    pick: u64,
}

impl PlanAnswers {
    /// Retires the answers in flight and the pick they belong to, returning
    /// the pick a new one starts at. The counter only ever moves forward, so
    /// anything stamped with an earlier pick can only miss from here on.
    ///
    /// Every reset path funnels through [`App::reset_ui_chrome`], which calls
    /// this: a new one gets the invariant for free.
    fn abandon(&mut self) -> u64 {
        self.form = None;
        self.action = None;
        self.pick += 1;
        self.pick
    }
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
    pub(super) pending_input: PendingInput,
    pub(crate) run_id: u64,
    pub(super) retry_info: Option<RetryInfo>,
    pub(super) zones: ZoneRegistry,
    pub(super) selection_state: Option<SelectionState>,
    pub(super) clipboard: ClipboardState,
    pub(super) last_esc: Option<Instant>,

    pub(crate) storage: StateDir,
    /// The folder trust question this run was started with, `None` when the
    /// folder is trusted or has nothing to ask about. Frozen at startup on
    /// purpose: a kind the project adds mid-session is not something the user
    /// was shown, so `/trust` must not cover it and the next start asks.
    pub(crate) trust_question: Option<TrustQuestion>,
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
    /// The value Lua was last told about. A fast typist would otherwise wake
    /// every handler once per keystroke.
    announced_input: String,
    /// The caret Lua was last told about, diffed alongside the value so a
    /// caret that moved on its own is reported once and a frame that moved
    /// neither is silent.
    announced_cursor: usize,
    input_writer: InputWriter,
    /// Whether this frame's tick already fired `InputChanged`, so the focus
    /// announcement drained below it does not repeat that value. A tab that
    /// did not tick clears it too ([`Self::tick_background`]): whatever it
    /// holds was last announced in an earlier frame.
    input_fired_this_frame: bool,
    pub(super) keymap_reader: KeymapReader,
    pub(super) hint_reader: HintReader,
    hints: Watch<HintSnapshot>,
    pub(super) plan_answers: PlanAnswers,
    /// Actions produced outside key handling, drained by the event loop. A
    /// plan row handler answers long after the key press that started it.
    pub(super) pending_actions: Vec<Action>,
    pub(crate) restore_event_tx: Option<maki_agent::EventSender>,
    pub(super) restoring: Arc<AtomicBool>,
    subagent_answers: HashMap<String, flume::Sender<String>>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &Model,
        session: OpenSession,
        storage: StateDir,
        available_models: Arc<ArcSwapOption<ModelList>>,
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
        let state = SessionState::from_session(session, model, &storage);
        let typewriter = ui_config.typewriter_ms_per_char;
        let flash = ui_config.flash_duration();
        let input_box = InputBox::new(
            InputHistory::load(&storage, input_history_size),
            ui_config.max_input_lines,
        );
        let mut app = Self {
            chats: vec![Chat::new(
                state.session.id,
                None,
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
            pending_input: PendingInput::None,
            run_id: 0,
            retry_info: None,
            zones: ZoneRegistry::new(),
            selection_state: None,
            clipboard: ClipboardState::new(),
            last_esc: None,
            storage,
            trust_question: None,
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
            announced_input: String::new(),
            announced_cursor: 0,
            input_writer: InputWriter::Untouched,
            input_fired_this_frame: false,
            hints: Watch::seeded(hint_reader.load_full()),
            keymap_reader,
            hint_reader,
            plan_answers: PlanAnswers::default(),
            pending_actions: Vec::new(),
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

    /// Takes the spelling `maki.model.set` accepts, which is what the
    /// `/thinking` plugin passes through. A blank {input} toggles.
    ///
    /// Stores the clamped value rather than the typed one, so the status bar
    /// can never read `off` on a model that is really sending minimal effort.
    pub(crate) fn set_thinking(&mut self, input: &str) -> Result<ThinkingConfig, String> {
        if !self.state.model.supports_thinking() {
            return Err(THINKING_UNSUPPORTED_MSG.into());
        }
        self.state.thinking = ThinkingConfig::parse(input.trim(), self.state.thinking)
            .map_err(str::to_owned)?
            .clamped(&self.state.model);
        Ok(self.state.thinking)
    }

    pub(crate) fn set_fast(&mut self, fast: bool) -> Result<(), String> {
        let model = &self.state.model;
        if fast && !model.supports_fast() && !model.fast_pending() {
            return Err(FAST_UNSUPPORTED_MSG.into());
        }
        self.state.set_fast(fast);
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
            "thinking_options": model.thinking_options(),
            "fast": self.state.fast,
            "supports_thinking": model.supports_thinking(),
            "supports_fast": model.supports_fast(),
        })
    }

    /// What `maki.ui.input` hands to Lua: text and offsets only. The terminal
    /// cell the caret sits in has no answer for half the modes the UI can be
    /// in, and the line and column the cursor is on are a slice of the two
    /// fields below, which Lua can take for itself.
    pub(crate) fn input_snapshot(&self) -> serde_json::Value {
        let buffer = &self.input_box.buffer;
        serde_json::json!({
            "session_id": self.state.session.id.to_string(),
            "text": buffer.value(),
            "cursor": buffer.cursor_byte(),
            "version": buffer.version(),
        })
    }

    /// Refuses an edit the input has moved on from: another tab now focused, a
    /// version the buffer has left behind, or a range it has outgrown. See
    /// [`InputBox::replace_range`].
    ///
    /// The version cannot stand in for the session check. The counter is per
    /// buffer and every buffer starts at 0, so two tabs typed in about as much
    /// collide.
    ///
    /// An input the user cannot see is refused too, or the text would be sent
    /// later without ever having been seen. {area} is the terminal the answer
    /// is worked out against; see [`App::input_live`] for what hides the box.
    pub(crate) fn apply_input_edit(
        &mut self,
        edit: InputEdit,
        area: Rect,
    ) -> Result<serde_json::Value, String> {
        let focused = self.state.session.id.to_string();
        if edit.session_id != focused {
            return Err(format!(
                "input of session {} is not focused (session {focused} is)",
                edit.session_id
            ));
        }
        if !self.input_live(area) {
            return Err(INPUT_NOT_LIVE_ERR.to_string());
        }
        let current = self.input_box.buffer.version();
        if edit.version != current {
            return Err(format!(
                "input changed since version {} (it is now {current})",
                edit.version
            ));
        }
        self.input_box
            .replace_range(edit.start, edit.stop, &edit.text, edit.cursor)?;
        self.input_changed(InputWriter::Plugin(
            edit.plugin,
            self.input_box.buffer.version(),
        ));
        Ok(serde_json::json!(true))
    }

    /// The paths that keep the command palette in step with the input, and
    /// the only ones that can name a writer. Submit, discard, history recall
    /// and `$EDITOR` change the value without coming through here, and the
    /// tick diff reports those unlabelled: the version stamped here no longer
    /// matches what they left.
    fn input_changed(&mut self, writer: InputWriter) {
        self.command_palette.sync(&self.input_box.buffer.value());
        self.input_writer = self.input_writer.take().merge(writer);
    }

    /// One event per frame at most, and only when the caret or the text
    /// really moved, so holding a key down wakes a handler once and a frame
    /// that moved nothing leaves it asleep.
    ///
    /// A caret that moved on its own fires too, with `cursor_only` set. An
    /// input plugin anchored to what the caret is sitting in has no other way
    /// to learn it left: a popup on an `@` mention would stay up holding
    /// `<CR>` for a mention the user has arrowed out of.
    fn tick_input_changed(&mut self) -> Dirty {
        let value = self.input_box.buffer.value();
        let cursor = self.input_box.buffer.cursor_byte();
        self.input_fired_this_frame =
            value != self.announced_input || cursor != self.announced_cursor;
        if !self.input_fired_this_frame {
            self.input_writer = InputWriter::Untouched;
            return Dirty::NO;
        }
        // The value is the trigger and the writer only a label: submit,
        // discard, history recall, a draft restored on a session switch and
        // $EDITOR all change the value without passing a path that could set a
        // writer flag.
        let source = self
            .input_writer
            .take()
            .into_source(self.input_box.buffer.version());
        let cursor_only = value == self.announced_input;
        self.announced_input = value;
        self.announced_cursor = cursor;
        self.fire_input_changed(source, cursor_only);
        Dirty::NO
    }

    /// Focus moving to another tab changes what `maki.ui.input` answers
    /// without anyone editing anything, so the tab taking focus republishes
    /// what it holds.
    ///
    /// It stays quiet only when this frame's own tick already said it: the
    /// switch is drained below the tick that opened the frame, so a tab whose
    /// draft that tick restored has already fired the value. Comparing
    /// against what was last announced cannot stand in for the latch. Every
    /// tab keeps its own record, so two tabs holding the same text - an empty
    /// one is the common case - would announce nothing, and handlers would go
    /// on acting on the text of the tab they came from while `maki.ui.input`
    /// already answers with this one's.
    ///
    /// It is never `cursor_only`: the whole input changed hands, so a handler
    /// that only watches the text has to see it.
    pub(crate) fn announce_input(&mut self) {
        self.input_writer = InputWriter::Untouched;
        if mem::take(&mut self.input_fired_this_frame) {
            return;
        }
        self.announced_input = self.input_box.buffer.value();
        self.announced_cursor = self.input_box.buffer.cursor_byte();
        self.fire_input_changed(None, false);
    }

    fn fire_input_changed(&mut self, source: Option<Arc<str>>, cursor_only: bool) {
        self.fire_session_autocmd(
            "InputChanged",
            serde_json::json!({
                "text": self.announced_input,
                "cursor": self.announced_cursor,
                "version": self.input_box.buffer.version(),
                "source": source,
                "cursor_only": cursor_only,
            }),
        );
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

    /// For a warning the user did not ask for, so a run that produced several
    /// shows all of them rather than whichever was reported last.
    pub(crate) fn queue_flash(&mut self, msg: String) {
        self.status_bar.queue_flash(msg);
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
            if let Some(answered) = self.permission_prompt.handle_key(key) {
                let encoded = TaggedAnswer::new(&answered.id, answered.answer).encode();
                self.send_to_agent(answered.subagent_id.as_deref(), encoded);
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

        // A focused plugin window is the thing the user is typing into, so it
        // goes ahead of the rest. The keys an *unfocused* window claimed are
        // settled far below, after every modal here: a claim is up while the
        // user works under it, and a popup that holds `<CR>` must not answer
        // the Enter meant for the file picker opened over it.
        if self.float_mgr.handle_focused_key(key) {
            return Some(vec![]);
        }

        if self.search_modal.is_open() {
            match self.search_modal.handle_key(key) {
                SearchAction::Consumed => self.refresh_search_matches(),
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
                    if let InputAction::Changed = self.input_box.handle_paste_with_spaces(&path) {
                        self.input_changed(InputWriter::Anyone);
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

        // The command palette is the host's overlay over the chat input, so it
        // is answered here with the rest of them rather than below the claims:
        // a plugin holding `<Tab>` or `<CR>` must not take them from the `/`
        // command the user is typing into. Last in the pass, because every
        // modal above is drawn over it.
        //
        // Ctrl keys pass it by, as they always did: `Ctrl+C` closes it and the
        // rest belong to the input box and the built-in bindings. So does every
        // key in a subagent chat, where there is no input to complete.
        if self.is_main_chat() && !is_ctrl(&key) {
            match self
                .command_palette
                .handle_key(key, &self.input_box.buffer.value())
            {
                CommandAction::Consumed => return Some(vec![]),
                CommandAction::Execute(cmd) => {
                    self.input_box.discard();
                    return Some(self.execute_command(cmd, 0));
                }
                CommandAction::Complete(text) => {
                    self.input_box.set_input(text);
                    self.input_box.buffer.move_to_end();
                    self.input_changed(InputWriter::Anyone);
                    return Some(vec![]);
                }
                CommandAction::Passthrough => {}
            }
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
                let chat = &self.chats[self.active_chat];
                self.search_modal
                    .open(chat.scroll_pos(), chat.auto_scroll());
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

    /// One chain, in one order: `Ctrl+Z`, then the focused plugin window, then
    /// the host's own overlays and modals, then the keys an unfocused window
    /// claimed, then `Esc` while the agent streams, then a plugin's global
    /// bindings, then the built-in keys.
    ///
    /// A focused window is above the overlays because it is the window the
    /// user is in. A claim is below them because it is not: the popup it
    /// belongs to is up while the user works underneath, so a modal opened
    /// over it outranks it, and the claim comes back the moment the modal
    /// closes. The command palette is one of those overlays, answered in
    /// [`Self::dispatch_overlay`] with the rest, so a plugin's claim on
    /// `<Tab>` or `<CR>` leaves the `/` command being typed alone.
    ///
    /// Every step either answers the key or passes it on untouched, and no key
    /// is ever handed back after the fact, because a keystroke replayed into a
    /// UI that has moved on lands somewhere the user never aimed it.
    fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        self.clear_selection_unless_pending_copy();

        if key::SUSPEND.matches(key) && cfg!(unix) {
            return vec![Action::Suspend];
        }

        if let Some(actions) = self.dispatch_overlay(key) {
            return actions;
        }

        if self.float_mgr.handle_claimed_key(key) {
            return vec![];
        }

        if !self.reserved_by_host(key) && self.dispatch_override(key) {
            return vec![];
        }

        if let Some(actions) = self.handle_ctrl(key) {
            return actions;
        }

        if key::SCROLL_PAGE_UP.matches(key) {
            let page = self.chats[self.active_chat].page();
            self.active_chat().scroll(page);
            return vec![];
        }
        if key::SCROLL_PAGE_DOWN.matches(key) {
            let page = self.chats[self.active_chat].page();
            self.active_chat().scroll(-page);
            return vec![];
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

    /// The keys the host answers before any plugin *binding* sees them.
    ///
    /// `Ctrl+C` is how the user leaves, and the app has to stay leavable
    /// whatever a plugin bound or how badly its handler is stuck. `Ctrl+Z` is
    /// resolved above every step of the chain, for the same reason. Both come
    /// from [`maki_lua::RESERVED_KEYS`], the list `maki.keymap.set` refuses
    /// and the list a window's `keys` refuses, so neither can be bound nor
    /// claimed.
    ///
    /// A focused plugin window is handed `Ctrl+C` all the same, before this
    /// runs: being handed every key is what focus is, and the bundled pickers
    /// answer it by closing, which is how the user gets back to a chat they
    /// can quit from. `Ctrl+Z` is the one key not even a focused window sees,
    /// because suspending cannot wait on a plugin reading its events.
    ///
    /// `Esc` joins them while the agent runs, because stopping a turn is the
    /// other thing a user cannot be made to wait for. A popup that put
    /// `Esc close` in its own footer is already past this: an unfocused window
    /// takes its claims in [`FloatManager::handle_claimed_key`], which runs
    /// above, so the popup takes the first `Esc` and the next one, with the
    /// popup gone, arms the cancel.
    fn reserved_by_host(&self, key: KeyEvent) -> bool {
        is_reserved(key) || (self.status == Status::Streaming && key.code == KeyCode::Esc)
    }

    /// Whether a plugin binding claimed {key}. The binding the keymap matched
    /// travels with the request, so one dropped on the Lua thread cannot leave
    /// this having consumed a key nothing will act on.
    ///
    /// `false` is the only fall-through there is, and it is answered here, in
    /// the keystroke the user pressed: no binding matched, the plugin has too
    /// many callbacks in flight, or its load is gone. The built-in binding
    /// then runs below, with the UI exactly as the user left it. Nothing comes
    /// back from the Lua thread to be replayed. A key no notation names is one
    /// no plugin could have bound, so it falls through too.
    fn dispatch_override(&self, key: KeyEvent) -> bool {
        Key::from_event(key).is_some_and(|k| {
            self.keymap_reader
                .dispatch(k, |bind| self.lua_event_handle.run_keybind_callback(bind))
        })
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
            } else if let InputAction::Changed = self.input_box.handle_key(key) {
                self.input_changed(InputWriter::Anyone);
            }
            return vec![];
        }

        let streaming = self.status == Status::Streaming;
        match self.input_box.handle_key(key) {
            InputAction::Submit(sub) => self.handle_submit(sub),
            InputAction::Changed => {
                self.input_changed(InputWriter::Anyone);
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

    /// `maki trust` lives outside the TUI, so without this a "not now" or a
    /// `.maki/` created mid-session is unrecoverable without quitting.
    fn trust_folder(&mut self) -> Vec<Action> {
        let Some(question) = self.trust_question.clone() else {
            self.flash(NOTHING_TO_TRUST_MSG.into());
            return Vec::new();
        };
        if let Err(error) = project::grant(&self.storage, &question) {
            self.flash(error);
            return Vec::new();
        }
        let covered: Vec<String> = question.present.iter().map(GatedFile::to_string).collect();
        self.flash(format!("{TRUSTED_PREFIX}{}", covered.join(", ")));
        self.quit_with(ExitRequest::Reload)
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
            self.main_chat().show_user_message(display, Vec::new());
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
        self.permission_prompt.drop_subagent(&tool_use_id);

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
            add_cost(
                &mut self.state.subsidised_list_cost,
                tc.subsidised_list_cost,
            );
            add_cost(&mut self.chats[chat_idx].list_cost, tc.subsidised_list_cost);
            self.state.session_mut().add_model_usage(
                &tc.model,
                tc.usage
                    .billed_with_subsidised_list_cost(tc.cost, tc.subsidised_list_cost),
            );
            let ctx_size = tc.context_size.unwrap_or_else(|| tc.usage.context_tokens());
            self.set_context_size(chat_idx, ctx_size);
            self.chats[chat_idx].set_pending_turn_usage(tc.usage.format(tc.cost));
            if let Some(tool_id) = &subagent_id {
                let formatted = tc.usage.format_sum_cost(self.chats[chat_idx].cost);
                self.chats[0].set_tool_turn_usage(tool_id, formatted);
            }
        }

        // Compaction is the one thing that lowers the context size with no turn
        // behind it. The number is stored in the session meta and seeds the
        // next run's gauge, so left stale a session compacted just before exit
        // gets compacted again on resume.
        if let AgentEvent::CompactionDone {
            context_size_after, ..
        } = envelope.event
        {
            self.set_context_size(chat_idx, context_size_after);
        }

        let plan_path = if self.state.mode == Mode::Plan {
            self.state.plan.path()
        } else {
            None
        };
        let result = self.chats[chat_idx].handle_event(envelope.event, plan_path);

        if let ChatEventResult::QueueItemConsumed { text, images } = result {
            if chat_idx == 0 {
                self.on_queue_item_consumed(text, images);
            }
            return vec![];
        }

        if let ChatEventResult::PermissionRequest { id, tool, scopes } = result {
            let project_trusted = self.permissions.project_is_trusted();
            self.permission_prompt
                .push(id, tool, scopes, subagent_id.clone(), project_trusted);
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
                    self.main_chat().push(DisplayMessage::new(
                        DisplayRole::Error,
                        cap_error_text(&message),
                    ));
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

    /// Chat 0 is the session itself, and its size is the one stored in the
    /// session meta.
    fn set_context_size(&mut self, chat_idx: usize, size: u32) {
        self.chats[chat_idx].context_size = size;
        if chat_idx == 0 {
            self.state.context_size = size;
        }
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
            self.state.session.id,
            Some(id),
            subagent.name.clone(),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        chat.set_restore_channel(self.restore_event_tx.clone());
        chat.model_id = subagent.model.clone();
        chat.opts = subagent.opts;
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
            "/fast" => {
                match self.set_fast(!self.state.fast_intent()) {
                    Ok(()) => self.flash(
                        if self.state.pending_fast {
                            FAST_PENDING_MSG
                        } else if self.state.fast {
                            FAST_ON_MSG
                        } else {
                            FAST_OFF_MSG
                        }
                        .into(),
                    ),
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
            // Typing `/trust` is the consent, exactly like `maki trust add
            // --yes`, so there is no second question to ask here.
            "/trust" => self.trust_folder(),
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

    /// Derived fresh every time rather than snapshotted on open: output can
    /// land behind the modal, and a `!` shell command streams into a segment
    /// that already existed without ever setting [`Status::Streaming`]. The
    /// copy is cheaper than the match pass that follows it over the same bytes.
    fn refresh_search_matches(&mut self) {
        let chat = &mut self.chats[self.active_chat];
        self.search_modal
            .update_matches(|| chat.segment_search_texts());
        sync_search_highlight(&self.search_modal, chat);
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
            | self.tick_plan()
            | self.tick_file_picker()
            | self.tick_input_changed()
            | Dirty::any(self.chats.iter_mut().map(Chat::tick))
    }

    /// Both halves of the plan surface the Lua host answers asynchronously:
    /// the menu a draft opens with, and the built-in outcome a picked plugin
    /// row still owes. Drained for every session, since a plan that lands in
    /// a background tab has to reach its form too.
    pub(crate) fn tick_plan(&mut self) -> Dirty {
        self.tick_plan_form() | self.tick_plan_action()
    }

    /// Open the form with the menu the `ui.plan_form*` chains answered with.
    /// A chain that answers to keep it closed leaves it closed, and the
    /// plan-toggle key still reopens it. A chain that says nothing at all
    /// runs out of [`PLAN_FORM_ANSWER_WAIT`] and leaves the built-in form.
    pub(crate) fn tick_plan_form(&mut self) -> Dirty {
        let Some((deadline, rx)) = self.plan_answers.form.as_ref() else {
            return Dirty::NO;
        };
        let (answered, expired) = (rx.try_recv(), Instant::now() >= *deadline);
        let (menu, slow) = match answered {
            // A layer took the surface over, so the host draws nothing. The
            // menu goes with it, since its rows answer for the draft they
            // were built for.
            Ok(None) => {
                self.plan_answers.abandon();
                self.plan_form.forget_menu();
                return Dirty::from(self.plan_form.is_visible());
            }
            Ok(Some(menu)) => (menu, false),
            // The host went away mid-question, so nobody is drawing the plan.
            Err(flume::TryRecvError::Disconnected) => (builtin_menu(), false),
            Err(flume::TryRecvError::Empty) if expired => (builtin_menu(), true),
            Err(flume::TryRecvError::Empty) => return Dirty::NO,
        };
        self.plan_answers.abandon();
        self.plan_form.open_with(menu);
        if slow {
            self.flash(FLASH_PLAN_FORM_SLOW.into());
        }
        Dirty::YES
    }

    /// The built-in outcome a picked plugin row kept, once its handler has
    /// answered.
    ///
    /// A handler that says `false` is gating the row on a check of its own
    /// and stays quiet. A handler that failed, or a pick that reached no
    /// handler, left the user watching the menu vanish for nothing, and says
    /// so.
    fn tick_plan_action(&mut self) -> Dirty {
        let Some(pending) = self.plan_answers.action.as_ref() else {
            return Dirty::NO;
        };
        // "Lost" is the host never having taken the pick, which reads
        // differently to the user than a handler that ran and failed.
        let (answered, expired) = (
            pending.answer.try_recv(),
            Instant::now() >= pending.deadline,
        );
        let (outcome, lost) = match answered {
            Ok(outcome) => (outcome, false),
            Err(flume::TryRecvError::Disconnected) => (PlanActionOutcome::Failed, true),
            Err(flume::TryRecvError::Empty) if expired => (PlanActionOutcome::Failed, true),
            Err(flume::TryRecvError::Empty) => return Dirty::NO,
        };
        let pending = self.plan_answers.action.take().expect("checked above");
        if pending.pick != self.plan_answers.pick {
            // Running the outcome of a pick the user has navigated away from
            // would be a second implement prompt behind the one they asked
            // for, or one against a session they never picked in.
            tracing::debug!(outcome = ?outcome, "dropping the answer of a stale plan form pick");
            return Dirty::NO;
        }
        match outcome {
            PlanActionOutcome::Proceed => {
                let actions = match pending.then {
                    Some(PlanRowAction::Implement) => self.implement_plan(false),
                    Some(PlanRowAction::ClearAndImplement) => self.implement_plan(true),
                    Some(PlanRowAction::Refine) | None => vec![],
                };
                self.pending_actions.extend(actions);
            }
            PlanActionOutcome::Vetoed => {}
            PlanActionOutcome::Failed => self.flash(
                if lost {
                    FLASH_PLAN_ACTION_LOST
                } else {
                    FLASH_PLAN_ACTION_FAILED
                }
                .into(),
            ),
        }
        Dirty::YES
    }

    /// Ask the `ui.plan_form*` chains what to draw for the draft that just
    /// landed, starting from the host's own rows.
    ///
    /// Only asked when a plugin is layering one of them. With no layer the
    /// chain answers with the host's own rows by construction, and the
    /// roundtrip through a request loop that may be busy would cost a stock
    /// install its form.
    pub(super) fn offer_plan_form(&mut self, path: Option<&str>) {
        // A new draft retires the pick the last one was waiting on, handler
        // and built-in outcome both. The handler may well still be running,
        // but its answer is stamped with a pick nobody waits on any more, so
        // the outcome the row promised is gone and the user is told.
        let lost_pick = self.plan_answers.action.is_some();
        self.plan_answers.abandon();
        if lost_pick {
            self.flash(FLASH_PLAN_ACTION_LOST.into());
        }
        let Some(path) = path.filter(|_| self.lua_event_handle.plan_form_layered()) else {
            self.plan_form.open_with(builtin_menu());
            return;
        };
        // The last draft's menu is not this draft's, and the chain has not
        // answered with one yet.
        self.plan_form.forget_menu();
        let answer = self.lua_event_handle.open_plan_form(
            path.to_owned(),
            self.state.session.id.to_string(),
            builtin_rows(),
        );
        self.plan_answers.form = Some((Instant::now() + PLAN_FORM_ANSWER_WAIT, answer));
    }

    /// What a tab nobody is looking at still owes the frame. Its floats have
    /// to drain, or a plugin writing to a window off screen would lose the
    /// output, and its plan form too, or a draft in a background tab would
    /// sit unanswered until the user focused it. Nothing it holds was
    /// announced this frame, because it never diffed its input, so the
    /// announcement has to speak when this tab takes focus.
    pub fn tick_background(&mut self) {
        self.input_fired_this_frame = false;
        let _ = self.float_mgr.tick();
        let _ = self.tick_plan();
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
            self.status_bar.cadence(
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
            self.refresh_search_matches();
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
        if let InputAction::Changed = self.input_box.handle_paste(text) {
            self.input_changed(InputWriter::Anyone);
        }
    }

    fn handle_plan_form_action(&mut self, action: PlanFormAction) -> Vec<Action> {
        match action {
            PlanFormAction::Consumed | PlanFormAction::Passthrough => vec![],
            PlanFormAction::Hide => {
                self.plan_answers.abandon();
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
            PlanFormAction::Implement => {
                self.plan_answers.abandon();
                self.implement_plan(false)
            }
            PlanFormAction::ClearAndImplement => {
                self.plan_answers.abandon();
                self.implement_plan(true)
            }
            PlanFormAction::Plugin {
                row,
                generation,
                then,
            } => {
                // Snapshot the parallel flag before reset() clears it, since
                // the handler is told what it was.
                let parallel = self.plan_form.parallel();
                let path = self
                    .state
                    .plan
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                // Plan state is per session, so the handler is told which one
                // fired instead of assuming the focused tab.
                let session = self.state.session.id.to_string();
                self.plan_form.reset();
                // The generation travels with the pick, so a chain that
                // resumed late and installed its handlers over this menu
                // cannot answer for the rows the user saw.
                let answer = self
                    .lua_event_handle
                    .run_plan_action(session, generation, row, path, parallel);
                let pick = self.plan_answers.abandon();
                self.plan_answers.action = Some(PlanAction {
                    pick,
                    then,
                    deadline: Instant::now() + PLAN_ACTION_ANSWER_WAIT,
                    answer,
                });
                vec![]
            }
        }
    }

    /// Snapshot of the current plan for `maki.plan.read()`. `content` stays
    /// `None` when the plan is not ready or the file cannot be read, which an
    /// empty plan is not.
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

fn sync_search_highlight(modal: &SearchModal, chat: &mut Chat) {
    let idx = modal.current_segment_index();
    if let Some(i) = idx {
        chat.scroll_to_segment(i);
    }
    chat.set_highlight_segment(idx);
}
