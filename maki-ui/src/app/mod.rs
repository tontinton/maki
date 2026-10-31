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
mod typeahead;
pub(crate) mod view;

use std::collections::{HashMap, HashSet};
use std::env;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::OpenSession;
use crate::app::tasks::{BackgroundMoves, TaskClock, TaskOutcome, TaskParent, count_phrase};
use crate::chat::Chat;
use crate::chat::{CANCELLED_TEXT, ChatEventResult, DONE_TEXT, ERROR_TEXT};
use crate::clipboard::ClipboardState;
use crate::components::alert_modal::AlertModal;
use crate::components::btw_modal::BtwModal;
use crate::components::command::{
    CommandAction, CommandPalette, ParsedCommand, PluginAccess, SUBMITS_PROMPT,
};
use crate::components::file_picker::{FilePickerModal, FilePickerModalAction};
use crate::components::help_modal::HelpModal;
use crate::components::input::{InputAction, InputBox, Placeholder, Submission, sent_text};
use crate::components::keybindings::key;
use crate::components::login_picker::{LoginPicker, LoginPickerAction};
use crate::components::lua_float::FloatManager;
use crate::components::mcp_picker::{McpPicker, McpPickerAction};
use crate::components::model_picker::{ModelPicker, ModelPickerAction};
use crate::components::pack_review::{PackReview, PackReviewAction};
use crate::components::permission_prompt::PermissionPrompt;
use crate::components::plan_form::{PlanForm, PlanFormAction, builtin_menu, builtin_rows};
use crate::components::provider_auth::provider_entries;
use crate::components::rewind_picker::{RewindPicker, RewindPickerAction};
use crate::components::scrollbar;
use crate::components::search_modal::{SearchAction, SearchModal};
use crate::components::status_bar::StatusBar;
use crate::components::theme_picker::{ThemePicker, ThemePickerAction};
use crate::components::usage_modal::{UsageFetchState, UsageModal};
use crate::components::{
    Action, BackgroundOutcome, DisplayMessage, DisplayRole, ExitRequest, Overlay, RetryInfo,
    Status, is_ctrl,
};
use crate::markdown::TRUNCATION_PREFIX;
use crate::repaint::{Cadence, Dirty, Watch};
use crate::selection::{SelectionState, SelectionZone, ZoneRegistry};
use crate::text_buffer::{RuleSet, TextBuffer, Writers, normalize_line_endings, sanitize};
use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use maki_agent::permissions::{PermissionManager, TaggedAnswer};
use maki_agent::{
    AgentEvent, Envelope, ImageSource, McpConfigErrors, McpPromptInfo, McpSnapshotReader,
    SessionMailbox, SharedBuf, SharedMessages, SubagentInfo, ToolPart,
};
use maki_config::project::{self, GatedFile, TrustQuestion};
use maki_config::{ModelPolicy, UiConfig};
use maki_lua::agent_autocmd::HostEvent;
use maki_lua::{
    BuiltinAction, EventHandle, HintReader, HintSnapshot, InputDecorate, InputEdit, InputMarks,
    InputMode, InputRule, InputRulesReader, InputSnapshot, InputSubmit, Key, KeymapReader,
    LuaCommandReader, PLAN_FORM_SLOT_DEADLINE, PLAN_ROW_HANDLER_DEADLINE, PackCommand,
    PackPreparation, Permission, PlanActionOutcome, PlanMenu, PlanRowAction, PluginReach, WinView,
    is_reserved,
};
use maki_providers::models_cache::ModelList;
use maki_providers::{ContentBlock, Message, Model, ThinkingConfig, TokenUsage, add_cost};
use maki_storage::StateDir;
use maki_storage::input_history::InputHistory;
use maki_storage::model::persist_thinking;
use maki_storage::sessions::StoredTokenUsage;

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
use shell::ShellPrefix;
use typeahead::{Hold, InputAt, KeyboardFocus, Pressed, Prompts, Sight, Turn, Typeahead};

const CANCEL_MSG: &str = "Cancelled.";
const MAIN_CHAT: usize = 0;
const EXIT_WORD: &str = "exit";
const EXIT_COMMAND: &str = "/exit";
/// Bypasses the per-run staleness filter because re-bake replies
/// don't belong to any real agent run.
pub(crate) const RESTORE_RUN_ID: u64 = u64::MAX;
const FLASH_CANCEL: &str = "Press esc again to stop...";
const FLASH_REWIND: &str = "Press esc again to rewind...";
// Short, because the flash shares the status bar with the model and cost.
const ESC_ONE_RUNNING: &str = "1 task in background";
const ESC_MANY_RUNNING_SUFFIX: &str = " tasks in background";
const QUIT_ONE_RUNNING: &str = "1 background task stops if you quit again";
const QUIT_MANY_RUNNING_SUFFIX: &str = " background tasks stop if you quit again";
const RELOAD_ONE_RUNNING: &str = "1 background task would stop. /reload! reloads anyway";
const RELOAD_MANY_RUNNING_SUFFIX: &str = " background tasks would stop. /reload! reloads anyway";
const NEW_ONE_RUNNING: &str = "1 background task would stop. /new! starts anyway";
const NEW_MANY_RUNNING_SUFFIX: &str = " background tasks would stop. /new! starts anyway";
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
const PROVIDER_FOLDED_SUFFIX: &str = ": folded away";
const PROVIDER_UNFOLDED_SUFFIX: &str = ": unfolded";
const PACK_CHANGES_DECLINED: &str = "Package changes declined";
pub(crate) const HELD_KEYS_DROPPED: &str =
    "Dropped the keys typed before a prompt opened, so they could not answer it";
const INPUT_CHANGED_UNSENT: &str = "The input changed while you typed. Press Enter to send.";
pub(crate) const HELD_KEYS_ABANDONED: &str = "Dropped keys typed behind a plugin.";
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
pub(crate) const STALE_VERSION_ERR: &str = "input changed since version";
const SUBMIT_NOT_LIVE_ERR: &str =
    "Enter would not reach the chat input right now, so it cannot be sent";
const NOTHING_TO_SEND_ERR: &str = "the chat input is empty, so there is nothing to send";
const NO_COMMAND_MATCHED_ERR: &str =
    "the command palette is open but matches no command, so nothing was sent";
pub(crate) const SEND_MESSAGE: &str = "sending a chat message";
pub(crate) const RUN_SHELL_LINE: &str = "running a shell line";
const RUNS_SHELL: PluginAccess = PluginAccess::Needs(&[Permission::Run]);
pub(crate) const WRITTEN_COMMAND: &str = "command";
pub(crate) const WRITTEN_SHELL_LINE: &str = "shell line";
pub(crate) const WRITTEN_EXIT: &str = "exit line";
pub(crate) const WRITTEN_MESSAGE: &str = "message";
pub(crate) const TYPE_IT_YOURSELF: &str = "retype it yourself in a fresh input";
/// A writer whose name was lost, like that of a draft read back from disk.
pub(crate) const UNKNOWN_WRITER: &str = "a plugin";

/// For a typed key `Swallowed` and `Handled` mean the same thing, the key is
/// used up. A plugin's submit has to tell them apart, because a swallowed
/// Enter ran nothing and sent nothing.
enum PaletteKey {
    Passthrough,
    Swallowed,
    Handled(Vec<Action>),
    /// Enter would run a command a sender may not, and left it in the input.
    Refused(String),
}

/// What Enter does with the input once the command palette passed it on.
/// The send, the check a plugin's send passes first and the check on the
/// plugins that wrote the text all read it here, so they cannot disagree.
enum Sending {
    /// Retries the turn a login cut short, whatever the input holds.
    AuthRetry,
    Nothing,
    /// `exit`, which quits like `/exit`.
    Exit,
    Shell(ShellPrefix),
    Message,
}

impl Sending {
    /// What Enter sends {text} as, once the command palette passed it on
    /// and no login retry waits.
    fn of(text: &str, has_images: bool) -> Self {
        if text.is_empty() && !has_images {
            return Self::Nothing;
        }
        if text.trim() == EXIT_WORD {
            return Self::Exit;
        }
        if let Some(prefix) = shell::parse_shell_prefix(text) {
            return Self::Shell(prefix);
        }
        Self::Message
    }

    /// A plugin needs what the direct way needs: `exit` what `/exit` needs,
    /// a shell line `run` since it starts a process, and a message what a
    /// turn needs, even one that waits in the queue first.
    fn refusal(&self, caller: &PluginReach, palette: &CommandPalette) -> Option<String> {
        match self {
            Self::Nothing => None,
            Self::Exit => palette.resolve_for_plugin(EXIT_COMMAND, caller).err(),
            Self::Shell(_) => RUNS_SHELL.refusal(RUN_SHELL_LINE, caller),
            Self::AuthRetry | Self::Message => SUBMITS_PROMPT.refusal(SEND_MESSAGE, caller),
        }
    }

    /// What the text is called when each plugin that wrote it has to pass
    /// [`Self::refusal`] too, `None` when nothing of it goes out. A message
    /// is held to it as well, because the user cannot be trusted to have
    /// read it. A plugin can hide text in more ways than we could ever spot,
    /// like a conceal, a char that draws nothing or a line scrolled away.
    /// A login retry sends none of the text.
    fn written_as(&self) -> Option<&'static str> {
        match self {
            Self::Exit => Some(WRITTEN_EXIT),
            Self::Shell(_) => Some(WRITTEN_SHELL_LINE),
            Self::Message => Some(WRITTEN_MESSAGE),
            Self::AuthRetry | Self::Nothing => None,
        }
    }

    /// Whether Enter acts on the text right away rather than handing it to
    /// the model.
    fn runs(&self) -> bool {
        matches!(self, Self::Exit | Self::Shell(_))
    }
}

/// Why a send may not go out. A key typed blind to what it would send sends
/// nothing, see [`Sight`]. Otherwise {refusal} is asked of {caller}, the
/// plugin sending if any.
fn send_refusal(
    sight: Sight,
    caller: Option<&PluginReach>,
    refusal: impl FnOnce(&PluginReach) -> Option<String>,
) -> Option<String> {
    if sight.blind {
        return Some(INPUT_CHANGED_UNSENT.to_owned());
    }
    caller.and_then(refusal)
}

/// Why Enter may not send {what} the plugins that wrote {buffer} left
/// there: {refusal}, asked of each of them. A plugin with `input` may write
/// anything, so what one of them could not run itself waits for the user to
/// retype it in a fresh input.
fn written_refusal(
    buffer: &TextBuffer,
    what: &str,
    refusal: impl Fn(&PluginReach) -> Option<String>,
) -> Option<String> {
    let refused: Vec<(&PluginReach, String)> = buffer
        .writers()
        .iter()
        .filter_map(|writer| Some((writer, refusal(writer)?)))
        .collect();
    let (_, reason) = refused.first()?;
    let names = refused
        .iter()
        .map(|(writer, _)| writer.owner().map_or(UNKNOWN_WRITER, AsRef::as_ref))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(
        what,
        writers = %names,
        reason = %reason,
        "send refused: a plugin wrote what it may not run"
    );
    Some(format!(
        "{names} wrote part of this {what}: {TYPE_IT_YOURSELF} ({reason})"
    ))
}

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
    AgentError { message: String },
}
impl Notification {
    /// Prompts blocking the agent outrank turn completions.
    pub(crate) fn is_urgent(&self) -> bool {
        !matches!(self, Self::TurnComplete { .. } | Self::AgentError { .. })
    }

    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::TurnComplete { .. } => "Turn complete",
            Self::PermissionRequested { .. } => "Permission requested",
            Self::AuthenticationRequired => "Authentication required",
            Self::QuestionRequested => "Question requested",
            Self::PlanReady => "Plan ready",
            Self::AgentError { .. } => "Agent error",
        }
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
            Self::AgentError { message } => message.clone(),
        }
    }

    pub(crate) fn error_completion() -> Self {
        Self::AgentError {
            message: "Agent stopped with an error".into(),
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

/// The task an event belongs to: a transcript names it itself, every other
/// event through its subagent stamp.
fn task_id_of(envelope: &Envelope) -> Option<&str> {
    match &envelope.event {
        AgentEvent::SubagentHistory { tool_use_id, .. } => Some(tool_use_id.as_str()),
        _ => envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.as_str()),
    }
}

fn started_by(info: &SubagentInfo) -> Option<TaskParent> {
    info.parent_call
        .as_deref()
        .map(|call| TaskParent::Started(Arc::from(call)))
}

/// A chat can open before its session speaks: a moved call's tab, or a
/// subagent a plugin adopted. The first info naming it fills in what it
/// lacks: the sender its permission answers go to, the inbox its input
/// feeds, and the model and options its header shows.
fn fill_missing_subagent_info(chat: &mut Chat, info: &SubagentInfo) {
    if chat.parent.is_none() {
        chat.parent = started_by(info);
    }
    if chat.is_finished() {
        return;
    }
    if chat.answer_tx.is_none() {
        chat.answer_tx.clone_from(&info.answer_tx);
    }
    if chat.inbox.is_none() {
        chat.inbox.clone_from(&info.inbox);
    }
    if chat.model_id.is_none() {
        chat.model_id.clone_from(&info.model);
    }
    chat.opts = chat.opts.or(info.opts);
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
    /// Cancel marks a task finished while its agent is still shutting down,
    /// so the user can delete it before its last events arrive. Those events
    /// would bring it back, so we drop them.
    pub(super) deleted_tasks: HashSet<String>,
    pub(crate) input_box: InputBox,
    pub(super) command_palette: CommandPalette,
    pub(super) theme_picker: ThemePicker,
    pub(super) model_picker: ModelPicker,
    pub(super) login_picker: LoginPicker,
    pub(super) mcp_picker: McpPicker,
    pub(super) rewind_picker: RewindPicker,
    pub(super) alert_modal: AlertModal,
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
    /// How long an `exit_on_done` exit waits for background work, `None` for
    /// not at all. One deadline, from the first held exit on, covers the
    /// whole wait, so a chain of woken runs cannot keep the process up.
    pub(crate) background_wait: Option<Duration>,
    background_deadline: Option<Instant>,
    pub(crate) queue: MessageQueue,
    recoverable_queue: Vec<String>,
    pub answer_tx: Option<flume::Sender<String>>,
    /// Bumped whenever the host opens a prompt, see [`Self::open_prompt`].
    /// The plugin windows that need input count their own, see
    /// [`Self::prompts`].
    prompts_opened: u64,
    pub(crate) run_id: u64,
    pub(super) retry_info: Option<RetryInfo>,
    pub(super) zones: ZoneRegistry,
    pub(super) selection_state: Option<SelectionState>,
    pub(super) clipboard: ClipboardState,
    pub(super) last_esc: Option<Instant>,
    /// The last key was a `Ctrl+C` that copied, see [`Self::handle_ctrl`].
    quit_copied: bool,
    pub(super) last_quit: Option<Instant>,
    /// Background tasks running in the other open sessions, which a quit
    /// ends too. Only the event loop sees every session, so it keeps this.
    pub(crate) background_elsewhere: usize,
    pub(super) background_moves: BackgroundMoves,
    /// Final results of moved calls whose receipt has not landed yet. The two
    /// travel on different channels, and the receipt opens the tab.
    pub(super) early_finals: HashMap<String, AgentEvent>,
    /// The last parts each call reported, by call id. Kept from the first
    /// report, since a call reports before it moves and `/tasks` lists the
    /// parts only after.
    pub(super) tool_parts: HashMap<String, Vec<ToolPart>>,

    pub(crate) storage: StateDir,
    /// The folder trust question this run was started with, `None` when the
    /// folder is trusted or has nothing to ask about. Frozen at startup on
    /// purpose: a kind the project adds mid-session is not something the user
    /// was shown, so `/trust` must not cover it and the next start asks.
    pub(crate) trust_question: Option<TrustQuestion>,
    pub(crate) usage_slot: Arc<ArcSwapOption<UsageFetchState>>,
    pub(crate) shared_history: Option<SharedMessages>,
    pub(crate) image_paste_rx: Vec<flume::Receiver<Result<ImageSource, String>>>,
    pub(crate) primary_paste_rx: Vec<flume::Receiver<Option<String>>>,
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
    /// The input Lua was last told about. A fast typist would otherwise wake
    /// every handler once per keystroke. A caret or a selection that moved on
    /// its own is reported once, and a frame that moved nothing is silent.
    /// The version stands in for the text, so an idle tick never compares
    /// the whole value.
    announced_input: InputAt,
    input_writer: InputWriter,
    /// Whether this frame's tick already fired `InputChanged`, so the focus
    /// announcement drained below it does not repeat that value. A tab that
    /// did not tick clears it too ([`Self::tick_background`]): whatever it
    /// holds was last announced in an earlier frame.
    input_fired_this_frame: bool,
    pub(super) keymap_reader: KeymapReader,
    pub(super) hint_reader: HintReader,
    hints: Watch<HintSnapshot>,
    pub(super) input_rules_reader: InputRulesReader,
    input_rules: Watch<Vec<InputRule>>,
    pub(super) plan_answers: PlanAnswers,
    /// Actions produced outside key handling, drained by the event loop. A
    /// plan row handler answers long after the key press that started it.
    pub(super) pending_actions: Vec<Action>,
    typeahead: Typeahead,
    pub(crate) restore_event_tx: Option<maki_agent::EventSender>,
    pub(super) restoring: Arc<AtomicBool>,
    /// The agent's mailbox, held here so the live session outlives every
    /// write of its receipts.
    pub(crate) mailbox: Option<SessionMailbox>,
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
        input_rules_reader: InputRulesReader,
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
        let mut input_box = InputBox::new(
            InputHistory::load(
                &storage,
                &env::current_dir().unwrap_or_else(|_| PathBuf::from(&state.session.cwd)),
                input_history_size,
            ),
            ui_config.max_input_lines,
        );
        let input_rules = input_rules_reader.load_full();
        input_box
            .buffer
            .set_rules(RuleSet::with_plugins(&input_rules));
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
            deleted_tasks: HashSet::new(),
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
            alert_modal: AlertModal::new(),
            help_modal: HelpModal::new(),
            usage_modal: UsageModal::new(),
            btw_modal: BtwModal::new(typewriter, ui_config.thinking_lines),
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
            background_wait: None,
            background_deadline: None,
            queue: MessageQueue::default(),
            recoverable_queue: Vec::new(),
            answer_tx: None,
            prompts_opened: 0,
            run_id: 0,
            retry_info: None,
            zones: ZoneRegistry::new(),
            selection_state: None,
            clipboard: ClipboardState::new(),
            last_esc: None,
            quit_copied: false,
            last_quit: None,
            background_elsewhere: 0,
            background_moves: BackgroundMoves::default(),
            early_finals: HashMap::new(),
            tool_parts: HashMap::new(),
            storage,
            trust_question: None,
            usage_slot: Arc::new(ArcSwapOption::empty()),
            shared_history: None,
            image_paste_rx: vec![],
            primary_paste_rx: vec![],
            storage_writer,
            last_sent: None,
            shell: shell::ShellState::default(),
            ui_config,
            permissions,
            model_policy: Arc::clone(&model_policy),
            lua_event_handle,
            announced_model_spec: model.spec(),
            announced_input: InputAt::default(),
            input_writer: InputWriter::Untouched,
            input_fired_this_frame: false,
            hints: Watch::seeded(hint_reader.load_full()),
            keymap_reader,
            hint_reader,
            input_rules: Watch::seeded(input_rules),
            input_rules_reader,
            plan_answers: PlanAnswers::default(),
            pending_actions: Vec::new(),
            typeahead: Typeahead::default(),
            restore_event_tx: None,
            mailbox: None,
            restoring: Arc::new(AtomicBool::new(false)),
        };
        app.model_picker.set_recents(
            maki_storage::model::read_recents(&app.storage)
                .into_iter()
                .filter(|spec| model_policy.allows(spec))
                .collect(),
        );
        app.model_picker
            .set_expanded(maki_storage::model::read_expanded_providers(&app.storage));
        // The manager arrives forked from the prototype the process was
        // started with, so a tab that resumes or spawns blank runs on
        // `--yolo` until its own meta is read back here.
        app.apply_stored_permissions(&app.state.session.meta);
        app
    }

    pub(crate) fn main_chat(&mut self) -> &mut Chat {
        &mut self.chats[MAIN_CHAT]
    }

    fn is_main_chat(&self) -> bool {
        self.active_chat == MAIN_CHAT
    }

    /// Whether the chat in front shows the input box. A running subagent's
    /// chat does, so a message can be queued for that subagent while it is
    /// watched. A finished one is a transcript, so it does not.
    pub(super) fn chat_accepts_input(&self) -> bool {
        self.is_main_chat() || !self.chats[self.active_chat].is_finished()
    }

    /// Stores this tab's focus into the mode every tab shares, so only the
    /// focused tab may call it. `Insert` only while typed keys reach the chat
    /// input. Pickers count as taking the keyboard even though they have a
    /// search field, because they answer their keys before any binding, and
    /// the mode is about the chat input.
    pub(crate) fn sync_input_mode(&self) -> InputMode {
        self.keymap_reader.sync_mode(self.input_focused())
    }

    fn input_focused(&self) -> bool {
        self.chat_accepts_input()
            && !self.any_overlay_open()
            && !self.plan_form_active()
            && !self.queue_focused()
    }

    /// The panel in a subagent chat shows that subagent's inbox, which
    /// the session queue's focus does not reach.
    fn queue_focused(&self) -> bool {
        self.is_main_chat() && self.queue.focus().is_some()
    }

    fn keyboard_focus(&self) -> KeyboardFocus {
        KeyboardFocus {
            window: self.float_mgr.focused_id(),
            input: self.input_focused(),
            prompts: self.prompts(),
        }
    }

    /// Every modal and prompt that answers keys on its own, the one list held
    /// keys check, see [`Prompts`]. Pickers are not in it: typing ahead into
    /// one a key handler opened is what the user meant.
    fn prompts(&self) -> Option<Prompts> {
        let overlays = self.overlays().map(|overlay| overlay.is_prompting());
        let up = [self.turn_awaits_auth(), self.plan_form_active()]
            .into_iter()
            .chain(overlays)
            .fold(0, |up, open| up << 1 | u32::from(open));
        (up != 0).then(|| Prompts {
            up,
            opened: self.prompts_opened + self.float_mgr.prompts_opened(),
        })
    }

    /// The mode is global, so the caller keeps {announced} across tabs, and a
    /// tab switch that leaves the mode alone stays quiet.
    pub(crate) fn announce_mode(&self, announced: &mut InputMode) {
        let mode = self.sync_input_mode();
        let old = mem::replace(announced, mode);
        if old != mode {
            self.lua_event_handle.fire(
                None,
                HostEvent::ModeChanged {
                    old_mode: old.letter(),
                    new_mode: mode.letter(),
                },
            );
        }
    }

    /// A running task with no inbox takes no messages, so its input says how
    /// to stop it instead. The background count is the receipts the model
    /// holds, so it leaves out a plugin's own sessions, which wake nobody.
    pub(super) fn input_placeholder(&self) -> Placeholder {
        let chat = &self.chats[self.active_chat];
        let waiting_on = self.live_mailbox().map_or(0, SessionMailbox::outstanding);
        if !self.is_main_chat() {
            if chat.inbox.is_none() && !chat.is_finished() {
                Placeholder::TaskStop
            } else {
                Placeholder::SubagentQueue
            }
        } else if self.status == Status::Streaming {
            Placeholder::Queue
        } else if waiting_on > 0 {
            Placeholder::Background(waiting_on)
        } else if self.state.session.messages().is_empty() {
            Placeholder::Suggestion
        } else {
            Placeholder::Blank
        }
    }

    /// One input box serves every chat, so the draft in it moves to the chat
    /// being left and the new chat's draft comes back. Without this, `Enter`
    /// in a subagent's chat would send a message typed for the main agent.
    pub(super) fn set_active_chat(&mut self, idx: usize) {
        if idx == self.active_chat {
            return;
        }
        let draft = mem::take(&mut self.chats[idx].draft);
        self.chats[self.active_chat].draft = self.input_box.swap_draft(draft);
        self.active_chat = idx;
    }

    /// The session's draft is the main agent's, which the box only holds while
    /// the main chat is in front. Its writers come from the same place.
    pub(super) fn main_draft(&self) -> (String, &Writers) {
        if self.is_main_chat() {
            (self.input_box.draft_text(), self.input_box.draft_writers())
        } else {
            let draft = &self.chats[MAIN_CHAT].draft;
            (draft.text.clone(), draft.writers())
        }
    }

    fn plan_form_open(&self) -> bool {
        self.state.mode == Mode::Plan && self.plan_form.is_visible()
    }

    fn plan_form_active(&self) -> bool {
        self.is_main_chat() && self.plan_form_open()
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
        self.fire_session_event(HostEvent::ModelChanged {
            model: self.model_state(),
            previous_spec,
        });
    }

    /// Takes the spelling `maki.model.set` accepts, which is what the
    /// `/thinking` plugin passes through. A blank {input} toggles.
    ///
    /// Stores the clamped value rather than the typed one, so the status bar
    /// can never read `off` on a model that is really sending minimal effort.
    ///
    /// The value is also written to disk here, and only here: the next run
    /// seeds its sessions from it (`SessionDefaults`), the same way the model
    /// is remembered. A clamp on model change is not the user's choice, so it
    /// does not overwrite the file.
    pub(crate) fn set_thinking(&mut self, input: &str) -> Result<ThinkingConfig, String> {
        if !self.state.model.supports_thinking() {
            return Err(THINKING_UNSUPPORTED_MSG.into());
        }
        self.state.thinking = ThinkingConfig::parse(input.trim(), self.state.thinking)
            .map_err(str::to_owned)?
            .clamped(&self.state.model);
        self.state.pending_thinking = None;
        persist_thinking(&self.storage, self.state.thinking.into());
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
            "thinking_shown": self.state.thinking.shown(model).to_string(),
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
        serde_json::json!(self.snapshot_input())
    }

    fn snapshot_input(&self) -> InputSnapshot {
        let buffer = &self.input_box.buffer;
        InputSnapshot {
            session_id: self.state.session.id.to_string(),
            text: buffer.value().to_owned(),
            cursor: buffer.cursor_byte(),
            anchor: buffer.anchor(),
            version: buffer.version(),
        }
    }

    /// Refuses an edit the input has moved on from: another tab now focused,
    /// a version the edit cannot be mapped forward from (any older one when
    /// it is strict), or a range the value has outgrown. See
    /// [`InputBox::apply_edits`]. Answers the version and the cursor the edit
    /// left, which the caller cannot work out for itself when its own version
    /// was stale.
    ///
    /// The version cannot stand in for the session check. The counter is per
    /// buffer and every buffer starts at 0, so two tabs typed in about as much
    /// collide.
    ///
    /// An input the user cannot see is refused too, or the text would be sent
    /// later without ever having been seen. {area} is the terminal the answer
    /// is worked out against, see [`App::input_live`] for what hides the box.
    ///
    /// The Lua side already checked `input`. The plugin joins the writers of
    /// the text it changes, see [`written_refusal`].
    pub(crate) fn apply_input_edit(
        &mut self,
        edit: InputEdit,
        area: Rect,
    ) -> Result<serde_json::Value, String> {
        self.check_input_session(&edit.session_id)?;
        if !self.input_live(area) {
            return Err(INPUT_NOT_LIVE_ERR.to_string());
        }
        if edit.strict {
            self.check_input_version(edit.version)?;
        }
        let plugin = edit
            .caller
            .owner()
            .map_or_else(|| Arc::from(UNKNOWN_WRITER), Arc::clone);
        let marks = edit.marks.map(|marks| (&*plugin, marks));
        let cursor = self.write_for(&edit.caller, |app| {
            app.input_box
                .apply_edits(edit.version, edit.edits, edit.cursor, edit.anchor, marks)
        })?;
        let version = self.input_box.buffer.version();
        self.input_changed(InputWriter::Plugin(plugin, version));
        Ok(serde_json::json!({ "version": version, "cursor": cursor }))
    }

    /// Places a plugin's marks with the same session guard as an edit. An
    /// input the user cannot see is fine here, since marks never reach the
    /// model. Answers the version the marks were mapped to.
    pub(crate) fn decorate_input(
        &mut self,
        req: InputDecorate,
    ) -> Result<serde_json::Value, String> {
        self.check_input_session(&req.session_id)?;
        let InputMarks { ns, marks } = req.marks;
        self.input_box
            .set_marks(req.version, &req.plugin, &ns, marks)?;
        Ok(serde_json::json!({ "version": self.input_box.buffer.version() }))
    }

    /// The drafts parked in other chats hold marks too, and would bring them
    /// back on the next switch.
    pub(crate) fn clear_plugin_marks(&mut self, plugin: &str) {
        self.input_box.clear_plugin_marks(plugin);
        for chat in &mut self.chats {
            chat.draft.clear_plugin_marks(plugin);
        }
    }

    /// Runs the same code as a typed Enter, so a plugin can never send
    /// something Enter would have handled another way. The `\` continuation
    /// is the one part left out, because the plugin asked to send.
    ///
    /// An edit gets moved past the typing since its version, but a send does
    /// not. That would send text the plugin never planned against and the user
    /// may not have read yet. An empty input still goes out while a login
    /// retry is pending, because Enter sends the retry then.
    ///
    /// The plugin gets no further than its own grants would take it: a
    /// command is held to what `maki.api.run_command` lets it run, and a send
    /// to [`Sending::refusal`]. The plugins that wrote the text are held to
    /// what an Enter holds them to, see [`written_refusal`]. A refused send
    /// leaves the input as it was.
    pub(crate) fn submit_input(
        &mut self,
        req: InputSubmit,
        area: Rect,
    ) -> Result<Vec<Action>, String> {
        self.check_input_session(&req.session_id)?;
        if !self.input_live(area) || !self.input_focused() {
            return Err(SUBMIT_NOT_LIVE_ERR.to_string());
        }
        self.check_input_version(req.version)?;
        let sight = Sight::default();
        let caller = Some(&req.caller);
        match self.dispatch_command_palette(KeyEvent::from(KeyCode::Enter), sight, caller) {
            PaletteKey::Handled(actions) => return Ok(actions),
            PaletteKey::Swallowed => return Err(NO_COMMAND_MATCHED_ERR.to_string()),
            PaletteKey::Refused(refusal) => return Err(refusal),
            PaletteKey::Passthrough => {}
        }
        let sending = self.input_sending();
        if matches!(sending, Sending::Nothing) {
            return Err(NOTHING_TO_SEND_ERR.to_string());
        }
        self.send_input(sending, sight, caller)
    }

    fn input_sending(&self) -> Sending {
        let input = &self.input_box;
        self.sending(sent_text(input.buffer.value()), input.has_images())
    }

    /// Sends the input the way Enter does. {sending} is what
    /// [`Self::input_sending`] read it as. It goes out only when {caller} may
    /// send it, see [`send_refusal`], and so may every plugin that wrote it,
    /// see [`written_refusal`]. A login retry sends none of the text, so it
    /// leaves the input as it was.
    fn send_input(
        &mut self,
        sending: Sending,
        sight: Sight,
        caller: Option<&PluginReach>,
    ) -> Result<Vec<Action>, String> {
        if !matches!(sending, Sending::Nothing) {
            let refusal = |plugin: &PluginReach| sending.refusal(plugin, &self.command_palette);
            if let Some(refusal) = send_refusal(sight, caller, refusal)
                .or_else(|| written_refusal(&self.input_box.buffer, sending.written_as()?, refusal))
            {
                return Err(refusal);
            }
        }
        let sub = if matches!(sending, Sending::AuthRetry) {
            Submission::default()
        } else {
            self.input_box.submit().unwrap_or_default()
        };
        Ok(self.handle_submit(sub))
    }

    fn check_input_version(&self, version: u64) -> Result<(), String> {
        let current = self.input_box.buffer.version();
        if version != current {
            return Err(format!(
                "{STALE_VERSION_ERR} {version} (it is now {current})"
            ));
        }
        Ok(())
    }

    fn check_input_session(&self, session_id: &str) -> Result<(), String> {
        let focused = self.state.session.id.to_string();
        if session_id != focused {
            return Err(format!(
                "input of session {session_id} is not focused (session {focused} is)"
            ));
        }
        Ok(())
    }

    /// The only path that can name a writer, see [`InputWriter`].
    fn input_changed(&mut self, writer: InputWriter) {
        self.input_writer = mem::take(&mut self.input_writer).merge(writer);
    }

    /// One event per frame at most, and only when the version, the caret or
    /// the selection changed, so holding a key down wakes a handler once and
    /// a frame that changed none of them leaves it asleep.
    ///
    /// A caret or a selection that moved on its own fires too, with
    /// `cursor_only` set. An input plugin anchored to what the caret is
    /// sitting in has no other way to learn it left: a popup on an `@`
    /// mention would stay up holding `<CR>` for a mention the user has
    /// arrowed out of.
    fn tick_input_changed(&mut self) -> Dirty {
        let now = self.input_at();
        self.input_fired_this_frame = now != self.announced_input;
        if !self.input_fired_this_frame {
            self.input_writer = InputWriter::Untouched;
            return Dirty::NO;
        }
        let source = mem::take(&mut self.input_writer).into_source(now.version);
        let cursor_only = now.version == self.announced_input.version;
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
        self.fire_input_changed(None, false);
    }

    fn fire_input_changed(&mut self, source: Option<Arc<str>>, cursor_only: bool) {
        let now = self.input_at();
        self.fire_session_event(HostEvent::InputChanged {
            text: self.input_box.buffer.value(),
            cursor: now.cursor,
            anchor: now.anchor,
            version: now.version,
            source: source.as_deref(),
            cursor_only,
        });
        self.announced_input = now;
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

    pub(crate) fn fire_session_event(&self, event: HostEvent<'_>) {
        self.lua_event_handle
            .fire(Some(&self.state.session.id), event);
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

    /// Dropping the keys waiting behind a handler is the first thing `Ctrl+C`
    /// does. When keys were dropped, that press does nothing else, since the
    /// input it would clear is not what the user typed yet, and the next one
    /// acts as usual.
    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        if let Msg::Key(key) = msg
            && key::QUIT.matches(key)
            && self.typeahead.abandon()
        {
            self.flash(HELD_KEYS_ABANDONED.to_owned());
            return vec![];
        }
        match self.typeahead.defer(msg) {
            Some((msg, sight, turn)) => self.handle_msg(msg, sight, &turn),
            None => vec![],
        }
    }

    fn handle_msg(&mut self, msg: Msg, sight: Sight, turn: &Turn) -> Vec<Action> {
        match msg {
            Msg::Key(key) => self.handle_key(key, sight, turn),
            Msg::Paste(text) => {
                if text.is_empty() {
                    if self.input_focused() && self.image_paste_rx.is_empty() {
                        self.start_image_paste();
                    }
                } else {
                    self.route_text_paste(&text, turn);
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

    fn send_to_agent(&self, subagent_id: Option<&str>, answer: String) {
        let idx = subagent_id
            .and_then(|id| self.chat_of_task(id).ok())
            .unwrap_or(MAIN_CHAT);
        if let Some(tx) = self.answer_channel(idx) {
            let _ = tx.try_send(answer);
        }
    }

    /// Who hears the user's answer to an ask from {idx}. A tool task without a
    /// channel of its own shares its turn's. A plugin task without one has
    /// nobody to ask, so its run fails rather than park, and its caller hears.
    /// A finished chat has no run left to answer, and the turn's channel is
    /// not its to borrow.
    fn answer_channel(&self, idx: usize) -> Option<&flume::Sender<String>> {
        let chat = &self.chats[idx];
        if chat.is_finished() {
            return None;
        }
        match (&chat.answer_tx, &chat.plugin) {
            (Some(tx), _) => Some(tx),
            (None, None) => self.answer_tx.as_ref(),
            (None, Some(_)) => None,
        }
    }

    /// The main turn waits on expired credentials: its own run or a
    /// subagent still on it. A task off the turn waits without holding it.
    fn turn_awaits_auth(&self) -> bool {
        self.chats
            .iter()
            .any(|chat| chat.auth_parked && chat.plugin.is_none())
    }

    /// One Enter retries every parked run: a lapsed login fails each of
    /// them, and one left out would wait forever.
    fn retry_auth(&mut self) {
        for idx in 0..self.chats.len() {
            if mem::take(&mut self.chats[idx].auth_parked)
                && let Some(tx) = self.answer_channel(idx)
            {
                let _ = tx.try_send(String::new());
            }
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
            // Copying lets go of the selection, so the next press clears the
            // input and the one after quits, just like without one. A press
            // right after a copy never copies, or a plugin that selects again
            // after every change could keep the user from leaving.
            if !mem::take(&mut self.quit_copied)
                && self.chat_accepts_input()
                && let Some(text) = self.input_box.take_selected_text()
            {
                self.copy(&text);
                self.quit_copied = true;
                return Some(vec![]);
            }
            return Some(if !self.chat_accepts_input() || self.input_box.is_empty() {
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
        if key::BACKGROUND.matches(key) {
            return Some(self.run_builtin(BuiltinAction::Background));
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
        None
    }

    /// [`Self::route_text_paste`] walks the same order, so a new overlay goes
    /// in both.
    fn dispatch_overlay(
        &mut self,
        key: KeyEvent,
        sight: Sight,
        turn: &Turn,
    ) -> Option<Vec<Action>> {
        // Drawn above everything else, so it answers keys before anything else.
        if self.alert_modal.is_open() {
            self.alert_modal.handle_key(key);
            return Some(vec![]);
        }

        // With both up the permission prompt goes first: a tool is blocked on
        // it and it owns the bottom panel. The pack review waits on nothing.
        // Ctrl+B passes it by, so the user hears why the parked call stays.
        if self.permission_prompt.is_open() {
            if key::BACKGROUND.matches(key) {
                return Some(self.run_builtin(BuiltinAction::Background));
            }
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

        // Step 2 of `handle_key`, among the overlays, see there for why.
        if self.dispatch_focused_window(&Pressed::Key(key, sight), turn) {
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

        if self.queue_focused() {
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
                RewindPickerAction::Fork(entry) => self.fork_to(entry),
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
                // Saved right here instead of going through an `Action`. A fold
                // only changes how the picker looks, and the state dir lives here.
                ModelPickerAction::Collapse(slug, folded) => {
                    maki_storage::model::persist_expanded_providers(
                        &self.storage,
                        self.model_picker.expanded_providers(),
                    );
                    let suffix = if folded {
                        PROVIDER_FOLDED_SUFFIX
                    } else {
                        PROVIDER_UNFOLDED_SUFFIX
                    };
                    self.flash(format!("{slug}{suffix}"));
                    vec![]
                }
                // The setup flow takes over: the picker closes so the
                // wizard has the screen.
                ModelPickerAction::Login(slug) => {
                    self.model_picker.close();
                    self.login_picker.open_for(self.storage.clone(), &slug);
                    vec![]
                }
                ModelPickerAction::Close => vec![],
            });
        }

        if self.login_picker.is_open() {
            return Some(match self.login_picker.handle_key(key) {
                LoginPickerAction::Consumed => vec![],
                // The wizard was opened from the model picker, so backing
                // out returns there instead of dropping into chat.
                LoginPickerAction::BackToPicker => {
                    let providers = provider_entries(&self.storage).0;
                    self.model_picker.open(&self.state.model.spec(), &providers);
                    vec![Action::RefreshModels]
                }
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
        match self.dispatch_command_palette(key, sight, None) {
            PaletteKey::Passthrough => None,
            PaletteKey::Swallowed => Some(vec![]),
            PaletteKey::Handled(actions) => Some(actions),
            PaletteKey::Refused(refusal) => {
                self.flash(refusal);
                Some(vec![])
            }
        }
    }

    /// Ctrl keys pass the palette by, as they always did: `Ctrl+C` closes it
    /// and the rest belong to the input box and the built-in bindings.
    ///
    /// The command an Enter would run is checked first, the way
    /// `maki.api.run_command` checks it, see [`send_refusal`], also for the
    /// plugins that wrote the text, see [`written_refusal`].
    fn dispatch_command_palette(
        &mut self,
        key: KeyEvent,
        sight: Sight,
        caller: Option<&PluginReach>,
    ) -> PaletteKey {
        if !self.chat_accepts_input() || is_ctrl(&key) {
            return PaletteKey::Passthrough;
        }
        if key.code == KeyCode::Enter
            && let Some(name) = self.command_palette.enter_target(&self.input_box.buffer)
        {
            let palette = &self.command_palette;
            let refusal = |plugin: &PluginReach| palette.resolve_for_plugin(&name, plugin).err();
            if let Some(refusal) = send_refusal(sight, caller, refusal)
                .or_else(|| written_refusal(&self.input_box.buffer, WRITTEN_COMMAND, refusal))
            {
                return PaletteKey::Refused(refusal);
            }
        }
        match self.command_palette.handle_key(key, &self.input_box.buffer) {
            CommandAction::Consumed => PaletteKey::Swallowed,
            CommandAction::Execute(cmd) => {
                // Commands act on the session, so the main chat comes to
                // front. Clear the box first, or the command would stay
                // behind as the subagent chat's draft.
                self.input_box.discard();
                self.set_active_chat(MAIN_CHAT);
                PaletteKey::Handled(self.execute_command(cmd, 0))
            }
            CommandAction::Complete(text) => {
                self.input_box.rewrite_input(&text);
                self.input_box.buffer.move_to_end();
                self.input_changed(InputWriter::Anyone);
                PaletteKey::Handled(vec![])
            }
            CommandAction::Passthrough => PaletteKey::Passthrough,
        }
    }

    fn plan_toggle_ready(&self) -> bool {
        self.state.mode == Mode::Plan && self.state.plan.is_ready()
    }

    /// [`Self::run_action`] for a default key, which has no one to answer.
    pub(crate) fn run_builtin(&mut self, action: BuiltinAction) -> Vec<Action> {
        self.run_action(action, None).1
    }

    /// Single implementation behind both the default keybindings and
    /// `maki.ui.action`, so a Lua rebind can never drift from the original
    /// key's behavior. The `bool` says whether the action did anything, so a
    /// handler can try something else when it did not.
    ///
    /// {caller} is the plugin that ran it, `None` for the user. The Lua side
    /// already checked `input` for an action that
    /// [changes the input](BuiltinAction::changes_input), and the plugin
    /// joins the writers of the text it changes, see [`written_refusal`]. A
    /// chat switch is not one of them: the draft it brings back carries its
    /// own writers.
    pub(crate) fn run_action(
        &mut self,
        action: BuiltinAction,
        caller: Option<&PluginReach>,
    ) -> (bool, Vec<Action>) {
        match caller {
            Some(caller) if action.changes_input() => {
                self.write_for(caller, |app| app.run_builtin_action(action))
            }
            _ => self.run_builtin_action(action),
        }
    }

    /// Runs {write} for {writer}, who joins the writers of the text when it
    /// changed.
    fn write_for<T>(&mut self, writer: &PluginReach, write: impl FnOnce(&mut Self) -> T) -> T {
        let before = self.input_box.buffer.version();
        let written = write(self);
        self.input_box.buffer.written_by(writer, before);
        written
    }

    fn run_builtin_action(&mut self, action: BuiltinAction) -> (bool, Vec<Action>) {
        let handled = match action {
            BuiltinAction::FilePicker => {
                self.file_picker.open(&self.state.session.cwd);
                true
            }
            BuiltinAction::Search => {
                let chat = &self.chats[self.active_chat];
                self.search_modal
                    .open(chat.scroll_pos(), chat.auto_scroll());
                true
            }
            BuiltinAction::Help => {
                self.toggle_prompt(self.help_modal.is_open(), |app| app.help_modal.toggle());
                true
            }
            BuiltinAction::PlanToggle => {
                let ready = self.plan_toggle_ready();
                if ready && self.plan_form.is_visible() {
                    self.plan_form.hide();
                } else if ready {
                    self.open_prompt(|app| app.plan_form.toggle());
                }
                ready
            }
            BuiltinAction::PlanEditor => {
                return match self.state.plan.path() {
                    Some(p) => (true, vec![Action::OpenEditor(p.to_path_buf())]),
                    None => {
                        self.flash(FLASH_NO_PLAN.into());
                        (false, vec![])
                    }
                };
            }
            BuiltinAction::EditInput => return (true, vec![Action::EditInputInEditor]),
            BuiltinAction::PopQueue => {
                self.pop_active_queue();
                true
            }
            BuiltinAction::PrevChat => {
                self.set_active_chat(self.active_chat.saturating_sub(1));
                true
            }
            BuiltinAction::NextChat => {
                self.set_active_chat((self.active_chat + 1).min(self.chats.len() - 1));
                true
            }
            BuiltinAction::ModelPicker => {
                let providers = provider_entries(&self.storage).0;
                self.model_picker.open(&self.state.model.spec(), &providers);
                return (true, vec![Action::RefreshModels]);
            }
            BuiltinAction::InputRaw => {
                self.input_box.buffer.toggle_raw();
                true
            }
            BuiltinAction::Undo => self.input_action(InputBox::undo),
            BuiltinAction::Redo => self.input_action(InputBox::redo),
            BuiltinAction::HistoryPrev => self.input_action(InputBox::history_up),
            BuiltinAction::HistoryNext => self.input_action(InputBox::history_down),
            BuiltinAction::Newline => self.input_action(|input| {
                input.newline();
                true
            }),
            BuiltinAction::Background => self.move_to_background(None),
        };
        (handled, vec![])
    }

    /// The input actions skip the mode check the keys make. The mode decides
    /// what a typed key does, and an action is a plugin asking on purpose: a
    /// vim plugin's `u` runs `undo` from normal mode.
    fn input_action(&mut self, edit: impl FnOnce(&mut InputBox) -> bool) -> bool {
        if !self.chat_accepts_input() {
            return false;
        }
        let changed = edit(&mut self.input_box);
        if changed {
            self.input_changed(InputWriter::Anyone);
        }
        changed
    }

    /// One handler per key, the first match in one order. Same list as the
    /// Key resolution docs, keep both in step.
    ///
    /// 1. `<C-c>` and `<C-z>`, which never wait behind a key handler, see
    ///    [`Self::update`]. `Ctrl+Z` suspends right here. `Ctrl+C` is not
    ///    answered here: no binding can take it and [`Self::reserved_by_host`]
    ///    skips the global bindings for it, but the modals and a focused
    ///    window still see it.
    /// 2. [`Self::dispatch_overlay`]: maki's modals and prompts, then the
    ///    focused window, [`Self::dispatch_focused_window`], then maki's
    ///    pickers, the search, the focused queue, the `/` command list and
    ///    `<C-t>` on a ready plan.
    /// 3. Windows on screen, front first, [`Self::dispatch_on_screen_windows`].
    /// 4. `<Esc>` while the agent runs, which [`Self::reserved_by_host`] keeps
    ///    from the global bindings, so it reaches the cancel in step 6.
    /// 5. Global bindings, [`Self::dispatch_binding`] without a buffer.
    /// 6. maki's built-in keys and the chat input, [`Self::handle_builtin_key`].
    ///
    /// A focused window is above the overlays opened under it because it is
    /// the window the user is in. An unfocused one is below them because it
    /// is not: the popup is up while the user works underneath, so a modal
    /// opened over it outranks it, and its keys come back the moment the
    /// modal closes. The command palette is one of those overlays, so a
    /// popup's `<Tab>` or `<CR>` leaves the `/` command being typed alone.
    ///
    /// Every step either answers the key or passes it on untouched, and no key
    /// is ever handed back after the fact, because a keystroke replayed into a
    /// UI that has moved on lands somewhere the user never aimed it. The one
    /// way back is `ev.fallback()`, which runs step 6, or the focused window's
    /// `recv` for its own buffer's binding, while the keys after it still wait.
    fn handle_key(&mut self, key: KeyEvent, sight: Sight, turn: &Turn) -> Vec<Action> {
        self.clear_selection_unless_pending_copy();
        if !key::QUIT.matches(key) {
            self.quit_copied = false;
        }

        if key::SUSPEND.matches(key) && cfg!(unix) {
            return vec![Action::Suspend];
        }

        if let Some(actions) = self.dispatch_overlay(key, sight, turn) {
            return actions;
        }

        let pressed = Pressed::Key(key, sight);

        if self.dispatch_on_screen_windows(&pressed, turn) {
            return vec![];
        }

        if !self.reserved_by_host(key) && self.dispatch_binding(None, &pressed, turn) {
            return vec![];
        }

        self.handle_builtin_key(key, sight, self.sync_input_mode())
    }

    /// Step 6, also what `ev.fallback()` runs for a key.
    fn handle_builtin_key(&mut self, key: KeyEvent, sight: Sight, mode: InputMode) -> Vec<Action> {
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
        if key::SCROLL_BOTTOM.matches(key) {
            self.active_chat().enable_auto_scroll();
            return vec![];
        }

        self.handle_chat_key(key, sight, mode)
    }

    /// Step 2: the focused window's buffer bindings, then its `recv`. False
    /// only when no window has focus, since a focused window takes everything.
    fn dispatch_focused_window(&mut self, pressed: &Pressed, turn: &Turn) -> bool {
        let Some(buf) = self.float_mgr.focused_buf() else {
            return false;
        };
        self.dispatch_binding(Some(&buf), pressed, turn) || self.send_to_focused_window(pressed)
    }

    /// A press no notation names is still spent on the window, see
    /// [`FloatManager::handle_focused_key`].
    fn send_to_focused_window(&self, pressed: &Pressed) -> bool {
        match pressed {
            Pressed::Key(key, _) => self.float_mgr.handle_focused_key(*key),
            Pressed::Paste(text) => self.float_mgr.handle_paste(text),
        }
    }

    /// Step 3. Only keys reach a window's `keys` claims: a paste goes to a
    /// window's `recv` only while it is focused.
    fn dispatch_on_screen_windows(&mut self, pressed: &Pressed, turn: &Turn) -> bool {
        let claim = match pressed {
            Pressed::Key(key, _) => Key::from_event(*key),
            Pressed::Paste(_) => None,
        };
        self.float_mgr.on_screen_bufs().iter().any(|(window, buf)| {
            self.dispatch_binding(Some(buf), pressed, turn)
                || claim.is_some_and(|key| self.float_mgr.take_claimed(*window, key))
        })
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
    /// other thing a user cannot be made to wait for. Only `Ctrl+C` and
    /// `Ctrl+Z` skip the keys waiting behind a key handler. `Esc` waits its
    /// turn like any key, because the window that takes it may be bound to
    /// start a handler of its own. A popup that put
    /// `Esc close` in its own footer is already past this: an unfocused window
    /// takes its keys in [`Self::dispatch_on_screen_windows`], which runs
    /// above, so the popup takes the first `Esc` and the next one, with the
    /// popup gone, arms the cancel.
    fn reserved_by_host(&self, key: KeyEvent) -> bool {
        is_reserved(key) || (self.status == Status::Streaming && key.code == KeyCode::Esc)
    }

    /// Whether a plugin binding in {buffer}'s scope, or a global one when it is
    /// `None`, claimed {pressed}. The binding the keymap matched travels with the
    /// request, so one dropped on the Lua thread cannot leave this having
    /// consumed a key nothing will act on.
    ///
    /// `false` is the only fall-through there is, and it is answered here, in
    /// the keystroke the user pressed: no binding matched or its load is gone.
    /// The next step then runs with the UI exactly as the user left it.
    ///
    /// A claimed key makes the keys after it wait for its handler, see
    /// [`Typeahead`]. The handler gets the input as it was at the press, so it
    /// works on the text the user pressed the key on, even if we stopped
    /// waiting and later keys landed first. The hold keeps {pressed} for
    /// `ev.fallback()`. Only a press that waited its [`Turn`] gets here, so
    /// none starts a second handler while one holds: the keys that skip the
    /// wait can never be bound.
    fn dispatch_binding(
        &mut self,
        buffer: Option<&Arc<SharedBuf>>,
        pressed: &Pressed,
        _turn: &Turn,
    ) -> bool {
        let Some(key) = pressed.key().filter(|key| !key.is_reserved()) else {
            return false;
        };
        debug_assert!(!self.typeahead.holding(), "a bound press got past the wait");
        let mode = self.sync_input_mode();
        let mut hold = None;
        let claimed = self.keymap_reader.dispatch(key, mode, buffer, |ticket| {
            let mut ticket = ticket.with_input(self.snapshot_input());
            if let Pressed::Paste(text) = pressed {
                ticket = ticket.with_text(sanitize(text));
            }
            let pending = Hold::new(
                &ticket,
                self.keyboard_focus(),
                mode,
                pressed.clone(),
                self.input_at(),
            );
            let sent = self.lua_event_handle.run_keybind_callback(ticket);
            hold = sent.then_some(pending);
            sent
        });
        if let Some(hold) = hold {
            self.typeahead.hold(hold);
        }
        claimed
    }

    /// Everything the handler of {ticket} asked of the UI arrived before this,
    /// so the keys typed after it can go now.
    pub(crate) fn keybind_done(&mut self, ticket: u64) -> Vec<Action> {
        if !self.typeahead.held_by(ticket) {
            return vec![];
        }
        // The handler's window changes come on their own channel, so they land
        // before its held keys follow the keyboard.
        let _ = self.float_mgr.drain_commands();
        self.release_typeahead()
    }

    /// `ev.fallback()` from the handler of {ticket}. While a window has focus
    /// only its own bindings run, so the fallback is its `recv` and not the
    /// chat input hidden behind it.
    ///
    /// The press is only still around while it holds the keys typed after it
    /// and neither the keyboard nor the caret has moved. Any later and the
    /// key would land behind those keys, or somewhere the user never aimed
    /// it, like a `!` put in front of the draft.
    pub(crate) fn keybind_fallback(&mut self, ticket: u64) -> Vec<Action> {
        // The handler's window changes come on their own channel, so they land
        // before the focus is read.
        let _ = self.float_mgr.drain_commands();
        let focus = self.keyboard_focus();
        let input = self.input_at();
        let mode = self.sync_input_mode();
        let Some(pressed) = self.typeahead.take_fallback(ticket, focus, mode, &input) else {
            tracing::debug!(
                ticket,
                "ev.fallback() dropped: its key no longer holds the keys after it, or the keyboard, the mode, the cursor or the selection moved"
            );
            return vec![];
        };
        if self.send_to_focused_window(&pressed) {
            return vec![];
        }
        let actions = match pressed {
            Pressed::Key(key, sight) => self.handle_builtin_key(key, sight, mode),
            Pressed::Paste(text) => {
                self.builtin_paste(&text);
                vec![]
            }
        };
        self.typeahead.fell_back(ticket, &input, self.input_at());
        actions
    }

    fn input_at(&self) -> InputAt {
        let buffer = &self.input_box.buffer;
        InputAt {
            version: buffer.version(),
            cursor: buffer.cursor_byte(),
            anchor: buffer.anchor(),
        }
    }

    fn release_typeahead(&mut self) -> Vec<Action> {
        let input = self.input_at();
        if self.typeahead.release(&input, self.keyboard_focus()) {
            self.flash(HELD_KEYS_DROPPED.to_owned());
        }
        self.replay_typeahead()
    }

    /// Replays the held keys up to the first one that hands the event loop
    /// actions, and returns those. An action can move the keyboard, like
    /// `/fork` to the new tab, so the event loop runs them before it asks for
    /// the rest, and the keys after them land where the user typed them.
    pub(crate) fn replay_typeahead(&mut self) -> Vec<Action> {
        while let Some((msg, sight, turn)) = self.typeahead.next() {
            let actions = self.handle_msg(msg, sight, &turn);
            if !actions.is_empty() {
                return actions;
            }
        }
        vec![]
    }

    /// The keys held for a key handler, for the tab the keyboard moves to.
    /// Replaying them here would send them, Enter and all, to a tab the user
    /// already left. They keep waiting there for the same handler, which is
    /// usually still at work on that tab after switching to it. Keys no
    /// handler holds any more replay there, see [`Self::replay_typeahead`].
    pub(crate) fn hand_typeahead_to(&mut self, to: &mut App) {
        // A modal or a prompt already open on that tab is not where these
        // keys were typed, so it must not take them.
        let focus = KeyboardFocus {
            prompts: None,
            ..to.keyboard_focus()
        };
        let input = to.input_at();
        to.typeahead
            .adopt(mem::take(&mut self.typeahead), focus, input);
    }

    fn tick_typeahead(&mut self) -> Dirty {
        if !self.typeahead.stale(|| self.keyboard_focus()) {
            return Dirty::NO;
        }
        let actions = self.release_typeahead();
        self.pending_actions.extend(actions);
        Dirty::YES
    }

    /// A message typed while watching a running subagent is for that
    /// subagent, see [`Self::queue_for_subagent`]. `Esc` follows the chat in front
    /// too: armed twice in a running subagent's chat it cancels that subagent
    /// alone. A finished subagent's chat has no input, so only the mode
    /// toggle answers there.
    ///
    /// While a turn runs, `Esc` drops a selection and arms the cancel in the
    /// same press, for the reason [`Self::reserved_by_host`] gives: a plugin
    /// that keeps setting a selection must not be able to keep the user from
    /// stopping. Idle, it only drops the selection, so it never arms the
    /// rewind.
    ///
    /// {mode} is the one the key was pressed in. A fallback must not type in
    /// insert mode a key a plugin matched in normal mode.
    fn handle_chat_key(&mut self, key: KeyEvent, sight: Sight, mode: InputMode) -> Vec<Action> {
        if !self.chat_accepts_input() {
            return match key.code {
                KeyCode::Tab if !self.is_bash_input() => self.toggle_mode(),
                KeyCode::Esc => {
                    self.last_esc = None;
                    self.set_active_chat(MAIN_CHAT);
                    vec![]
                }
                // A finished tab has no input box. Typing there used to do
                // nothing at all, so the keys go to the main chat instead.
                // Without an input box the mode always reads normal, so the
                // real one is read again once the main chat has focus.
                KeyCode::Char(_)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.last_esc = None;
                    self.set_active_chat(MAIN_CHAT);
                    let mode = self.sync_input_mode();
                    self.handle_chat_key(key, sight, mode)
                }
                _ => vec![],
            };
        }
        if key::EDIT_INPUT.matches(key) {
            return self.run_builtin(BuiltinAction::EditInput);
        }
        if key::MODEL_PICKER.matches(key) {
            return self.run_builtin(BuiltinAction::ModelPicker);
        }
        if key::UNDO.matches(key) {
            if mode == InputMode::Insert && self.input_box.undo() {
                self.input_changed(InputWriter::Anyone);
            }
            return vec![];
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
            } else if let InputAction::Changed = self.input_box.handle_key(key, mode) {
                self.input_changed(InputWriter::Anyone);
            }
            return vec![];
        }

        let streaming = self.status == Status::Streaming;
        if streaming && key.code == KeyCode::Esc {
            self.input_box.buffer.deselect();
            return self.handle_esc(streaming);
        }
        match self.input_box.handle_key(key, mode) {
            InputAction::Submit => self
                .send_input(self.input_sending(), sight, None)
                .unwrap_or_else(|refusal| {
                    self.flash(refusal);
                    vec![]
                }),
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
                    KeyCode::Esc => self.handle_esc(streaming),
                    _ => vec![],
                }
            }
            InputAction::ContinueLine | InputAction::None => vec![],
        }
    }

    fn handle_esc(&mut self, streaming: bool) -> Vec<Action> {
        let in_subagent = !self.is_main_chat();
        let armed = self
            .last_esc
            .take()
            .is_some_and(|t| t.elapsed() < self.status_bar.flash_duration);
        if armed {
            return if in_subagent {
                self.handle_subagent_cancel()
            } else if streaming {
                self.handle_cancel()
            } else {
                self.open_rewind_picker()
            };
        }
        self.last_esc = Some(Instant::now());
        let hint = if in_subagent || streaming {
            FLASH_CANCEL
        } else {
            FLASH_REWIND
        };
        // Esc never reaches background work, which a flash about stopping
        // would otherwise suggest.
        let running = self.running_background_tasks().count();
        let flash = if in_subagent || running == 0 {
            hint.to_owned()
        } else {
            let still = count_phrase(running, ESC_ONE_RUNNING, ESC_MANY_RUNNING_SUFFIX);
            format!("{hint} {still}")
        };
        self.status_bar.flash(flash);
        vec![]
    }

    /// Background tasks of every open session. They all run in the one Lua
    /// host, which a quit or a reload ends.
    fn background_everywhere(&self) -> usize {
        self.running_background_tasks().count() + self.background_elsewhere
    }

    /// Flashes how many background tasks are {running}, if any, and says
    /// whether it did, so the caller holds off.
    fn warn_if_running(&mut self, running: usize, one: &str, many_suffix: &str) -> bool {
        if running == 0 {
            return false;
        }
        self.flash(count_phrase(running, one, many_suffix));
        true
    }

    /// With background tasks running, quitting takes a second press, the way
    /// Esc does.
    fn quit(&mut self) -> Vec<Action> {
        let running = self.background_everywhere();
        let armed = self
            .last_quit
            .take()
            .is_some_and(|t| t.elapsed() < self.status_bar.flash_duration);
        if !armed && self.warn_if_running(running, QUIT_ONE_RUNNING, QUIT_MANY_RUNNING_SUFFIX) {
            self.last_quit = Some(Instant::now());
            return vec![];
        }
        self.quit_with(ExitRequest::Success)
    }

    /// A press is easy to repeat, a typed command is not, so with background
    /// tasks running a reload takes the bang.
    fn reload(&mut self, force: bool) -> Vec<Action> {
        let running = self.background_everywhere();
        if !force && self.warn_if_running(running, RELOAD_ONE_RUNNING, RELOAD_MANY_RUNNING_SUFFIX) {
            return vec![];
        }
        self.quit_with(ExitRequest::Reload)
    }

    /// `/new` ends this session, and its background tasks with it. Only the
    /// tab's own count: other tabs keep running.
    fn start_new_session(&mut self, force: bool) -> Vec<Action> {
        let running = self.running_background_tasks().count();
        if !force && self.warn_if_running(running, NEW_ONE_RUNNING, NEW_MANY_RUNNING_SUFFIX) {
            return vec![];
        }
        self.reset_session()
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

    /// An exit the user asks for goes now, never held by
    /// [`Self::awaits_background`].
    fn quit_with(&mut self, req: ExitRequest) -> Vec<Action> {
        self.save_input_history();
        self.exit_request = req;
        self.exit_on_done = false;
        vec![Action::ManualExit]
    }

    /// `--exit-on-done` holds a successful exit while work a run moved to the
    /// background is out: its result wakes a run, and that run's drain exits.
    /// A failed turn exits with its error. Once the deadline lapses the exit
    /// goes, the work stops with the process and the meta keeps its receipts.
    pub(crate) fn awaits_background(&self, now: Instant) -> bool {
        self.exit_on_done
            && self.exit_request == ExitRequest::Success
            && self
                .background_deadline
                .is_some_and(|deadline| now < deadline)
            && self
                .live_mailbox()
                .is_some_and(|mailbox| mailbox.outstanding() > 0)
    }

    /// A woken run would clear the error a failed `--exit-on-done` turn
    /// exits with, so such a session takes none.
    pub(crate) fn takes_mailbox_runs(&self) -> bool {
        self.exit_request != ExitRequest::Error
    }

    pub(crate) fn clear_exit_request(&mut self) {
        self.exit_request = ExitRequest::None;
    }

    fn sending(&self, text: &str, has_images: bool) -> Sending {
        if self.turn_awaits_auth() {
            return Sending::AuthRetry;
        }
        Sending::of(text, has_images)
    }

    pub(crate) fn handle_submit(&mut self, sub: Submission) -> Vec<Action> {
        let sending = self.sending(&sub.text, !sub.images.is_empty());
        self.retry_auth();
        match sending {
            Sending::AuthRetry | Sending::Nothing => vec![],
            Sending::Exit => self.quit(),
            Sending::Shell(prefix) => self.run_shell_line(prefix),
            Sending::Message if self.is_main_chat() => self.submit_or_queue(sub.into()),
            Sending::Message => self.queue_for_subagent(sub.into()),
        }
    }

    fn run_shell_line(&mut self, prefix: ShellPrefix) -> Vec<Action> {
        // The shell belongs to the session, not to the subagent being watched,
        // so the main chat comes to front where the output lands.
        self.set_active_chat(MAIN_CHAT);
        let cmd = prefix.command.trim();
        if cmd == "cd" || cmd.starts_with("cd ") {
            self.flash("Only /cd can change the working directory".into());
        }
        let id = self.shell.reserve_id();
        let sigil = if prefix.visible { "!" } else { "!!" };
        let display = format!("{sigil} {}", prefix.command);
        self.main_chat().show_user_message(display, Vec::new());
        vec![Action::ShellCommand {
            id,
            command: prefix.command,
            visible: prefix.visible,
        }]
    }

    fn handle_cancel(&mut self) -> Vec<Action> {
        let cancelled_run = self.run_id;
        self.run_id += 1;
        self.retry_info = None;
        self.close_all_overlays();
        self.finish_subagents(TaskOutcome::Cancelled, CANCELLED_TEXT);
        self.chats[MAIN_CHAT].auth_parked = false;
        self.shell.cancel_all();
        for chat in self.chats.iter_mut().filter(|c| c.plugin.is_none()) {
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
        let id = self.active_task_id();
        self.stop_task(&id).into_iter().collect()
    }

    fn bill(&mut self, chat_idx: usize, model: &str, usage: StoredTokenUsage) {
        self.state.token_usage += TokenUsage::from(usage);
        add_cost(&mut self.state.cost, usage.cost);
        add_cost(&mut self.chats[chat_idx].cost, usage.cost);
        add_cost(
            &mut self.state.subsidised_list_cost,
            usage.subsidised_list_cost,
        );
        add_cost(
            &mut self.chats[chat_idx].list_cost,
            usage.subsidised_list_cost,
        );
        self.state.session_mut().add_model_usage(model, usage);
        // A task's cost outlives it on disk, and nothing else writes the meta
        // between a plugin task's turns.
        if chat_idx != MAIN_CHAT {
            self.sync_subagents();
        }
    }

    /// Run-scoped delivery for the tab's own agent: restore snapshots fan out
    /// to every chat, stale runs are dropped, the rest goes to [`Self::deliver`].
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
        // Taken before the run check, since a moved call keeps reporting
        // after its turn ended.
        if let AgentEvent::ToolParts { id, parts } = envelope.event {
            self.set_parts(id, parts);
            return vec![];
        }
        // What an adopted subagent sent before its call moved belongs to its
        // task, not to the run, and may land after the next run started.
        if let Some(plugin) = task_id_of(&envelope).and_then(|id| self.task_plugin(id)) {
            return self.deliver(envelope, Some(plugin));
        }
        if envelope.run_id != self.run_id && !self.is_racing_receipt(&envelope) {
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
        self.deliver(envelope, None)
    }

    /// A move racing Esc lands after the run it moved out of. Its task lives
    /// on, so the row it moved, while still on screen, needs its tab. Only a
    /// main chat call: Esc ends the subagents with the run, and a call moved
    /// out of one has no agent left to report to.
    fn is_racing_receipt(&self, envelope: &Envelope) -> bool {
        envelope.subagent.is_none()
            && matches!(&envelope.event, AgentEvent::ToolDone(done)
                if done.backgrounded && self.chats[MAIN_CHAT].tool_message(&done.id).is_some())
    }

    /// `plugin` names the plugin whose session sent the event, which the chat
    /// shows as a task of that plugin. Those events skip the run gate above:
    /// the session outlives turns, Esc and rewinds, so the only gate is the
    /// chat session, which the event loop already matched.
    pub(crate) fn deliver(&mut self, envelope: Envelope, plugin: Option<Arc<str>>) -> Vec<Action> {
        if task_id_of(&envelope).is_some_and(|id| self.deleted_tasks.contains(id)) {
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
            // to the ToolDone that follows elsewhere. A plugin task is never in
            // `chat_index`, and rightly stays open here: it writes its
            // transcript after every prompt, and its close is `StreamClosed`.
            if let Some(&sub_idx) = self.chat_index.get(tool_use_id.as_str()) {
                self.chats[sub_idx].mark_finished(TaskOutcome::Unknown, DONE_TEXT);
            }
            self.state
                .session_mut()
                .set_subagent_messages(tool_use_id, messages);
            return vec![];
        }

        // A moved call's final result names the call, and the envelope names
        // no task or that same one. It ends the task and never reaches
        // history: the receipt is what the model got for that call. A
        // `ToolDone` inside another task's session is that session's own row.
        if plugin.is_some()
            && let AgentEvent::ToolDone(done) = &envelope.event
            && envelope
                .subagent
                .as_ref()
                .is_none_or(|s| s.parent_tool_use_id == done.id)
        {
            match self.chat_of_task(&done.id) {
                Ok(idx) if idx != MAIN_CHAT && self.chats[idx].plugin.is_some() => {
                    self.finish_background_call(idx, envelope.event);
                    return vec![];
                }
                // A subagent's tab is the turn's until the receipt adopts it,
                // and the turn may have ended it by then. The result waits so
                // the adoption can tell, see `App::adopt_subagent`.
                Ok(idx) if idx != MAIN_CHAT => {
                    self.early_finals.insert(done.id.clone(), envelope.event);
                    return vec![];
                }
                // A deleted tab's receipt never comes back to claim it.
                Err(_) if envelope.subagent.is_none() => {
                    if !self.deleted_tasks.contains(&done.id) {
                        self.early_finals.insert(done.id.clone(), envelope.event);
                    }
                    return vec![];
                }
                _ => {}
            }
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

        let is_plugin_task = plugin.is_some();
        let chat_idx = match envelope.subagent {
            Some(ref subagent) => self.resolve_or_create_chat(subagent, &plugin),
            // A plugin event without its task must never land in the main chat.
            None if is_plugin_task => return vec![],
            None => MAIN_CHAT,
        };
        if is_plugin_task && matches!(envelope.event, AgentEvent::StreamClosed) {
            // The session is gone. If Esc already ended the task, its `Error`
            // stays. A close mid-prompt leaves tools without a result, and
            // nothing after this would settle them. The close knows no
            // verdict, so the final result of an adopted subagent's call can
            // still refine it, and its badge.
            let chat = &mut self.chats[chat_idx];
            chat.cancel_in_progress();
            if !chat.is_finished() {
                chat.mark_finished(TaskOutcome::Unknown, DONE_TEXT);
                if let Some(id) = &subagent_id {
                    self.set_badge_outcome(id, BackgroundOutcome::Done);
                }
            }
            return vec![];
        }
        self.chats[chat_idx].track_run(&envelope.event);

        if let AgentEvent::ToolDone(ref e) = envelope.event {
            if self.state.mode == Mode::Plan
                && self.state.plan.path().is_some_and(|pp| e.wrote_to(pp))
            {
                self.transition_plan(PlanTrigger::WriteDone);
            }
            self.state
                .session_mut()
                .insert_tool_output(e.id.clone(), e.output.clone());
            // A receipt does not end the subagent behind it: the call goes
            // on, and adopting its chat takes it off the turn first.
            if e.backgrounded {
                self.open_background_call(chat_idx, e);
            } else if let Some(&sub_idx) = self.chat_index.get(&e.id) {
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

        // A plugin task streams while the main chat may be mid-retry.
        if chat_idx == MAIN_CHAT {
            self.retry_info = None;
        }

        if let AgentEvent::TurnComplete(ref tc) = envelope.event {
            self.bill(
                chat_idx,
                &tc.model,
                tc.usage
                    .billed_with_subsidised_list_cost(tc.cost, tc.subsidised_list_cost),
            );
            let ctx_size = tc.context_size.unwrap_or_else(|| tc.usage.context_tokens());
            self.set_context_size(chat_idx, ctx_size);
            self.chats[chat_idx].set_pending_turn_usage(tc.usage.format(tc.cost));
            if let Some(tool_id) = &subagent_id {
                let formatted = tc.usage.format_sum_cost(self.chats[chat_idx].cost);
                self.chats[MAIN_CHAT].set_tool_turn_usage(tool_id, formatted);
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
            } else {
                self.chats[chat_idx].show_user_message(text, images);
            }
            return vec![];
        }

        if let ChatEventResult::PermissionRequest {
            id,
            tool,
            scopes,
            reason,
        } = result
        {
            let project_trusted = self.permissions.project_is_trusted();
            self.open_prompt(|app| {
                app.permission_prompt.push(
                    id,
                    tool,
                    scopes,
                    subagent_id.clone(),
                    project_trusted,
                    reason,
                );
            });
            return vec![];
        }

        if let ChatEventResult::AuthRequired = result {
            self.chats[chat_idx].push(DisplayMessage::new(
                DisplayRole::Error,
                AUTH_EXPIRED_MSG.into(),
            ));
            if self.answer_channel(chat_idx).is_none() {
                return vec![];
            }
            if chat_idx != MAIN_CHAT {
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Error,
                    AUTH_EXPIRED_MSG.into(),
                ));
            }
            let park = |app: &mut Self| app.chats[chat_idx].auth_parked = true;
            if self.chats[chat_idx].plugin.is_none() {
                self.open_prompt(park);
            } else {
                park(self);
            }
            return vec![];
        }

        if is_plugin_task {
            // Either way the prompt has returned, and the session waits for
            // the next one. A run its Lua task's end cut short mid-tool has no
            // `handle_cancel` to settle the tool, so it is settled here.
            match result {
                ChatEventResult::Done => self.chats[chat_idx].cancel_in_progress(),
                ChatEventResult::Error(message) => {
                    self.chats[chat_idx].cancel_in_progress();
                    // Esc in the tab already wrote the task's ending, and the
                    // error its cut short run sends after that would say it twice.
                    if !self.chats[chat_idx].is_finished() {
                        self.chats[chat_idx].push(DisplayMessage::new(
                            DisplayRole::Error,
                            cap_error_text(&message),
                        ));
                    }
                }
                _ => {}
            }
            return vec![];
        }

        if chat_idx == 0 {
            match result {
                ChatEventResult::Done => {
                    self.status_bar.clear_flash();
                    self.terminalize_turn(MISSING_TOOL_COMPLETION);
                    self.chat_index.clear();
                    self.status = Status::Idle;
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Success;
                        self.background_deadline = self
                            .background_deadline
                            .or_else(|| self.background_wait.map(|wait| Instant::now() + wait));
                    }
                }
                ChatEventResult::Error(message) => {
                    self.status = Status::error(message.clone());
                    self.status_bar.clear_flash();
                    self.main_chat().push(DisplayMessage::new(
                        DisplayRole::Error,
                        cap_error_text(&message),
                    ));
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

    /// A tool task is found through `chat_index`, a plugin task by id: the
    /// index is wiped every turn, and a plugin task would come back as a
    /// second chat at the main chat's next turn end. A plugin event naming a
    /// subagent's chat adopts it, see [`Self::adopt_subagent`].
    fn resolve_or_create_chat(
        &mut self,
        subagent: &SubagentInfo,
        plugin: &Option<Arc<str>>,
    ) -> usize {
        let id = &subagent.parent_tool_use_id;
        let known = match plugin {
            None => self.chat_index.get(id.as_str()).copied(),
            Some(_) => self.chat_of_task(id).ok(),
        };
        if let Some(idx) = known {
            if let Some(plugin) = plugin {
                self.adopt_subagent(idx, plugin);
            }
            fill_missing_subagent_info(&mut self.chats[idx], subagent);
            return idx;
        }
        let idx = self.chats.len();
        if plugin.is_none() {
            self.chat_index.insert(id.clone(), idx);
            self.chats[MAIN_CHAT].update_tool_summary(id, &subagent.name);
            if let Some(ref model) = subagent.model {
                self.chats[MAIN_CHAT].update_tool_model(id, model);
            }
        }
        let mut chat = Chat::new(
            self.state.session.id,
            Some(id),
            subagent.name.clone(),
            self.ui_config.clone(),
            self.lua_event_handle.clone(),
        );
        chat.set_restore_channel(self.restore_event_tx.clone());
        chat.plugin = plugin.clone();
        chat.clock = Some(TaskClock::start());
        chat.model_id = subagent.model.clone();
        chat.opts = subagent.opts;
        chat.inbox = subagent.inbox.clone();
        chat.answer_tx = subagent.answer_tx.clone();
        chat.parent = started_by(subagent);
        if let Some(ref prompt) = subagent.prompt {
            chat.push_user_message(prompt);
        }
        self.chats.push(chat);
        self.sync_subagents();
        idx
    }

    /// A chat this task already has, maybe one the user just stopped, stays as
    /// it is rather than turning idle again. A subagent's chat is adopted,
    /// which is how a session nested under a moved call leaves the turn.
    pub(crate) fn open_plugin_task(&mut self, info: &SubagentInfo, plugin: Arc<str>) {
        let id = &info.parent_tool_use_id;
        if self.deleted_tasks.contains(id) {
            return;
        }
        let known = self.chat_of_task(id).is_ok();
        let idx = self.resolve_or_create_chat(info, &Some(plugin));
        if !known {
            self.chats[idx].idle = true;
        }
    }

    /// Entry point for `maki.api.run_command`: splits a command line into the
    /// name and args the input bar would hand over, leading slash optional.
    /// `Err` means nothing ran at all, so the Lua caller can say why.
    pub(crate) fn run_cmdline(
        &mut self,
        cmdline: &str,
        depth: u8,
        caller: &PluginReach,
    ) -> Result<Vec<Action>, String> {
        if depth > MAX_COMMAND_DEPTH {
            return Err(COMMAND_DEPTH_MSG.to_string());
        }
        let trimmed = cmdline.trim();
        let (name, args) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        let resolved = self
            .command_palette
            .resolve_for_plugin(&format!("/{}", name.trim_start_matches('/')), caller)?;
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
                self.toggle_prompt(self.help_modal.is_open(), |app| app.help_modal.toggle());
                vec![]
            }
            "/usage" => {
                self.toggle_prompt(self.usage_modal.is_open(), |app| app.usage_modal.toggle());
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
            "/new" => self.start_new_session(cmd.bang),
            "/fork" => self.fork_session(),
            "/queue" => {
                self.queue.set_focus();
                vec![]
            }
            "/model" => {
                let providers = provider_entries(&self.storage).0;
                self.model_picker.open(&self.state.model.spec(), &providers);
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
            EXIT_COMMAND => self.quit(),
            "/reload" => self.reload(cmd.bang),
            // Typing `/trust` is the consent, exactly like `maki trust add
            // --yes`, so there is no second question to ask here.
            "/trust" => self.trust_folder(),
            name @ ("/packupdate" | "/packdel") => {
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
                self.open_prompt(|app| app.pack_review.open(prompt, plan));
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
        self.save_input_history();
        match env::set_current_dir(&path) {
            Ok(()) => {
                if let Ok(canonical) = env::current_dir() {
                    let max_entries = self.input_box.history().max_entries();
                    self.input_box.set_history(InputHistory::load(
                        &self.storage,
                        &canonical,
                        max_entries,
                    ));
                    self.state
                        .session_mut()
                        .set_cwd(canonical.to_string_lossy().into_owned());
                    self.fire_session_event(HostEvent::SessionCwdChanged {
                        cwd: &canonical.to_string_lossy(),
                    });
                }
                self.status_bar.refresh_cwd();
                self.flash(format!("cd {}", path.display()))
            }
            Err(e) => self.flash(format!("cd: {e}")),
        }
        vec![]
    }

    fn overlays(&self) -> [&dyn Overlay; 14] {
        [
            &self.alert_modal,
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

    fn overlays_mut(&mut self) -> [&mut dyn Overlay; 14] {
        [
            &mut self.alert_modal,
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
        self.permission_prompt.is_open() || self.turn_awaits_auth() || self.float_mgr.needs_input()
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
        if self.turn_awaits_auth() {
            return Some(Notification::AuthenticationRequired);
        }
        if self.status != Status::Streaming && self.plan_form_open() {
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
            | self.tick_typeahead()
            | self.tick_edge_scroll()
            | self.tick_error_expiry()
            | self.poll_image_paste()
            | self.poll_primary_paste()
            | self.btw_modal.poll()
            | self.status_bar.poll_branch_update()
            | self.status_bar.clear_expired_hint()
            | self.mcp_picker.refresh()
            | self.model_picker.refresh()
            | self.usage_modal.poll(&self.usage_slot)
            | self.hints.poll(self.hint_reader.load_full())
            | self.poll_input_rules()
            | Dirty::from(self.input_box.poll_decor())
            | self.tick_plan()
            | self.tick_file_picker()
            | self.tick_input_changed()
            | self.tick_background_moves()
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
        self.open_plan_form(menu);
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
            self.open_plan_form(builtin_menu());
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

    fn open_plan_form(&mut self, menu: PlanMenu) {
        self.open_prompt(|app| app.plan_form.open_with(menu));
    }

    pub(crate) fn open_alert(&mut self, message: String) {
        self.open_prompt(|app| app.alert_modal.open(message));
    }

    /// Every prompt opens through here. A prompt closed and opened again, or
    /// one replacing another of its kind, leaves the same ones up, and only
    /// the count tells the held keys it is not the prompt they were typed at,
    /// see [`Prompts`].
    fn open_prompt(&mut self, open: impl FnOnce(&mut Self)) {
        open(self);
        self.prompts_opened += 1;
    }

    fn toggle_prompt(&mut self, is_open: bool, toggle: impl FnOnce(&mut Self)) {
        if is_open {
            toggle(self);
        } else {
            self.open_prompt(toggle);
        }
    }

    /// What a tab nobody is looking at still owes the frame. Its floats have
    /// to drain, or a plugin writing to a window off screen would lose the
    /// output, and its plan form too, or a draft in a background tab would
    /// sit unanswered until the user focused it. So do the replies to a
    /// Ctrl+B pressed just before the user switched away. Nothing it holds
    /// was announced this frame, because it never diffed its input, so the
    /// announcement has to speak when this tab takes focus. It holds no keys:
    /// those left with the focus, see [`Self::hand_typeahead_to`].
    pub fn tick_background(&mut self) {
        self.input_fired_this_frame = false;
        let _ = self.float_mgr.tick();
        let _ = self.tick_plan();
        let _ = self.poll_input_rules();
        let _ = self.input_box.poll_decor();
        let _ = self.tick_background_moves();
    }

    /// Rules hold for every session, so a background tab takes them up too
    /// and comes back to the front already matching them.
    fn poll_input_rules(&mut self) -> Dirty {
        let plugin_rules = self.input_rules_reader.load_full();
        let dirty = self.input_rules.poll(Arc::clone(&plugin_rules));
        if dirty == Dirty::YES {
            self.input_box
                .buffer
                .set_rules(RuleSet::with_plugins(&plugin_rules));
        }
        dirty
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
            // Only the chat on screen draws motion. The rest only take data,
            // which `tick` polls in every chat.
            self.chats[self.active_chat].cadence(),
            // A Ctrl+B reply wakes nothing, so the loop polls until it is in.
            Cadence::when(!self.background_moves.pending.is_empty(), Cadence::PENDING),
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
        self.chats[MAIN_CHAT].fail_in_progress_except(message.into(), self.shell.active_ids());
        for chat in self.chats.iter_mut().skip(1).filter(|c| c.plugin.is_none()) {
            chat.fail_in_progress_with_message(message.into());
        }
    }

    /// Marks unfinished subagent chats as ended and drops them from
    /// `chat_index`, then mirrors the tabs into the session meta.
    fn retain_resolved_subagents(&mut self, outcome: TaskOutcome, text: &str) {
        self.chat_index.retain(|_, &mut sub_idx| {
            if self.chats[sub_idx].is_finished() {
                true
            } else {
                self.chats[sub_idx].mark_finished(outcome, text);
                false
            }
        });
        self.forget_untasked_parts();
        self.sync_subagents();
    }

    pub fn flush_all_chats(&mut self) {
        for chat in &mut self.chats {
            chat.flush();
        }
    }

    /// A paste walks the steps of [`Self::handle_key`] as the `<Paste>` key.
    /// The host's prompts and pickers have no bindings, they just take the
    /// text. Every route gets `\n` line endings. A binding's `ev.text` is also
    /// cleaned the way the input cleans what lands in it, so a `<Paste>`
    /// handler counting offsets over it counts what inserting it adds, while
    /// a prompt, a picker or a plugin window's `recv` keeps its tabs.
    ///
    /// The first step that owns the keyboard owns the paste as well, and one
    /// with no field for it drops it. The chat input and the bindings under a
    /// modal get none of its keys, so they get none of its pastes either.
    fn route_text_paste(&mut self, text: &str, turn: &Turn) {
        let text = &normalize_line_endings(text);
        if self.alert_modal.is_open() || self.permission_prompt.handle_paste(text) {
            return;
        }
        if self.pack_review.is_open()
            || self.plan_form_active()
            || self.help_modal.is_open()
            || self.usage_modal.is_open()
            || self.btw_modal.is_open()
        {
            return;
        }
        let pressed = Pressed::Paste(text.to_owned());
        if self.dispatch_focused_window(&pressed, turn) {
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
        if self.queue_focused() {
            return;
        }
        try_picker!(self.rewind_picker);
        try_picker!(self.theme_picker);
        try_picker!(self.model_picker);
        try_picker!(self.login_picker);
        try_picker!(self.mcp_picker);
        if self.dispatch_on_screen_windows(&pressed, turn)
            || self.dispatch_binding(None, &pressed, turn)
        {
            return;
        }
        self.builtin_paste(text);
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
