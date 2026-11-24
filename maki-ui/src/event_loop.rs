//! Multi-session supervisor: every session owns an `App` + `AgentHandles` and
//! keeps draining agent events while backgrounded; only the focused session
//! renders and receives input. `SpawnCtx` carries the shared resources needed
//! to spawn session runtimes at any point.
//!
//! The loop is async and selects over every event source at once (terminal
//! input through [`InputReader`], plugin actions, agent events, timers), so
//! it wakes the moment any of them has something and never blocks a worker:
//! it runs under `block_on` on the main thread.

use std::collections::{BTreeMap, VecDeque};
use std::future::{pending, ready};
use std::mem;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};

use crossterm::event::{
    Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use futures::future::{LocalBoxFuture, select_all};
use maki_agent::agent::SessionExport;
use maki_agent::command::CustomCommand;
use maki_agent::permissions::PermissionManager;
use maki_agent::session::Resumed;
use maki_agent::{
    AgentConfig, AgentEvent, CancelToken, CompactReason, Envelope, HostPrompt, McpCommand,
    McpConfigErrors, McpHandle, SessionMailbox, Transcript, UiWaker, mcp,
};
use maki_config::project::TrustState;
use maki_config::{ModelPolicy, ProjectConfig, UiConfig};
use maki_lua::agent_autocmd::HostEvent;
use maki_lua::session_snapshot::{
    MODE_BUILD, MODE_PLAN, STATUS_IDLE, STATUS_NEEDS_INPUT, STATUS_WORKING, SessionQueueSnapshot,
    SessionSnapshot,
};
use maki_lua::{
    EventHandle, HintReader, InputMode, InputRequest, InputRulesReader, Key, KeymapReader,
    LuaCommandReader, ModelRequest, PackCommand, PackPreparation, PlanRequest, SessionEndReason,
    SessionRequest, TaskRequest, UiAction, UiReply, WinEvent, fetch_provider_usage,
};
use maki_providers::Timeouts;
use maki_providers::model_registry;
use maki_providers::models_cache::{ModelList, fetch_all_models_cached};
use maki_providers::provider::{Provider, from_model};
use maki_providers::{Message, Model, ModelTier};
use maki_storage::StateDir;
use maki_storage::blocking;
use maki_storage::id::{MakiId, MakiIdParseError, SessionRef};
use maki_storage::model::{persist_model, push_recent, read_expanded_providers, read_recents};
use maki_storage::sessions::normalize_title;
use ratatui::layout::Rect;
use serde_json::json;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Notify, oneshot};
use tokio::task::coop::unconstrained;
use tokio::task::{JoinHandle, spawn_blocking};
use tokio::time::timeout;
use tokio_util::task::{AbortOnDropHandle, TaskTracker};
use tracing::{debug, info, warn};

use crate::agent::model_slots::Landed;
use crate::agent::{
    AgentDeps, AgentHandles, ModelSlot, ModelSlots, Respawn,
    shared_queue::{Compaction, QueueItem, QueuedInput},
};
use crate::app::mode::Mode;
use crate::app::shell::{ShellEvent, spawn_shell};
use crate::app::tasks::TaskWatch;
use crate::app::{App, Msg, Notification, QueuedMessage, SubmitOutcome, turn_response};
use crate::appearance::{self, Background};
use crate::color_compat;
use crate::components::input::Submission;
use crate::components::keybindings::key;
use crate::components::model_picker::PickerMemory;
use crate::components::usage_modal::UsageFetchState;
use crate::components::{Action, ExitRequest};
use crate::decor_worker;
use crate::input::InputReader;
use crate::present::{Caret, present};
use crate::repaint::{Dirty, IDLE_POLL};
use crate::{AppSession, OpenSession};

use crate::storage_writer::StorageWriter;
use crate::terminal::{self, BlockedKind, ProgramState, ProgramStatusReporter, cursor_style};

/// Max events handled per frame so a flood cannot starve rendering.
const DRAIN_BUDGET: usize = 256;
const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DELETE_FOCUSED_ERR: &str = "cannot delete the focused session";
const NOT_LIVE_ERR: &str = "session not live";
const PACK_PREPARING: &str = "Checking packages...";
const PACK_BUSY_ERR: &str = "a package command is already running";
const UNKNOWN_MODE_ERR: &str = "unknown mode; expected \"build\" or \"plan\"";
const PACK_PANIC_ERR: &str = "the package command stopped unexpectedly";
const FORKED_MSG: &str = "Forked, {key} to switch back";
const CWD_GONE_FLASH: &str = "current directory no longer exists. /cd to move elsewhere";
/// A `/reload` cannot ask the terminal: its input reader owns the tty.
const RESTART_FOR_BACKGROUND: &str =
    "ui.theme: restart Maki to detect the terminal background, using dark until then";

/// Tabs carry their in-memory sessions so `/reload` reopens them without a
/// disk round-trip; `session_has_content` tells which ones were saved.
pub(crate) struct ShutdownReport {
    pub exit: ExitRequest,
    pub tabs: Vec<OpenSession>,
    pub focused: usize,
    /// Sessions whose transcript never reached disk. The caller prints them
    /// after dropping the terminal guard, so the line is not lost with the
    /// alternate screen.
    pub unsaved: Vec<MakiId>,
    /// A result that settled but no run drained yet lives only in its
    /// session's mailbox. `/reload` carries these over so the next generation
    /// finds it there instead of reporting the work lost.
    pub mailboxes: Vec<SessionMailbox>,
}

pub struct EventLoopParams {
    pub model: Model,
    pub needs_login: bool,
    pub commands: Vec<CustomCommand>,
    pub sessions: Vec<OpenSession>,
    pub focused: usize,
    pub startup_warnings: Vec<String>,
    pub startup_notice: Option<String>,
    /// Shown in a popup that stays up until dismissed, not a flash.
    pub startup_alert: Option<String>,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    /// Caps stderr.log, where the TUI sends fd 2.
    pub max_log_bytes: u64,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: Timeouts,
    pub exit_on_done: bool,
    /// How long `exit_on_done` waits for background work, `None` for not at all.
    pub background_wait: Option<Duration>,
    pub lua_command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: HintReader,
    pub input_rules_reader: InputRulesReader,
    pub ui_action_rx: UnboundedReceiver<UiAction>,
    pub ui_waker: UiWaker,
    pub lua_event_handle: EventHandle,
    pub model_policy: Arc<ModelPolicy>,
    pub project_config: ProjectConfig,
    /// What `/trust` would grant and what the `[restricted]` indicator reports.
    pub trust: TrustState,
    /// The mailboxes the generation before handed over, held until this one
    /// registered its own.
    pub handed_over: Vec<SessionMailbox>,
    pub host_prompt: HostPrompt,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SessionStatus {
    Working,
    NeedsInput,
    Idle,
}

enum PendingCompletion {
    WaitingForQueueDrain(Notification),
    Due(Notification),
}

#[derive(Default)]
struct RunNotificationState {
    response_candidate: Option<String>,
    pending_completion: Option<PendingCompletion>,
    last_attention: Option<Notification>,
    outcome: Option<ProgramState>,
}

impl RunNotificationState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn on_queue_item_consumed(&mut self) {
        self.response_candidate = None;
        self.pending_completion = None;
        self.outcome = None;
    }

    fn on_turn_complete(&mut self, message: &Message) {
        self.response_candidate = turn_response(message);
    }

    fn on_done(&mut self, event: &AgentEvent) {
        let notification = match event {
            AgentEvent::Done { .. } => {
                self.outcome = Some(ProgramState::Done);
                Notification::TurnComplete {
                    response: self.response_candidate.take(),
                }
            }
            AgentEvent::Error { .. } => {
                self.outcome = Some(ProgramState::Error);
                self.response_candidate = None;
                Notification::error_completion()
            }
            _ => return,
        };
        self.pending_completion = Some(PendingCompletion::WaitingForQueueDrain(notification));
    }

    fn on_drain(&mut self) {
        self.pending_completion = match self.pending_completion.take() {
            Some(PendingCompletion::WaitingForQueueDrain(notification)) => {
                Some(PendingCompletion::Due(notification))
            }
            pending => pending,
        };
    }

    fn on_manual_exit(&mut self) {
        self.pending_completion = None;
    }

    /// True between `Done`/`Error` and the run's `QueueDrained`. An exit must
    /// not fire in that window: a queued follow-up may still start a new run.
    fn waiting_for_drain(&self) -> bool {
        matches!(
            self.pending_completion,
            Some(PendingCompletion::WaitingForQueueDrain(_))
        )
    }

    fn reconcile(
        &mut self,
        attention: Option<Notification>,
        status: SessionStatus,
    ) -> Option<Notification> {
        let settled = attention.is_none() && status == SessionStatus::Idle;
        let prompt = (attention != self.last_attention)
            .then(|| attention.clone())
            .flatten();
        self.last_attention = attention;

        // A turn can end while work goes on, and queued input or a background
        // task may wake the agent again. So the completion waits until the
        // session waits on the user (see `App::is_working`), and new work
        // drops it.
        let completion = match self.pending_completion.take() {
            Some(PendingCompletion::Due(notification)) if settled => Some(notification),
            pending => {
                self.pending_completion = pending;
                None
            }
        };
        prompt.or(completion)
    }
}

impl SessionStatus {
    /// Background work keeps the session working, like the status bar, so a
    /// picker never flashes finished while a result is still to come.
    fn of(app: &App) -> Self {
        if app.awaiting_input() {
            Self::NeedsInput
        } else if app.is_working() {
            Self::Working
        } else {
            Self::Idle
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Working => STATUS_WORKING,
            Self::NeedsInput => STATUS_NEEDS_INPUT,
            Self::Idle => STATUS_IDLE,
        }
    }
}

fn prepend_preamble(preamble: &mut Vec<Message>, mut leading: Vec<Message>) {
    leading.append(preamble);
    *preamble = leading;
}

fn is_current_top_level(current_run_id: u64, envelope: &Envelope) -> bool {
    envelope.run_id == current_run_id && envelope.subagent.is_none()
}

fn select_notification(
    selected: Option<Notification>,
    candidate: Option<Notification>,
) -> Option<Notification> {
    match (selected, candidate) {
        (Some(current), Some(candidate)) if candidate.is_urgent() && !current.is_urgent() => {
            Some(candidate)
        }
        (selected @ Some(_), _) => selected,
        (None, candidate) => candidate,
    }
}

fn blocked_kind(attention: Option<&Notification>) -> BlockedKind {
    match attention {
        Some(Notification::AuthenticationRequired) => BlockedKind::Auth,
        Some(Notification::QuestionRequested) => BlockedKind::Question,
        _ => BlockedKind::Permission,
    }
}

fn program_state_rank(state: ProgramState) -> u8 {
    match state {
        ProgramState::Idle => 0,
        ProgramState::Done => 1,
        ProgramState::Error => 2,
        ProgramState::Working => 3,
        ProgramState::Blocked(_) => 4,
    }
}

fn select_program_status<T>(
    candidates: impl Iterator<Item = (ProgramState, bool, T)>,
) -> Option<(ProgramState, T)> {
    candidates
        .max_by_key(|&(state, focused, _)| (program_state_rank(state), focused))
        .map(|(state, _, payload)| (state, payload))
}

/// Maki never turns focus reporting on under Windows, so anything that looks
/// like a focus record there is a guess rather than a report.
const TRUSTS_FOCUS_EVENTS: bool = !cfg!(windows);

/// How long input keeps an unproven terminal counting as watched. Short on
/// purpose: suppressing wrongly hides a finished turn, notifying wrongly only
/// costs a bell.
const INPUT_IMPLIES_WATCHING: Duration = Duration::from_secs(30);

/// Whether the user is watching this terminal.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Focus {
    /// No focus report has arrived yet. Terminals that never send one (GNU
    /// screen, tmux without `focus-events on`) stay here for good, and one
    /// never arrives while the user simply keeps the window focused, so
    /// recent input is the only evidence available.
    Unproven {
        last_input: Option<Instant>,
    },
    Focused,
    Unfocused,
}

impl Default for Focus {
    fn default() -> Self {
        Self::Unproven { last_input: None }
    }
}

impl Focus {
    /// A prompt parks the agent on the user, so it rings even while they
    /// watch. A finished turn they can already see is just noise.
    fn allows(self, notification: &Notification) -> bool {
        notification.is_urgent() || !self.is_watched()
    }

    fn is_watched(self) -> bool {
        match self {
            Self::Focused => true,
            Self::Unfocused => false,
            Self::Unproven { last_input } => {
                last_input.is_some_and(|at| at.elapsed() < INPUT_IMPLIES_WATCHING)
            }
        }
    }

    fn report(&mut self, reported: Self) {
        if TRUSTS_FOCUS_EVENTS {
            *self = reported;
        }
    }

    /// Typing proves the user was here just now. It never latches: without a
    /// report to clear it, a terminal that cannot send `FocusLost` would go
    /// quiet for good, so the evidence expires on its own.
    fn note_input(&mut self) {
        match self {
            Self::Unfocused => *self = Self::Focused,
            Self::Unproven { last_input } => *last_input = Some(Instant::now()),
            Self::Focused => {}
        }
    }

    /// An editor, a shell or a suspend eats the focus reports we would have
    /// seen, so assume the user may have walked away.
    fn on_resume(&mut self) {
        match self {
            Self::Focused => *self = Self::Unfocused,
            Self::Unproven { last_input } => *last_input = None,
            Self::Unfocused => {}
        }
    }
}

fn parse_session_id(id: &str) -> Result<MakiId, String> {
    id.parse().map_err(|e: MakiIdParseError| e.to_string())
}

fn parse_mode(mode: &str) -> Result<Mode, String> {
    match mode {
        MODE_BUILD => Ok(Mode::Build),
        MODE_PLAN => Ok(Mode::Plan),
        other => Err(format!("{UNKNOWN_MODE_ERR}, got {other:?}")),
    }
}

/// `maki.model.set`'s answer, with whatever model the session is on by now.
fn apply_settings(app: &mut App, thinking: Option<String>, fast: Option<bool>) -> UiReply {
    if let Some(thinking) = thinking {
        app.set_thinking(&thinking)?;
    }
    if let Some(fast) = fast {
        app.set_fast(fast)?;
    }
    Ok(app.model_state())
}

struct SessionRuntime {
    app: App,
    handles: AgentHandles,
    shell_tx: UnboundedSender<ShellEvent>,
    shell_rx: UnboundedReceiver<ShellEvent>,
    last_status: SessionStatus,
    last_title: String,
    last_tasks: TaskWatch,
    notifications: RunNotificationState,
    /// This session's model and provider, one cell per runtime and never
    /// shared. The agent loop reads it at the start of every turn, and a
    /// background tab must not get dragged onto the model another tab just
    /// picked. Only `apply_model` writes it, on the UI thread.
    slot: Arc<ArcSwap<ModelSlot>>,
}

impl SessionRuntime {
    fn id(&self) -> MakiId {
        self.app.state.session.id
    }

    fn program_state(&self) -> (ProgramState, Option<String>) {
        let state = match self.last_status {
            SessionStatus::NeedsInput => {
                let attention = self.app.attention();
                let msg = attention.as_ref().map(Notification::message);
                return (ProgramState::Blocked(blocked_kind(attention.as_ref())), msg);
            }
            SessionStatus::Working => ProgramState::Working,
            SessionStatus::Idle => self.notifications.outcome.unwrap_or(ProgramState::Idle),
        };
        (state, None)
    }

    /// New work cancels an `exit_on_done` exit still waiting on its drain.
    fn reset_run_notifications(&mut self) {
        if self.notifications.waiting_for_drain() {
            self.app.clear_exit_request();
        }
        self.notifications.reset();
    }
}

/// Everything needed to bring up a new session runtime after startup.
struct SpawnCtx {
    storage: StateDir,
    config: AgentConfig,
    ui_config: UiConfig,
    input_history_size: usize,
    /// Prototype only: every runtime forks its own manager so session rules
    /// stay per-session. `App::new` then restates the fork from the session's
    /// meta, so a toggle the prototype never heard about still holds.
    permissions: Arc<PermissionManager>,
    timeouts: Timeouts,
    custom_commands: Arc<[CustomCommand]>,
    lua_command_reader: LuaCommandReader,
    keymap_reader: KeymapReader,
    hint_reader: HintReader,
    input_rules_reader: InputRulesReader,
    lua_event_handle: EventHandle,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    available_models: Arc<ArcSwapOption<ModelList>>,
    picker_memory: Arc<PickerMemory>,
    storage_writer: Arc<StorageWriter>,
    model_policy: Arc<ModelPolicy>,
    trust: TrustState,
    host_prompt: Arc<HostPrompt>,
}

impl SpawnCtx {
    fn spawn_runtime(&self, open: OpenSession, slot: Arc<ModelSlot>) -> SessionRuntime {
        let session = &open.session;
        let resumed = !session.messages().is_empty();
        let permissions = Arc::new(self.permissions.fork());
        let cell = Arc::new(ArcSwap::from(Arc::clone(&slot)));
        let handles = AgentHandles::spawn(
            &cell,
            Resumed {
                id: SessionRef::from(session.id),
                history: session.messages().to_vec(),
                context_size: session.meta.context_size,
                frame: session.frame().cloned(),
                // The tab owns the session and persists it through
                // `StorageWriter`, so the agent only gets the transcript and
                // the frame it was sent under.
                session: None,
            },
            self.config.clone(),
            self.ui_config.tool_output_lines,
            &permissions,
            self.lua_event_handle.clone(),
            AgentDeps {
                mcp_handle: self.mcp_handle.clone(),
                mcp_config_errors: self.mcp_config_errors.clone(),
                timeouts: self.timeouts,
                model_policy: Arc::clone(&self.model_policy),
                host_prompt: Arc::clone(&self.host_prompt),
            },
        );
        let mut app = App::new(
            &slot.model,
            open,
            self.storage.clone(),
            Arc::clone(&self.available_models),
            Arc::clone(&self.picker_memory),
            handles.mcp_reader(),
            handles.deps.mcp_config_errors.clone(),
            self.lua_command_reader.clone(),
            self.keymap_reader.clone(),
            self.hint_reader.clone(),
            self.input_rules_reader.clone(),
            Arc::clone(&self.storage_writer),
            self.ui_config.clone(),
            self.input_history_size,
            permissions,
            Arc::clone(&self.custom_commands),
            self.lua_event_handle.clone(),
        );
        app.trust = self.trust.clone();
        handles.apply_to_app(&mut app);
        if resumed {
            app.restore_resumed_session();
        }
        let (shell_tx, shell_rx) = unbounded_channel::<ShellEvent>();
        let last_title = app.state.session.title.clone();
        SessionRuntime {
            app,
            handles,
            shell_tx,
            shell_rx,
            last_status: SessionStatus::Idle,
            last_title,
            last_tasks: TaskWatch::default(),
            notifications: RunNotificationState::default(),
            slot: cell,
        }
    }
}

/// A plugin polling a slug needs the live instance whatever tab it is in:
/// after a failover the original slug may only be served by a background
/// session. Fresh providers keep headless and no-session answers consistent.
async fn provider_usage_reply(slots: Vec<Arc<ArcSwap<ModelSlot>>>, slug: String) -> UiReply {
    let live = slots.iter().find_map(|slot| {
        let slot = slot.load_full();
        (slot.model.provider.as_ref() == slug.as_str()).then(|| Arc::clone(&slot.provider))
    });
    match live {
        Some(provider) => fetch_provider_usage(provider.as_ref(), &slug).await,
        None => {
            let lookup = slug.clone();
            let provider = blocking(move || {
                maki_providers::provider::provider_for_slug(&lookup, Timeouts::default())
            })
            .await
            .map_err(|e| e.user_message())?;
            fetch_provider_usage(provider.as_ref(), &slug).await
        }
    }
}

pub(crate) struct EventLoop<'t> {
    terminal: &'t mut ratatui::DefaultTerminal,
    sessions: Vec<SessionRuntime>,
    focused: usize,
    /// The `(session, task)` pair whose transcript was on screen last frame.
    last_focus: Option<(MakiId, Arc<str>)>,
    /// The mode `ModeChanged` last announced, one for all tabs.
    last_mode: InputMode,
    focus: Focus,
    notifier: Option<terminal::TerminalNotifier>,
    program_status: Option<ProgramStatusReporter>,
    ctx: SpawnCtx,
    slots: ModelSlots,
    input: InputReader,
    warn_rx: UnboundedReceiver<String>,
    warn_tx: UnboundedSender<String>,
    models_fetched: Arc<Notify>,
    ui_action_rx: UnboundedReceiver<UiAction>,
    ui_waker: UiWaker,
    /// One package command at a time. The work runs as its own task, so a
    /// second `/packupdate` would otherwise race the first over the same clones
    /// and locks.
    pack: Option<JoinHandle<PackPreparation>>,
    /// The stored session a focus is reading. One at a time, and none while
    /// an earlier one waits for its slot, with later requests in
    /// `queued_focus`, so they land in the order they were made, as they did
    /// when each one read inline.
    opening: Option<AbortOnDropHandle<Box<OpenedSession>>>,
    queued_focus: VecDeque<(MakiId, oneshot::Sender<UiReply>)>,
    /// Picks and sessions waiting for a slot [`ModelSlots`] is building.
    waiting: SlotWaiters,
    /// Writes to sessions no tab has open, which shutdown waits for like it
    /// waits for the storage writer.
    stored_writes: TaskTracker,
    /// Whether the cwd was gone last frame, so the flash shows once per loss.
    cwd_lost: bool,
    _model_fetch_task: AbortOnDropHandle<()>,
}

/// A focus that reads a stored session.
struct PendingFocus {
    reply_tx: oneshot::Sender<UiReply>,
    /// The tab focused when it was asked, the only one it may take over.
    from: MakiId,
}

/// A stored session a focus read off the loop, for the loop to bring up once
/// it has a slot for its model.
struct OpenedSession(PendingFocus, Result<OpenSession, String>);

/// A model picked for the tab focused at the time.
struct Pick {
    /// The tab's slot cell, which stays the same when `/new` gives the tab
    /// another session while the build is out.
    tab: Weak<ArcSwap<ModelSlot>>,
    origin: PickOrigin,
}

/// Who hears how a pick went.
enum PickOrigin {
    /// The model picker or a login, which flash a failure.
    Picker,
    /// `maki.model.set`, whose thinking and fast go on top of the new model
    /// before it answers.
    Lua {
        thinking: Option<String>,
        fast: Option<bool>,
        reply_tx: oneshot::Sender<UiReply>,
    },
}

/// A session `maki.session.new` opens in a tab, which answers with its id.
struct CreatedSession {
    open: OpenSession,
    prompt: Option<String>,
    focus: bool,
    reply_tx: oneshot::Sender<UiReply>,
}

/// What a slot is wanted for.
enum Wanted {
    Pick(Pick),
    Open(PendingFocus, OpenSession),
    Create(CreatedSession),
}

struct Waiter {
    spec: String,
    built: Option<Result<Arc<ModelSlot>, String>>,
    wanted: Wanted,
}

/// Everything waiting for a slot, in the order it asked. A session comes up
/// as soon as its slot is there. A pick also waits for the picks made before
/// it, so they apply in order and the last one wins.
#[derive(Default)]
struct SlotWaiters(VecDeque<Waiter>);

impl SlotWaiters {
    fn push(&mut self, slots: &mut ModelSlots, spec: String, wanted: Wanted) {
        let built = slots.get(&spec).map(Ok);
        if built.is_none() {
            slots.request(&spec);
        }
        self.0.push_back(Waiter {
            spec,
            built,
            wanted,
        });
    }

    fn landed(&mut self, spec: &str, built: &Result<Arc<ModelSlot>, String>) {
        for waiter in self.0.iter_mut().filter(|waiter| waiter.spec == spec) {
            waiter.built.get_or_insert_with(|| built.clone());
        }
    }

    /// After an invalidate nothing a waiter holds is current, so each asks
    /// again.
    fn restart(&mut self, slots: &mut ModelSlots) {
        for waiter in &mut self.0 {
            waiter.built = None;
            slots.request(&waiter.spec);
        }
    }

    fn take_ready(&mut self) -> Vec<(Waiter, Result<Arc<ModelSlot>, String>)> {
        let mut ready = Vec::new();
        let mut pick_ahead = false;
        for mut waiter in mem::take(&mut self.0) {
            let is_pick = matches!(waiter.wanted, Wanted::Pick(_));
            match waiter.built.take_if(|_| !(is_pick && pick_ahead)) {
                Some(built) => ready.push((waiter, built)),
                None => {
                    pick_ahead |= is_pick;
                    self.0.push_back(waiter);
                }
            }
        }
        ready
    }

    /// The spec `tab` ends up on once its picks have settled: the last one
    /// made for it.
    fn pending_pick(&self, tab: &Arc<ArcSwap<ModelSlot>>) -> Option<&str> {
        let tab = Arc::downgrade(tab);
        self.0
            .iter()
            .rfind(|waiter| matches!(&waiter.wanted, Wanted::Pick(pick) if Weak::ptr_eq(&pick.tab, &tab)))
            .map(|waiter| waiter.spec.as_str())
    }

    fn opens_a_session(&self) -> bool {
        self.0
            .iter()
            .any(|waiter| matches!(waiter.wanted, Wanted::Open(..)))
    }
}

/// One item from any of the event loop's sources; `None` from `next_wake`
/// means the wait timed out (animation/idle tick).
enum Wake {
    Input(Event),
    InputGone,
    Ui(UiAction),
    /// Nothing to handle. The tick after every wake is what reads plugin
    /// windows.
    PluginWindow,
    /// Nothing to handle either. The tick takes in what the worker finished
    /// for the chat input.
    Decor,
    Agent(usize, Box<maki_agent::Envelope>),
    Shell(usize, ShellEvent),
    Warn(String),
    Pack(Box<PackPreparation>),
    /// `None` when the read panicked, which dropped its reply.
    SessionOpened(Option<Box<OpenedSession>>),
    ModelsDiscovered,
    SlotBuilt(Landed),
}

/// A value from the channel, or never, once it is closed: a closed channel
/// is not an event.
/// Outside tokio's coop budget: it would report an empty queue after 128
/// envelopes in one poll and end the drain at half of [`DRAIN_BUDGET`].
async fn next_or_pending<T>(rx: &mut UnboundedReceiver<T>) -> T {
    match unconstrained(rx.recv()).await {
        Some(value) => value,
        None => pending().await,
    }
}

/// The running package command's answer, or never, when none is running.
///
/// The handle is only cleared once it has answered, so a `select!` that drops
/// this future for another wake leaves the task in place. A panic still
/// answers, or every later package command would report "already running"
/// until the next `/reload`.
async fn pack_done(pack: &mut Option<JoinHandle<PackPreparation>>) -> PackPreparation {
    let Some(task) = pack else {
        return pending().await;
    };
    let preparation = task
        .await
        .unwrap_or_else(|_| PackPreparation::failed(PACK_PANIC_ERR.to_owned()));
    *pack = None;
    preparation
}

/// What a task answered, `None` when it panicked, or never, when none is
/// running. Cleared only once it answered, like [`pack_done`].
async fn answered<T>(task: &mut Option<AbortOnDropHandle<T>>) -> Option<T> {
    let Some(handle) = task else {
        return pending().await;
    };
    let answer = handle.await.ok();
    *task = None;
    answer
}

/// The first agent envelope or shell event from any session.
async fn session_wake(sessions: &mut [SessionRuntime]) -> Wake {
    let waits: Vec<LocalBoxFuture<'_, Wake>> = sessions
        .iter_mut()
        .enumerate()
        .flat_map(|(i, rt)| {
            let (agent_rx, shell_rx) = (&mut rt.handles.agent_rx, &mut rt.shell_rx);
            [
                Box::pin(async move { Wake::Agent(i, Box::new(next_or_pending(agent_rx).await)) })
                    as LocalBoxFuture<'_, Wake>,
                Box::pin(async move {
                    match shell_rx.recv().await {
                        Some(event) => Wake::Shell(i, event),
                        None => pending().await,
                    }
                }),
            ]
        })
        .collect();
    if waits.is_empty() {
        return pending().await;
    }
    select_all(waits).await.0
}

struct BackgroundModels {
    available: Arc<ArcSwapOption<ModelList>>,
    warn_rx: UnboundedReceiver<String>,
    warn_tx: UnboundedSender<String>,
    models_fetched: Arc<Notify>,
    task: AbortOnDropHandle<()>,
}

/// The one way discovery starts, so startup and `/models refresh` cannot drift
/// apart. It only reports that it finished and leaves rebuilding to the UI
/// thread, over a `Notify` so the loop wakes on it instead of noticing at the
/// next tick. It keeps one permit, which collapses overlapping fetches into a
/// single rebuild.
fn fetch_models(
    available: Arc<ArcSwapOption<ModelList>>,
    policy: Arc<ModelPolicy>,
    warn_tx: UnboundedSender<String>,
    models_fetched: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        fetch_all_models_cached(&policy, |list, warning| {
            if let Some(warning) = warning {
                let _ = warn_tx.send(warning);
            }
            available.store(Some(Arc::new(list)));
        })
        .await;
        models_fetched.notify_one();
    })
}

fn spawn_model_fetch(policy: Arc<ModelPolicy>) -> BackgroundModels {
    let available: Arc<ArcSwapOption<ModelList>> = Arc::new(ArcSwapOption::empty());
    let (warn_tx, warn_rx) = unbounded_channel::<String>();
    let models_fetched = Arc::new(Notify::new());
    let task = AbortOnDropHandle::new(fetch_models(
        Arc::clone(&available),
        policy,
        warn_tx.clone(),
        Arc::clone(&models_fetched),
    ));
    BackgroundModels {
        available,
        warn_rx,
        warn_tx,
        models_fetched,
        task,
    }
}

impl<'t> EventLoop<'t> {
    pub(crate) async fn new(
        terminal: &'t mut ratatui::DefaultTerminal,
        params: EventLoopParams,
    ) -> Result<Self> {
        let EventLoopParams {
            mut model,
            needs_login,
            commands,
            sessions,
            focused,
            mut startup_warnings,
            startup_notice,
            startup_alert,
            storage,
            config,
            ui_config,
            input_history_size,
            max_log_bytes: _,
            permissions,
            timeouts,
            exit_on_done,
            background_wait,
            lua_command_reader,
            keymap_reader,
            hint_reader,
            input_rules_reader,
            ui_action_rx,
            ui_waker,
            lua_event_handle,
            model_policy,
            project_config,
            trust,
            handed_over,
            host_prompt,
        } = params;
        // Apply the config theme before the warmup thread spawns, or warmup
        // could bake the syntax palette from the old theme. It is not
        // persisted, so the user's saved pick survives.
        if let Some(ref pair) = ui_config.theme {
            let background = appearance::background();
            if pair.follows_background() && background.is_none() {
                startup_warnings.push(RESTART_FOR_BACKGROUND.to_owned());
            }
            let name = pair.name(background == Some(Background::Light));
            match crate::theme::load_by_name(name) {
                Ok(theme) => crate::theme::set(theme),
                Err(e) => startup_warnings.push(format!("config ui.theme: {e}")),
            }
        }

        static PROCESS_WARMUP: std::sync::Once = std::sync::Once::new();
        PROCESS_WARMUP.call_once(|| {
            maki_highlight::pool::spawn(crate::highlight::warmup);
            crate::update::spawn_check();
        });

        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let (mcp_handle, mcp_config_errors) = mcp::start(&cwd, project_config).await;

        let provider: Arc<dyn Provider> = if needs_login {
            Arc::from(maki_providers::provider::from_model_fallback(
                &mut model, timeouts,
            ))
        } else {
            Arc::from(from_model(&mut model, timeouts).context("create provider")?)
        };
        let startup_slot = Arc::new(ModelSlot {
            model: model.clone(),
            provider,
        });
        let mut slots = ModelSlots::new(startup_slot, Arc::clone(&model_policy), timeouts);
        let bg = spawn_model_fetch(Arc::clone(&model_policy));
        let storage_writer = Arc::new(StorageWriter::new(storage.clone(), bg.warn_tx.clone()));

        let notifier = terminal::TerminalNotifier::new(ui_config.notifications).await;
        let program_status = ui_config.program_status.then(ProgramStatusReporter::new);
        let recent_models = read_recents(&storage)
            .into_iter()
            .filter(|spec| model_policy.allows(spec))
            .collect();
        let picker_memory = PickerMemory::new(recent_models, read_expanded_providers(&storage));
        let ctx = SpawnCtx {
            storage,
            config,
            ui_config,
            input_history_size,
            permissions,
            timeouts,
            custom_commands: Arc::from(commands),
            lua_command_reader,
            keymap_reader,
            hint_reader,
            input_rules_reader,
            lua_event_handle,
            mcp_handle,
            mcp_config_errors,
            available_models: bg.available,
            picker_memory: Arc::new(picker_memory),
            storage_writer,
            model_policy,
            trust,
            host_prompt: Arc::new(host_prompt),
        };

        let mut runtimes: Vec<SessionRuntime> = sessions
            .into_iter()
            .map(|open| {
                let (slot, reason) = slots.get_or_fallback(&open.session.model);
                let mut rt = ctx.spawn_runtime(open, slot);
                if let Some(reason) = reason {
                    rt.app.flash(reason);
                }
                rt
            })
            .collect();
        drop(handed_over);
        if runtimes.is_empty() {
            return Err(eyre!("event loop needs at least one session"));
        }
        let focused = focused.min(runtimes.len() - 1);
        let app = &mut runtimes[focused].app;
        if exit_on_done {
            app.exit_when_done(background_wait);
        }
        if needs_login {
            app.login_picker.open(app.storage.clone());
        }
        if !ctx.mcp_config_errors.is_empty() {
            let msg = format!("MCP config error: {}", ctx.mcp_config_errors);
            app.queue_flash(msg);
        }
        if let Some(notice) = startup_notice {
            app.queue_flash(notice);
        }
        for warning in startup_warnings {
            app.queue_flash(warning);
        }
        if let Some(alert) = startup_alert {
            app.open_alert(alert);
        }

        Ok(Self {
            terminal,
            sessions: runtimes,
            focused,
            last_focus: None,
            last_mode: InputMode::Insert,
            focus: Focus::default(),
            notifier,
            program_status,
            ctx,
            slots,
            input: InputReader::new().context("watch terminal input")?,
            warn_rx: bg.warn_rx,
            warn_tx: bg.warn_tx,
            models_fetched: bg.models_fetched,
            ui_action_rx,
            ui_waker,
            pack: None,
            opening: None,
            queued_focus: VecDeque::new(),
            waiting: SlotWaiters::default(),
            stored_writes: TaskTracker::new(),
            cwd_lost: false,
            _model_fetch_task: bg.task,
        })
    }

    fn focused_app(&mut self) -> &mut App {
        &mut self.sessions[self.focused].app
    }

    fn paint(&mut self) -> Result<()> {
        let app = &mut self.sessions[self.focused].app;
        present(self.terminal, |frame| {
            let position = app.view(frame);
            color_compat::downgrade_if_needed(frame.buffer_mut());
            position.map(|position| Caret {
                position,
                shown: app.shows_terminal_cursor(),
                style: app.cursor_style().map(cursor_style),
            })
        })?;
        Ok(())
    }

    pub(crate) async fn run(mut self, initial_prompt: Option<String>) -> Result<ShutdownReport> {
        if let Some(prompt) = initial_prompt {
            let sub = Submission {
                text: prompt,
                images: Vec::new(),
            };
            let actions = self.focused_app().handle_submit(sub);
            self.dispatch(self.focused, actions);
        }
        // The first frame always paints. After that only a poller, an event or
        // an animation tick owes another.
        let mut dirty = Dirty::YES;
        let result = loop {
            dirty |= self.tick();
            match self.drain_channels().await {
                Ok(d) => dirty |= d,
                Err(e) => break Err(e),
            }
            self.checkpoint_all();
            for rt in &self.sessions {
                rt.handles.publish_chat_settings(&rt.app);
            }
            if dirty.take()
                && let Err(e) = self.paint()
            {
                break Err(e);
            }

            if let Some(i) = self.sessions.iter().position(|rt| {
                rt.app.exit_request != ExitRequest::None
                    && !rt.notifications.waiting_for_drain()
                    && !rt.app.awaits_background(Instant::now())
            }) {
                // A result settling since the drain above both clears its
                // receipt and wakes the session, so the wake is claimed after
                // the read: its run goes first, and that run's drain exits.
                if self.sessions[i].app.exits_on_done() {
                    dirty |= self.start_woken_runs();
                }
                if self.sessions[i].app.exit_request != ExitRequest::None {
                    // A backgrounded session can finish an `exit_on_done` turn.
                    // Focus it so shutdown reports its exit code and id.
                    self.set_focused(i);
                    self.emit_notifications();
                    break Ok(());
                }
            }

            // Sleeping a whole frame instead of a fraction of one is what
            // makes a spinner cost 12 paints a second instead of 62.
            let cadence = self.sessions[self.focused].app.cadence();
            match self.next_wake(cadence.frame().unwrap_or(IDLE_POLL)).await {
                // Any event can change the screen, so paint after handling it
                // rather than asking every handler to prove it did.
                Some(wake) => {
                    dirty = Dirty::YES;
                    if let Err(e) = self.handle_wake(wake) {
                        break Err(e);
                    }
                }
                // Only the clock moved, so motion alone owes the frame. The
                // cadence is the one from before the sleep, so motion that
                // just stopped still gets a last paint to clear itself off
                // the screen.
                None => dirty |= Dirty::from(cadence.moves()),
            }
        };
        // Fatal errors still save every session, kill MCP process groups,
        // and drain the storage writer before the process exits.
        let report = self.shutdown().await;
        result.map(|()| report)
    }

    /// Wait for the next event from any source, or time out so animations
    /// and periodic polls keep running.
    async fn next_wake(&mut self, wait: Duration) -> Option<Wake> {
        timeout(wait, self.wake()).await.ok()
    }

    /// An event that is already pending, checked once. A zero timeout would
    /// still arm a timer and wait for its next tick. A `select!` rather than
    /// `now_or_never`, so the poll carries the loop's own waker: crossterm's
    /// reader thread keeps whatever waker it is first polled with.
    async fn pending_wake(&mut self) -> Option<Wake> {
        tokio::select! {
            biased;
            wake = self.wake() => Some(wake),
            () = ready(()) => None,
        }
    }

    /// Terminal input first, then plugin UI actions, then the rest, so a
    /// keypress is never queued behind a burst of agent events.
    async fn wake(&mut self) -> Wake {
        tokio::select! {
            biased;
            event = self.input.next() => event.map_or(Wake::InputGone, Wake::Input),
            action = next_or_pending(&mut self.ui_action_rx) => Wake::Ui(action),
            () = self.ui_waker.woken() => Wake::PluginWindow,
            () = decor_worker::wakes().woken() => Wake::Decor,
            Some(warning) = self.warn_rx.recv() => Wake::Warn(warning),
            preparation = pack_done(&mut self.pack) => Wake::Pack(Box::new(preparation)),
            answer = answered(&mut self.opening) => Wake::SessionOpened(answer),
            Some(landed) = self.slots.next_built() => Wake::SlotBuilt(landed),
            () = self.models_fetched.notified() => Wake::ModelsDiscovered,
            wake = session_wake(&mut self.sessions) => wake,
        }
    }

    fn handle_wake(&mut self, wake: Wake) -> Result<()> {
        match wake {
            Wake::Input(ev) => self.handle_input(ev),
            Wake::InputGone => return Err(eyre!("terminal input reader stopped")),
            Wake::Ui(action) => self.handle_ui_action(action),
            Wake::PluginWindow | Wake::Decor => {}
            Wake::Agent(i, envelope) => self.handle_agent(i, envelope),
            Wake::Shell(i, event) => self.sessions[i].app.handle_shell_event(event),
            Wake::Warn(warning) => self.focused_app().flash(warning),
            Wake::Pack(preparation) => self.finish_pack(*preparation),
            Wake::SessionOpened(answer) => self.finish_open(answer),
            Wake::ModelsDiscovered => self.rebuild_models(),
            Wake::SlotBuilt(landed) => self.land_slot(landed),
        }
        Ok(())
    }

    /// The one save trigger. A checkpoint writes only on a real change, so
    /// every tool result reaches disk within a frame while an idle session
    /// writes nothing.
    fn checkpoint_all(&mut self) {
        for rt in &mut self.sessions {
            rt.app.checkpoint();
        }
    }

    /// One `getcwd` a frame, so a deleted directory is reported before it
    /// surfaces as an unrelated "no such file". The frame that flashes owes a
    /// paint, or an idle loop lets it expire unseen.
    fn watch_cwd(&mut self) -> Dirty {
        let lost = std::env::current_dir().is_err();
        if lost == self.cwd_lost {
            return Dirty::NO;
        }
        self.cwd_lost = lost;
        if !lost {
            return Dirty::NO;
        }
        self.focused_app().flash(CWD_GONE_FLASH.into());
        Dirty::YES
    }

    /// Only the focused session is drawn, so only it can owe a frame; focusing
    /// another is an event, and events always repaint. The rest get
    /// [`App::tick_background`].
    fn tick(&mut self) -> Dirty {
        let mut dirty = self.watch_cwd();
        // A quit ends the whole process, so each session's double press
        // counts the background tasks of all of them.
        let running: usize = self
            .sessions
            .iter()
            .map(|rt| rt.app.running_background_tasks().count())
            .sum();
        for rt in &mut self.sessions {
            rt.app.background_elsewhere = running - rt.app.running_background_tasks().count();
        }
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            if i == self.focused {
                dirty |= rt.app.tick();
            } else {
                rt.app.tick_background();
            }
        }
        // A plan form row outlives the key press that picked it: its handler
        // runs on the Lua thread, and the built-in outcome the row kept
        // follows only once that answers.
        for i in 0..self.sessions.len() {
            let actions = std::mem::take(&mut self.sessions[i].app.pending_actions);
            self.dispatch(i, actions);
        }
        self.replay_typeahead();
        dirty
    }

    fn handle_agent(&mut self, idx: usize, envelope: Box<maki_agent::Envelope>) {
        let rt = &mut self.sessions[idx];
        let current = is_current_top_level(rt.app.run_id, &envelope);
        match &envelope.event {
            AgentEvent::QueueDrained => {
                if current {
                    rt.notifications.on_drain();
                }
                return;
            }
            AgentEvent::QueueItemConsumed { .. } if current => {
                rt.notifications.on_queue_item_consumed();
                if rt.app.exits_on_done() {
                    rt.app.clear_exit_request();
                }
            }
            AgentEvent::TurnComplete(turn) if current => {
                rt.notifications.on_turn_complete(&turn.message);
            }
            event if current => rt.notifications.on_done(event),
            _ => {}
        }
        self.sessions[idx].app.update(Msg::Agent(envelope));
    }

    async fn drain_channels(&mut self) -> Result<Dirty> {
        let mut dirty = Dirty::NO;
        // Leftovers beyond the budget are picked up right after the next draw.
        for _ in 0..DRAIN_BUDGET {
            match self.pending_wake().await {
                Some(wake) => {
                    self.handle_wake(wake)?;
                    dirty = Dirty::YES;
                }
                None => break,
            }
        }

        // Only an announcer for Lua, not a second authority. Diffing is the
        // sturdiest way to fire the autocmd however the model got changed.
        for rt in &mut self.sessions {
            rt.app.emit_model_change();
        }

        // These only fire Lua autocmds. Anything a handler does comes back
        // as a `UiAction` on the next wake, which repaints then.
        self.emit_focus_changes();
        self.sessions[self.focused]
            .app
            .announce_mode(&mut self.last_mode);
        dirty |= self.start_woken_runs();
        self.emit_status_changes();
        self.emit_title_changes();
        for rt in &mut self.sessions {
            rt.app.settle_stranded_parts();
        }
        self.emit_task_changes();
        self.emit_notifications();
        self.emit_program_status();
        // An `exit_on_done` exit waits on `QueueDrained`; a dead agent loop
        // can never send it, so fail instead of hanging forever.
        if let Some(runtime) = self.sessions.iter().find(|rt| {
            rt.app.exit_request != ExitRequest::None
                && rt.notifications.waiting_for_drain()
                && rt.handles.is_finished()
                && rt.handles.agent_rx.is_empty()
        }) {
            return Err(eyre!(
                "agent for session {} stopped before queue drain",
                runtime.id()
            ));
        }
        Ok(dirty)
    }

    fn handle_ui_action(&mut self, action: UiAction) {
        match action {
            UiAction::Flash(msg) => {
                self.focused_app().flash(msg);
            }
            UiAction::SetWindowTitle(title) => {
                if let Err(error) = terminal::set_window_title(&title) {
                    warn!(%error, "failed to set window title");
                }
            }
            UiAction::OpenEditor { path, reply_tx } => {
                let code = self.open_editor(self.focused, &path);
                let _ = reply_tx.send(code);
            }
            UiAction::OpenWin {
                session,
                buf,
                config,
                focus,
                event_tx,
                cmd_rx,
            } => {
                let Some(idx) = self.tab_of(session) else {
                    debug!(session = ?session, "open_win for a session open in no tab, closed");
                    let _ = event_tx.send(WinEvent::Close);
                    return;
                };
                let app = &mut self.sessions[idx].app;
                app.float_mgr.open(buf, config, focus, event_tx, cmd_rx);
                if focus {
                    app.transition_plan(crate::app::mode::PlanTrigger::InteractivePrompt);
                }
            }
            UiAction::Session { req, reply_tx } => {
                self.handle_session_request(req, reply_tx);
            }
            UiAction::ExportSession { id, reply_tx } => match self.position(id) {
                Some(idx) => {
                    let _ = reply_tx.send(Ok(self.sessions[idx].app.export()));
                }
                None => self
                    .answer_from_disk(id, reply_tx, |session| Ok(SessionExport::stored(session))),
            },
            UiAction::Model { req, reply_tx } => self.handle_model_request(req, reply_tx),
            UiAction::ProviderUsage { slug, reply_tx } => {
                let slots = self
                    .sessions
                    .iter()
                    .map(|rt| Arc::clone(&rt.slot))
                    .collect();
                tokio::spawn(async move {
                    let _ = reply_tx.send(provider_usage_reply(slots, slug).await);
                });
            }
            UiAction::Input { req, reply_tx } => {
                let (reply, actions) = self.handle_input_request(req);
                let _ = reply_tx.send(reply);
                self.dispatch(self.focused, actions);
            }
            UiAction::Task { req, reply_tx } => {
                let _ = reply_tx.send(self.handle_task_request(req));
            }
            UiAction::Plan { req, reply_tx } => {
                let _ = reply_tx.send(self.handle_plan_request(req));
            }
            UiAction::WinSaveView { reply_tx } => {
                let _ = reply_tx.send(self.focused_app().win_view());
            }
            UiAction::WinRestView { scroll_top } => {
                self.focused_app().scroll_to_row(scroll_top);
            }
            // Answered before dispatching, which can hold the loop for as long
            // as `$EDITOR` is open.
            UiAction::Builtin {
                action,
                caller,
                reply_tx,
            } => {
                let (handled, actions) = self.focused_app().run_action(action, Some(&caller));
                let _ = reply_tx.send(handled);
                self.dispatch(self.focused, actions);
            }
            // Only the focused tab can be holding keys. A tab that loses focus
            // hands them over in `set_focused`.
            UiAction::KeybindDone(ticket) => {
                let actions = self.focused_app().keybind_done(ticket);
                self.dispatch(self.focused, actions);
                self.replay_typeahead();
            }
            UiAction::KeybindFallback(ticket) => {
                let actions = self.focused_app().keybind_fallback(ticket);
                self.dispatch(self.focused, actions);
            }
            UiAction::PluginUnloaded(plugin) => {
                for rt in &mut self.sessions {
                    rt.app.clear_plugin_marks(&plugin);
                }
            }
            UiAction::ChatItem { session, item } => {
                // A straggler after `/new` finds no tab and is dropped.
                if let Some(rt) = self.sessions.iter_mut().find(|rt| rt.id() == session) {
                    rt.app.handle_chat_item(&item);
                }
            }
            UiAction::RunCommand {
                cmdline,
                depth,
                caller,
                reply_tx,
            } => {
                // Answer before dispatching: the caller only waits on the name
                // resolving, and dispatch may take a while (or exit the app).
                match self.focused_app().run_cmdline(&cmdline, depth, &caller) {
                    Ok(actions) => {
                        let _ = reply_tx.send(Ok(()));
                        self.dispatch(self.focused, actions);
                    }
                    Err(e) => {
                        warn!(cmdline, depth, error = %e, "run_command refused");
                        let _ = reply_tx.send(Err(e));
                    }
                }
            }
            UiAction::PluginTaskEvent {
                session,
                plugin,
                envelope,
            } => {
                // A straggler after `/new` finds no tab and is dropped.
                if let Some(idx) = self.sessions.iter().position(|rt| rt.id() == session) {
                    self.sessions[idx].app.deliver(*envelope, Some(plugin));
                }
            }
            UiAction::PluginTaskOpened {
                session,
                plugin,
                info,
            } => {
                if let Some(rt) = self.sessions.iter_mut().find(|rt| rt.id() == session) {
                    rt.app.open_plugin_task(&info, plugin);
                }
            }
            UiAction::PluginTaskEnded { session, id, end } => {
                if let Some(rt) = self.sessions.iter_mut().find(|rt| rt.id() == session) {
                    rt.app.end_task(&id, end.into());
                }
            }
        }
    }

    /// Exits with the editor's status code; `-1` (flashed on the session's
    /// app) when the editor could not be launched.
    fn open_editor(&mut self, idx: usize, path: &std::path::Path) -> i32 {
        let result = {
            let _pause = self.input.pause();
            terminal::open_in_editor(path, self.terminal)
        };
        self.focus.on_resume();
        match result {
            Ok(code) => code,
            Err(e) => {
                self.sessions[idx].app.flash(e);
                -1
            }
        }
    }

    fn emit_status_changes(&mut self) {
        let handle = &self.ctx.lua_event_handle;
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            let status = SessionStatus::of(&rt.app);
            if status == rt.last_status {
                continue;
            }
            rt.last_status = status;
            handle.fire(
                Some(&rt.id()),
                HostEvent::SessionStatusChanged {
                    title: &rt.app.state.session.title,
                    status: status.as_str(),
                    focused: i == self.focused,
                },
            );
        }
    }

    /// `/rename` and title auto-generation change the title without touching
    /// the status diff above, so the title gets its own diff per frame.
    fn emit_title_changes(&mut self) {
        let handle = &self.ctx.lua_event_handle;
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            if rt.app.state.session.title == rt.last_title {
                continue;
            }
            rt.last_title = rt.app.state.session.title.clone();
            handle.fire(
                Some(&rt.id()),
                HostEvent::SessionTitleChanged {
                    title: &rt.last_title,
                    focused: i == self.focused,
                },
            );
        }
    }

    /// One diff per frame covers every path that finishes, cancels or errors a
    /// chat, so none of them has to remember to fire an event.
    fn emit_task_changes(&mut self) {
        let handle = &self.ctx.lua_event_handle;
        for rt in &mut self.sessions {
            let session_id = rt.app.state.session.id;
            rt.last_tasks
                .diff(session_id, rt.app.task_states(), |task| {
                    handle.fire(
                        Some(&session_id),
                        HostEvent::TaskStatusChanged {
                            id: task.id,
                            name: task.name,
                            status: task.status.into(),
                            plugin: task.plugin.map(|plugin| &**plugin),
                        },
                    );
                });
        }
    }

    /// Called only after the cancel went out, so a handler that checks
    /// `maki.agent.list()` already finds a stopped plugin task gone.
    fn emit_task_stopped(&self, idx: usize, id: &str, plugin: Option<&str>) {
        self.ctx.lua_event_handle.fire(
            Some(&self.sessions[idx].app.state.session.id),
            HostEvent::TaskStopped { id, plugin },
        );
    }

    fn emit_notifications(&mut self) {
        let Some(notifier) = &self.notifier else {
            return;
        };
        let mut selected = None;
        for rt in &mut self.sessions {
            let candidate = rt
                .notifications
                .reconcile(rt.app.attention(), SessionStatus::of(&rt.app));
            selected = select_notification(selected, candidate);
        }
        if let Some(notification) = selected.filter(|n| self.focus.allows(n))
            && let Err(error) = notifier.notify(&notification.message(), notification.reason())
        {
            warn!(notifier = ?notifier.notifier(), %error, "terminal notifications disabled after write failure");
            self.notifier = None;
        }
    }

    fn clear_program_status(&mut self) {
        if let Some(reporter) = &mut self.program_status
            && let Err(error) = reporter.clear()
        {
            warn!(%error, "failed to clear program status");
        }
    }

    fn emit_program_status(&mut self) {
        let Some(reporter) = &mut self.program_status else {
            return;
        };
        let focused = self.focused;
        let candidates = self.sessions.iter().enumerate().map(|(i, rt)| {
            let (state, msg) = rt.program_state();
            (
                state,
                i == focused,
                (rt.app.state.session.title.as_str(), msg),
            )
        });
        let Some((state, (title, msg))) = select_program_status(candidates) else {
            return;
        };
        if let Err(error) = reporter.report(state, title, msg.as_deref()) {
            warn!(%error, "program status reporting disabled after write failure");
            self.program_status = None;
        }
    }

    /// One diff per frame covers the session picker, the chat cycling keys
    /// and `maki.task.focus`, so none of them has to remember to fire an
    /// event. A session switch is a task switch too, so `TaskFocusChanged`
    /// always follows `SessionFocusChanged`.
    ///
    /// A switch also republishes the input the newly focused tab holds. Only
    /// the focused session ticks, so a background tab never diffs its own
    /// input, and without the announcement a handler keeps acting on the text
    /// of the tab it came from.
    fn emit_focus_changes(&mut self) {
        let rt = &self.sessions[self.focused];
        let current = (rt.id(), rt.app.active_task_id());
        if self.last_focus.as_ref() == Some(&current) {
            return;
        }
        let previous_session = self.last_focus.replace(current.clone()).map(|(id, _)| id);
        let (session_id, task_id) = current;
        let switched = previous_session.is_some() && previous_session != Some(session_id);
        let eh = &self.ctx.lua_event_handle;
        if previous_session != Some(session_id) {
            eh.fire(
                Some(&session_id),
                HostEvent::SessionFocusChanged {
                    previous_session_id: previous_session.map(|id| id.to_string()),
                },
            );
        }
        eh.fire(
            Some(&session_id),
            HostEvent::TaskFocusChanged { id: &task_id },
        );
        if switched {
            self.focused_app().announce_input();
        }
    }

    fn start_woken_runs(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        for index in 0..self.sessions.len() {
            if let Some(request) = self.sessions[index].app.take_requested_compact() {
                self.sessions[index].app.mark_compacting();
                self.queue_compact(index, request.instructions, CompactReason::Requested);
                dirty = Dirty::YES;
            }
            let actions = self.sessions[index].app.start_woken_run();
            dirty |= Dirty::from(!actions.is_empty());
            self.dispatch(index, actions);
        }
        dirty
    }

    /// Pushes a compaction to the queue, so a mid-run request lands at the
    /// run's next boundary instead of racing its history.
    fn queue_compact(&mut self, idx: usize, instructions: Option<String>, reason: CompactReason) {
        let rt = &mut self.sessions[idx];
        rt.reset_run_notifications();
        rt.handles.queue.push(QueueItem::Compact(Compaction {
            run_id: rt.app.run_id,
            instructions,
            reason,
            workflow: rt.app.state.workflow,
        }));
    }

    /// `List` replies from a background task (the scan can be slow); every
    /// other request is answered synchronously by the event loop, which owns
    /// the live runtimes.
    fn handle_session_request(&mut self, req: SessionRequest, reply_tx: oneshot::Sender<UiReply>) {
        match req {
            SessionRequest::List => {
                let storage = self.ctx.storage.clone();
                spawn_blocking(move || {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let reply = AppSession::list(&cwd.to_string_lossy(), &storage)
                        .map_err(|e| e.to_string())
                        .and_then(|list| serde_json::to_value(list).map_err(|e| e.to_string()));
                    let _ = reply_tx.send(reply);
                });
            }
            // Deletes run on the storage writer thread after any queued
            // flushes, so the loop never blocks on disk and a queued save
            // cannot resurrect the files.
            SessionRequest::Delete { id } => {
                let id = match parse_session_id(&id) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = reply_tx.send(Err(e));
                        return;
                    }
                };
                let mut claim = None;
                if let Some(i) = self.position(id) {
                    if i == self.focused {
                        let _ = reply_tx.send(Err(DELETE_FOCUSED_ERR.into()));
                        return;
                    }
                    let rt = self.remove_runtime(i);
                    self.ctx
                        .lua_event_handle
                        .end_session(rt.id(), SessionEndReason::Delete);
                    rt.handles.cancel_all();
                    claim = Some(rt.app.state.claim.clone());
                }
                self.ctx.storage_writer.delete(id, claim, move |res| {
                    let reply = match res {
                        Ok(()) => Ok(json!(true)),
                        Err(e) if e.is_not_found() => Ok(json!(true)),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = reply_tx.send(reply);
                });
            }
            SessionRequest::Live => {
                let list: Vec<_> = self
                    .sessions
                    .iter()
                    .enumerate()
                    .map(|(i, rt)| {
                        json!({
                            "id": rt.id(),
                            "title": rt.app.state.session.title,
                            "status": SessionStatus::of(&rt.app).as_str(),
                            "updated_at": rt.app.state.session.updated_at,
                            "focused": i == self.focused,
                        })
                    })
                    .collect();
                let _ = reply_tx.send(Ok(json!(list)));
            }
            SessionRequest::Current => {
                let _ = reply_tx.send(Ok(json!(self.sessions[self.focused].id())));
            }
            // A stored session is read from disk, off the loop like `List`.
            SessionRequest::Read { id } => {
                let id = match id.as_deref().map(parse_session_id).transpose() {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = reply_tx.send(Err(e));
                        return;
                    }
                };
                let live = match id {
                    None => Ok(self.focused),
                    Some(id) => self.position(id).ok_or(id),
                };
                match live {
                    Ok(idx) => {
                        let _ = reply_tx.send(Ok(self.session_snapshot_json(idx)));
                    }
                    Err(id) => self.answer_from_disk(id, reply_tx, |session| {
                        serde_json::to_value(SessionSnapshot::stored(&session))
                            .map_err(|e| e.to_string())
                    }),
                }
            }
            SessionRequest::New {
                transcript,
                prompt,
                focus,
            } => self.create_session(*transcript, prompt, focus, reply_tx),
            SessionRequest::Prompt { id, text } => {
                let idx = match id {
                    None => Ok(self.focused),
                    Some(id) => parse_session_id(&id).and_then(|id| {
                        self.position(id)
                            .ok_or_else(|| format!("{NOT_LIVE_ERR}: {id}"))
                    }),
                };
                let _ = reply_tx.send(idx.and_then(|idx| self.submit_text(idx, text)));
            }
            SessionRequest::Focus { id } => match parse_session_id(&id) {
                Ok(id) => self.focus_session(id, reply_tx),
                Err(e) => {
                    let _ = reply_tx.send(Err(e));
                }
            },
            // Resolved by id like the rest: a plan form row asking for build
            // mode fires for the session whose plan landed, which may be in
            // the background.
            SessionRequest::SetMode { id, mode } => {
                let reply = parse_mode(&mode).and_then(|mode| {
                    let idx = self.resolve_session_index(id.as_deref())?;
                    self.sessions[idx].app.set_mode(mode);
                    Ok(json!(true))
                });
                let _ = reply_tx.send(reply);
            }
            SessionRequest::SetTitle { id, title } => {
                let title = normalize_title(&title);
                let id = match parse_session_id(&id) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = reply_tx.send(Err(e));
                        return;
                    }
                };
                if let Some(i) = self.position(id) {
                    self.sessions[i].app.state.session_mut().set_title(title);
                    let _ = reply_tx.send(Ok(json!(true)));
                    return;
                }
                // No tab has it, so the claim taken here is the only writer.
                let storage = self.ctx.storage.clone();
                self.stored_writes.spawn_blocking(move || {
                    let reply = OpenSession::load(id, &storage)
                        .and_then(|OpenSession { mut session, claim }| {
                            session.set_title(title);
                            claim.persist(&session)
                        })
                        .map(|()| json!(true))
                        .map_err(|e| e.to_string());
                    let _ = reply_tx.send(reply);
                });
            }
        }
    }

    /// The input a plugin reads and writes is the focused session's.
    ///
    /// An edit or a submit is answered against the terminal as it is now, not
    /// as the last frame found it: a batch of wakes runs between two paints,
    /// so the size the box would be drawn at has to be asked for here.
    fn handle_input_request(&mut self, req: InputRequest) -> (UiReply, Vec<Action>) {
        let reply = match req {
            InputRequest::Read => Ok(self.focused_app().input_snapshot()),
            InputRequest::Edit(edit) => {
                let area = self.terminal_area();
                self.focused_app().apply_input_edit(edit, area)
            }
            InputRequest::Decorate(req) => self.focused_app().decorate_input(req),
            InputRequest::Submit(req) => {
                let area = self.terminal_area();
                let version = req.version;
                return match self.focused_app().submit_input(req, area) {
                    Ok(actions) => (Ok(json!(true)), actions),
                    Err(err) => {
                        info!(version, error = %err, "input_submit refused");
                        (Err(err), Vec::new())
                    }
                };
            }
        };
        (reply, Vec::new())
    }

    fn terminal_area(&self) -> Rect {
        self.terminal.size().map(Rect::from).unwrap_or_default()
    }

    /// Lua acts on the focused session, the same target the model picker
    /// writes to.
    fn handle_model_request(&mut self, req: ModelRequest, reply_tx: oneshot::Sender<UiReply>) {
        let reply = match req {
            ModelRequest::Get => Ok(self.focused_app().model_state()),
            ModelRequest::Available => {
                let available = self.ctx.available_models.load();
                Ok(json!(
                    available
                        .as_deref()
                        .map(|list| list.specs.as_slice())
                        .unwrap_or(&[])
                ))
            }
            ModelRequest::Set {
                spec: Some(spec),
                thinking,
                fast,
            } => {
                let origin = PickOrigin::Lua {
                    thinking,
                    fast,
                    reply_tx,
                };
                return self.change_model(spec, origin);
            }
            ModelRequest::Set {
                spec: None,
                thinking,
                fast,
            } => apply_settings(self.focused_app(), thinking, fast),
        };
        let _ = reply_tx.send(reply);
    }

    /// Route a plan-surface request to the session it names, or the focused
    /// one when it names none. Plan state is per session, so a plugin woken
    /// by a background tab's `PlanReady` would otherwise read the wrong plan.
    fn handle_plan_request(&mut self, req: PlanRequest) -> UiReply {
        let idx = self.resolve_session_index(req.session())?;
        match req {
            PlanRequest::Read { .. } => Ok(self.sessions[idx].app.plan_snapshot()),
        }
    }

    fn handle_task_request(&mut self, req: TaskRequest) -> UiReply {
        match req {
            TaskRequest::List => Ok(json!(self.focused_app().tasks())),
            TaskRequest::Focus { id } => self.focused_app().focus_task(&id).map(|()| json!(true)),
            TaskRequest::Remove { id } => self.focused_app().remove_task(&id).map(|()| json!(true)),
            TaskRequest::Stop { id, session } => {
                let action = self.focused_app().stop_task_in_session(&id, &session)?;
                self.dispatch(self.focused, vec![action]);
                Ok(json!(true))
            }
        }
    }

    fn submit_text(&mut self, idx: usize, text: String) -> UiReply {
        let msg = QueuedMessage {
            text,
            images: Vec::new(),
        };
        match self.sessions[idx].app.submit_prompt(msg) {
            SubmitOutcome::Started(actions) => {
                self.dispatch(idx, actions);
                Ok(json!("started"))
            }
            SubmitOutcome::Queued => Ok(json!("queued")),
            SubmitOutcome::Rejected(e) => Err(e.into()),
        }
    }

    /// Without `prompt` or `focus` the session is only stored, and the reply
    /// waits for the write. Otherwise it opens in a tab, which saves it on its
    /// first checkpoint.
    fn create_session(
        &mut self,
        transcript: Transcript,
        prompt: Option<String>,
        focus: bool,
        reply_tx: oneshot::Sender<UiReply>,
    ) {
        let inherits_model = transcript.model.is_none();
        let open = match self.sessions[self.focused].app.session_from(transcript) {
            Ok(open) => open,
            Err(e) => {
                let _ = reply_tx.send(Err(e));
                return;
            }
        };
        if prompt.is_none() && !focus {
            let storage = self.ctx.storage.clone();
            self.stored_writes.spawn_blocking(move || {
                let reply = open.store(&storage).map(|id| json!(id));
                let _ = reply_tx.send(reply.map_err(|e| e.to_string()));
            });
            return;
        }
        let created = CreatedSession {
            open,
            prompt,
            focus,
            reply_tx,
        };
        // An inherited model's slot is already built and sitting right here.
        if inherits_model {
            let slot = self.sessions[self.focused].slot.load_full();
            self.show_created(created, slot, None);
        } else {
            let spec = created.open.session.model.clone();
            self.wait_for_slot(spec, Wanted::Create(created));
        }
    }

    fn show_created(
        &mut self,
        created: CreatedSession,
        slot: Arc<ModelSlot>,
        fallback: Option<String>,
    ) {
        let CreatedSession {
            open,
            prompt,
            focus,
            reply_tx,
        } = created;
        let idx = self.push_runtime(self.ctx.spawn_runtime(open, slot));
        let id = self.sessions[idx].id();
        maki_otel::emit::session_started(maki_otel::emit::START_FRESH, Some(&id.to_string()));
        if let Some(reason) = fallback {
            self.sessions[idx].app.flash(reason);
        }
        if let Some(prompt) = prompt {
            let _ = self.submit_text(idx, prompt);
        }
        if focus {
            self.set_focused(idx);
        }
        let _ = reply_tx.send(Ok(json!(id)));
    }

    /// Reads a session no tab has open off the loop, without claiming it, so
    /// one another maki holds still answers.
    fn answer_from_disk<T: Send + 'static>(
        &self,
        id: MakiId,
        reply_tx: oneshot::Sender<Result<T, String>>,
        answer: impl FnOnce(AppSession) -> Result<T, String> + Send + 'static,
    ) {
        let storage = self.ctx.storage.clone();
        spawn_blocking(move || {
            let reply = AppSession::read_only(id, &storage)
                .map_err(|e| e.to_string())
                .and_then(answer);
            let _ = reply_tx.send(reply);
        });
    }

    fn position(&self, id: MakiId) -> Option<usize> {
        self.sessions.iter().position(|rt| rt.id() == id)
    }

    /// No session means the focused tab. A session that is open in no tab
    /// has none, and the request should not land on some other tab instead.
    fn tab_of(&self, session: Option<MakiId>) -> Option<usize> {
        match session {
            None => Some(self.focused),
            Some(id) => self.position(id),
        }
    }

    /// A plugin holding the id of a tab that has since closed gets
    /// `session not live` back, so it knows to stop.
    fn resolve_session_index(&self, id: Option<&str>) -> Result<usize, String> {
        let session = id.map(parse_session_id).transpose()?;
        self.tab_of(session).ok_or_else(|| NOT_LIVE_ERR.into())
    }

    /// The totals live on the session, so a plugin that reloads mid run keeps
    /// the accounting it would lose by summing `TurnEnd` payloads itself.
    fn session_snapshot_json(&self, idx: usize) -> serde_json::Value {
        let rt = &self.sessions[idx];
        let app = &rt.app;
        let snapshot = SessionSnapshot {
            id: rt.id().to_string(),
            cwd: app.state.session.cwd.clone(),
            title: Some(app.state.session.title.clone()),
            model: app.state.model.spec(),
            mode: if app.state.mode == Mode::Plan {
                MODE_PLAN
            } else {
                MODE_BUILD
            },
            thinking: Some(app.state.thinking.to_string()),
            fast: Some(app.state.fast),
            status: SessionStatus::of(app).as_str(),
            focused: idx == self.focused,
            updated_at: app.state.session.updated_at,
            queue: Some(SessionQueueSnapshot {
                count: app.queue.text_messages().len(),
            }),
            usage: app.state.token_usage,
            context_size: app.state.context_size,
            context_window: Some(app.state.model.context_window),
            cost: app.state.cost,
        };
        json!(snapshot)
    }

    /// The single place that removes a runtime: keeps `focused` pointing at
    /// the same session afterwards. The focused runtime itself is never
    /// removable, so `sessions` stays non-empty.
    fn remove_runtime(&mut self, idx: usize) -> SessionRuntime {
        debug_assert_ne!(idx, self.focused);
        let rt = self.sessions.remove(idx);
        if idx < self.focused {
            self.focused -= 1;
        }
        rt
    }

    /// The single place the focused tab changes. Keys held behind a key
    /// handler were typed at the keyboard, so they follow it to the new tab
    /// rather than replay into the one left behind. They keep waiting there:
    /// the handler that switched is usually not done, and its `KeybindDone`
    /// goes to the focused tab.
    fn set_focused(&mut self, idx: usize) {
        if idx == self.focused {
            return;
        }
        let left = mem::replace(&mut self.focused, idx);
        if let Ok([from, to]) = self.sessions.get_disjoint_mut([left, idx]) {
            from.app.hand_typeahead_to(&mut to.app);
        }
    }

    fn push_runtime(&mut self, rt: SessionRuntime) -> usize {
        self.sessions.push(rt);
        self.sessions.len() - 1
    }

    /// Focus a live session, or read a stored one off the loop and bring it up
    /// once [`Wake::SessionOpened`] lands, see [`Self::show_opened`].
    fn focus_session(&mut self, id: MakiId, reply_tx: oneshot::Sender<UiReply>) {
        if self.opening.is_some() || self.waiting.opens_a_session() {
            self.queued_focus.push_back((id, reply_tx));
            return;
        }
        if let Some(i) = self.position(id) {
            self.set_focused(i);
            let _ = reply_tx.send(Ok(json!(true)));
            return;
        }
        let focus = PendingFocus {
            reply_tx,
            from: self.sessions[self.focused].id(),
        };
        let storage = self.ctx.storage.clone();
        self.opening = Some(AbortOnDropHandle::new(tokio::spawn(async move {
            // A session another maki owns is refused here, where it costs a
            // flash, rather than at the first write, after a whole conversation.
            let opened = OpenSession::load_async(id, storage)
                .await
                .map_err(|e| format!("Failed to load session: {e}"));
            Box::new(OpenedSession(focus, opened))
        })));
    }

    fn finish_open(&mut self, answer: Option<Box<OpenedSession>>) {
        match answer.map(|answer| *answer) {
            Some(OpenedSession(focus, Ok(open))) => {
                let spec = open.session.model.clone();
                return self.wait_for_slot(spec, Wanted::Open(focus, open));
            }
            Some(OpenedSession(focus, Err(e))) => {
                let _ = focus.reply_tx.send(Err(e));
            }
            None => {}
        }
        self.next_focus();
    }

    /// The focus requests that queued behind a read or a build go next, up to
    /// the next one that has to wait for one too.
    fn next_focus(&mut self) {
        while self.opening.is_none()
            && !self.waiting.opens_a_session()
            && let Some((id, reply_tx)) = self.queued_focus.pop_front()
        {
            self.focus_session(id, reply_tx);
        }
    }

    fn wait_for_slot(&mut self, spec: String, wanted: Wanted) {
        self.waiting.push(&mut self.slots, spec, wanted);
        self.settle_waiters();
    }

    /// A slot that landed goes to whoever waits for it and to every tab on
    /// its spec, which is how a rebuild reaches them. A tab that picked
    /// another model in the meantime is on another spec and stays.
    fn land_slot(&mut self, landed: Landed) {
        let Some((spec, built)) = self.slots.land(landed) else {
            return;
        };
        let on_spec = |rt: &SessionRuntime| rt.app.state.model.spec() == spec;
        match &built {
            Ok(slot) => {
                for idx in 0..self.sessions.len() {
                    let rt = &self.sessions[idx];
                    if on_spec(rt) && !Arc::ptr_eq(&rt.slot.load(), slot) {
                        self.apply_model(idx, Arc::clone(slot));
                    }
                }
            }
            Err(error) if self.sessions.iter().any(on_spec) => {
                warn!(%spec, %error, "keeping the previous model");
            }
            Err(_) => {}
        }
        self.waiting.landed(&spec, &built);
        self.settle_waiters();
    }

    fn settle_waiters(&mut self) {
        for (Waiter { spec, wanted, .. }, built) in self.waiting.take_ready() {
            match (wanted, built) {
                (Wanted::Pick(pick), Ok(slot)) => self.apply_pick(pick, spec, slot),
                (Wanted::Pick(pick), Err(error)) => self.refuse_pick(pick.origin, error),
                (Wanted::Open(focus, open), built) => {
                    let (slot, fallback) = self.session_slot(&spec, built);
                    self.show_opened(open, focus, slot, fallback);
                }
                (Wanted::Create(created), built) => {
                    let (slot, fallback) = self.session_slot(&spec, built);
                    self.show_created(created, slot, fallback);
                }
            }
        }
        for rt in &mut self.sessions {
            rt.app.picking = self.waiting.pending_pick(&rt.slot).map(str::to_owned);
        }
        self.next_focus();
    }

    /// A session always comes up, on the model the process started on when
    /// its own would not build.
    fn session_slot(
        &self,
        spec: &str,
        built: Result<Arc<ModelSlot>, String>,
    ) -> (Arc<ModelSlot>, Option<String>) {
        built.map_or_else(
            |reason| self.slots.fall_back(spec, reason),
            |slot| (slot, None),
        )
    }

    /// In place of the tab the focus was asked from, while it is still focused
    /// and a blank idle one (nothing worth keeping), otherwise as a new runtime
    /// so the session you came from stays live. Decided now rather than at the
    /// request, since the read and the build may have taken a while.
    fn show_opened(
        &mut self,
        open: OpenSession,
        focus: PendingFocus,
        slot: Arc<ModelSlot>,
        fallback: Option<String>,
    ) {
        let focused = &self.sessions[self.focused];
        if focused.id() == focus.from
            && SessionStatus::of(&focused.app) == SessionStatus::Idle
            && focused.app.is_blank()
        {
            let idx = self.focused;
            let history = self.sessions[idx]
                .app
                .apply_loaded_session(open, &slot.model);
            self.apply_model(idx, slot);
            self.respawn_agent(idx, history, Respawn::Load);
        } else {
            let idx = self.push_runtime(self.ctx.spawn_runtime(open, slot));
            self.set_focused(idx);
        }
        if let Some(reason) = fallback {
            self.focused_app().flash(reason);
        }
        let _ = focus.reply_tx.send(Ok(json!(true)));
    }

    /// Handles one input event plus any leftover produced while coalescing
    /// bursts of scroll/drag events.
    fn handle_input(&mut self, raw: Event) {
        let mut pending = Some(raw);
        while let Some(ev) = pending.take() {
            let (msg, leftover) = self.translate(ev);
            if let Some(msg) = msg {
                let actions = self.sessions[self.focused].app.update(msg);
                self.dispatch(self.focused, actions);
            }
            pending = leftover;
        }
    }

    fn translate(&mut self, raw: Event) -> (Option<Msg>, Option<Event>) {
        match raw {
            Event::FocusGained => {
                self.focus.report(Focus::Focused);
                (None, None)
            }
            Event::FocusLost => {
                self.focus.report(Focus::Unfocused);
                (None, None)
            }
            // The one place the host's view of a keypress is normalized, so
            // it can never disagree with the identity a plugin is handed. A
            // key no notation names passes through untouched: nothing to
            // normalize, and `text_buffer` still reads its SUPER bit.
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.focus.note_input();
                (
                    Some(Msg::Key(Key::from_event(key).map_or(key, KeyEvent::from))),
                    None,
                )
            }
            Event::Key(_) => (None, None),
            Event::Paste(text) => {
                self.focus.note_input();
                (Some(Msg::Paste(text)), None)
            }
            Event::Mouse(mouse) => {
                self.focus.note_input();
                self.translate_mouse(mouse)
            }
            _ => (None, None),
        }
    }

    fn translate_mouse(&mut self, mouse: CtMouseEvent) -> (Option<Msg>, Option<Event>) {
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let scroll_lines = self.focused_app().ui_config.mouse_scroll_lines;
                let (msg, leftover) = self.aggregate_scroll(mouse, scroll_lines);
                (Some(msg), leftover)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (drag, leftover) = self.coalesce_drag(mouse);
                (Some(Msg::Mouse(drag)), leftover)
            }
            _ => (Some(Msg::Mouse(mouse)), None),
        }
    }

    /// Sums queued scroll events into one delta; the first non-scroll event
    /// drained along the way is returned so it isn't lost.
    fn aggregate_scroll(&self, first: CtMouseEvent, scroll_lines: u32) -> (Msg, Option<Event>) {
        let mut delta = scroll_delta(first.kind, scroll_lines);
        let mut leftover = None;
        while let Some(next) = self.input.try_next() {
            match next {
                Event::Mouse(m)
                    if matches!(
                        m.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    delta += scroll_delta(m.kind, scroll_lines);
                }
                other => {
                    leftover = Some(other);
                    break;
                }
            }
        }
        (
            Msg::Scroll {
                column: first.column,
                row: first.row,
                delta,
            },
            leftover,
        )
    }

    /// Keeps only the newest queued drag position; the first non-drag event
    /// drained along the way is returned so it isn't lost.
    fn coalesce_drag(&self, mut latest: CtMouseEvent) -> (CtMouseEvent, Option<Event>) {
        let mut leftover = None;
        while let Some(next) = self.input.try_next() {
            match next {
                Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left)) => {
                    latest = m;
                }
                other => {
                    leftover = Some(other);
                    break;
                }
            }
        }
        (latest, leftover)
    }

    /// Held keys replay up to the next one with actions, and those run before
    /// the keys after it replay, in whichever tab has the keyboard by then.
    /// So the keys typed after `/fork` land in the fork. Every round replays
    /// at least one key, so this ends.
    fn replay_typeahead(&mut self) {
        loop {
            let actions = self.focused_app().replay_typeahead();
            if actions.is_empty() {
                return;
            }
            self.dispatch(self.focused, actions);
        }
    }

    fn dispatch(&mut self, idx: usize, actions: Vec<Action>) {
        for action in actions {
            self.handle_action(idx, action);
        }
    }

    /// The only writer of a runtime's model, so the cell the agent reads, the
    /// app that draws and persists it and the per provider usage panel all move
    /// together. Spread over callers, one of them always forgets a piece.
    fn apply_model(&mut self, idx: usize, slot: Arc<ModelSlot>) {
        let rt = &mut self.sessions[idx];
        rt.app.state.update_model(&slot.model);
        rt.app.usage_slot.store(None);
        rt.slot.store(slot);
    }

    /// Discovery landed or a provider was re-authenticated. Invalidating alone
    /// would leave the tabs on stale providers, so every session's spec builds
    /// again, one build per spec, and [`Self::land_slot`] moves the tabs over.
    fn rebuild_models(&mut self) {
        self.slots.invalidate();
        for rt in &self.sessions {
            self.slots.request(&rt.app.state.model.spec());
        }
        self.waiting.restart(&mut self.slots);
    }

    fn respawn_agent(&mut self, idx: usize, history: Vec<Message>, reason: Respawn) {
        let rt = &mut self.sessions[idx];
        rt.reset_run_notifications();
        rt.handles.respawn(
            history,
            &rt.slot,
            self.ctx.config.clone(),
            self.ctx.ui_config.tool_output_lines,
            &mut rt.app,
            reason,
        );
    }

    /// The fork runs on the model of the session it came from, whose slot is
    /// already built.
    fn open_fork(&mut self, idx: usize, open: OpenSession) {
        let slot = self.sessions[idx].slot.load_full();
        let fork = self.push_runtime(self.ctx.spawn_runtime(open, slot));
        self.set_focused(fork);
        let id = self.sessions[self.focused].id();
        maki_otel::emit::session_started(maki_otel::emit::START_FRESH, Some(&id.to_string()));
        self.focused_app()
            .flash(FORKED_MSG.replace("{key}", key::SESSIONS.label));
    }

    fn handle_action(&mut self, idx: usize, action: Action) {
        match action {
            Action::SendMessage(input) => {
                let rt = &mut self.sessions[idx];
                rt.reset_run_notifications();
                let mut input = *input;
                prepend_preamble(&mut input.preamble, rt.app.shell.drain_results());
                let run_id = rt.app.run_id;
                rt.handles.queue.push(QueueItem::Message(QueuedInput {
                    text: input.message.clone(),
                    input,
                    run_id,
                    displayed: true,
                }));
            }
            Action::CancelAgent { run_id } => {
                let rt = &mut self.sessions[idx];
                rt.notifications.reset();
                rt.handles.cancel_run(run_id);
            }
            Action::CancelSubagent { tool_use_id } => {
                self.sessions[idx]
                    .handles
                    .cancel_subagent(tool_use_id.clone());
                self.emit_task_stopped(idx, &tool_use_id, None);
            }
            Action::CancelPluginTask { id, plugin } => {
                self.ctx.lua_event_handle.cancel_plugin_session(&id);
                self.emit_task_stopped(idx, &id, Some(&plugin));
            }
            Action::RestartAgent(history) => self.respawn_agent(idx, history, Respawn::Restart),
            Action::OpenFork(open) => self.open_fork(idx, *open),
            Action::ChangeModel(spec) => self.change_model(spec, PickOrigin::Picker),
            Action::RefreshProvider { slug } => self.refresh_provider(slug),
            Action::AssignTier(spec, tier) => {
                self.persist_tiers(model_registry::set(spec, tier));
            }
            Action::UnassignTier(spec, tier) => {
                self.persist_tiers(model_registry::unset(&spec, tier));
            }
            Action::Compact(instructions) => {
                self.queue_compact(idx, instructions, CompactReason::Manual);
            }
            Action::ToggleMcp(server_name, enabled) => {
                self.sessions[idx].handles.send_mcp(McpCommand::Toggle {
                    server: server_name,
                    enabled,
                });
            }
            Action::ShellCommand {
                id,
                command,
                visible,
            } => {
                let rt = &mut self.sessions[idx];
                let (trigger, cancel) = CancelToken::new();
                rt.app.shell.add_trigger(trigger);
                spawn_shell(
                    command,
                    id,
                    visible,
                    rt.shell_tx.clone(),
                    cancel,
                    self.ctx.config.clone(),
                );
            }
            Action::OpenEditor(path) => {
                self.open_editor(idx, &path);
            }
            Action::OpenUrl(url) => {
                spawn_blocking(move || {
                    if let Err(e) = open::that_detached(&url) {
                        warn!(error = %e, url, "failed to open browser");
                    }
                });
            }
            Action::EditInputInEditor => {
                let current_text = self.sessions[idx].app.input_box.buffer.value();
                let result = {
                    let _pause = self.input.pause();
                    terminal::edit_temp_content(current_text, self.terminal)
                };
                self.focus.on_resume();
                match result {
                    Ok(edited) if edited == current_text => {}
                    Ok(edited) => self.sessions[idx].app.input_box.rewrite_input(&edited),
                    Err(e) => self.sessions[idx].app.flash(e),
                }
            }
            Action::Btw(question) => {
                let slot = self.sessions[idx].slot.load_full();
                self.sessions[idx].app.start_btw(
                    question,
                    Arc::clone(&slot.provider),
                    slot.model.clone(),
                );
            }
            Action::PreparePack(command) => self.start_pack(idx, command),
            Action::Suspend => {
                self.clear_program_status();
                let _pause = self.input.pause();
                terminal::suspend(self.terminal);
                self.focus.on_resume();
            }
            Action::RefreshModels => self.refresh_models(),
            Action::RefreshUsage => self.refresh_usage(),
            Action::ManualExit => self.sessions[idx].notifications.on_manual_exit(),
        }
    }

    fn change_model(&mut self, spec: String, origin: PickOrigin) {
        let tab = Arc::downgrade(&self.sessions[self.focused].slot);
        self.wait_for_slot(spec, Wanted::Pick(Pick { tab, origin }));
    }

    /// The one explicit pick, which is why it alone writes the last-used file.
    /// Opening an old session must not change what the next `maki` starts on.
    fn apply_pick(&mut self, pick: Pick, spec: String, slot: Arc<ModelSlot>) {
        let Some(idx) = self
            .sessions
            .iter()
            .position(|rt| Weak::ptr_eq(&Arc::downgrade(&rt.slot), &pick.tab))
        else {
            return self.refuse_pick(pick.origin, NOT_LIVE_ERR.to_owned());
        };
        let picked = slot.model.spec();
        self.apply_model(idx, slot);
        self.ctx
            .picker_memory
            .record_recent(&spec, &self.ctx.model_policy);
        self.ctx.storage_writer.persist(move |dir| {
            persist_model(dir, &picked);
            push_recent(dir, &spec);
        });
        if let PickOrigin::Lua {
            thinking,
            fast,
            reply_tx,
        } = pick.origin
        {
            let _ = reply_tx.send(apply_settings(&mut self.sessions[idx].app, thinking, fast));
        }
    }

    fn refuse_pick(&mut self, origin: PickOrigin, error: String) {
        match origin {
            PickOrigin::Picker => self.focused_app().flash(error),
            PickOrigin::Lua { reply_tx, .. } => {
                let _ = reply_tx.send(Err(error));
            }
        }
    }

    /// Prepares a package command as a task.
    ///
    /// Preparation fetches over the network, and inline it would freeze drawing
    /// and input for as long as the slowest remote takes, so the answer comes
    /// back as a wake instead.
    fn start_pack(&mut self, idx: usize, command: PackCommand) {
        if self.pack.is_some() {
            self.sessions[idx].app.flash(PACK_BUSY_ERR.to_owned());
            return;
        }
        self.sessions[idx].app.flash(PACK_PREPARING.to_owned());
        let handle = self.ctx.lua_event_handle.clone();
        self.pack = Some(tokio::spawn(async move {
            match handle.package_context().await {
                Ok(context) => maki_lua::prepare_pack_command(&command, &context).await,
                Err(error) => PackPreparation::failed(error),
            }
        }));
    }

    /// The answer lands on the focused session, not on the one that asked: a
    /// package command reloads the whole Lua host, and only the focused session
    /// is read for an exit request or seen by whoever answers the review.
    fn finish_pack(&mut self, preparation: PackPreparation) {
        let actions = self.focused_app().handle_pack_preparation(preparation);
        self.dispatch(self.focused, actions);
    }

    fn refresh_models(&self) {
        fetch_models(
            Arc::clone(&self.ctx.available_models),
            Arc::clone(&self.ctx.model_policy),
            self.warn_tx.clone(),
            Arc::clone(&self.models_fetched),
        );
    }

    fn refresh_usage(&mut self) {
        let rt = &self.sessions[self.focused];
        let provider = Arc::clone(&rt.slot.load().provider);
        let slot = Arc::clone(&rt.app.usage_slot);
        slot.store(Some(Arc::new(UsageFetchState::Loading)));
        tokio::spawn(async move {
            let state = match provider.fetch_usage().await {
                Ok(Some(usage)) => UsageFetchState::Ready(usage),
                Ok(None) => UsageFetchState::Unsupported,
                Err(e) => UsageFetchState::Error(e.user_message()),
            };
            slot.store(Some(Arc::new(state)));
        });
    }

    /// Re-login rebuilds the provider for every tab on it, not just the focused
    /// one. A tab the user has not looked at would otherwise keep using the
    /// credentials that just failed.
    fn refresh_provider(&mut self, slug: String) {
        if self
            .sessions
            .iter()
            .any(|rt| *rt.app.state.model.provider == *slug)
        {
            self.rebuild_models();
        } else if let Some(default_model) =
            maki_config::providers::builtin_provider(&slug).and_then(|b| b.default_model)
        {
            self.change_model(default_model.to_owned(), PickOrigin::Picker);
        }
    }

    fn persist_tiers(&self, overrides: BTreeMap<ModelTier, String>) {
        self.ctx
            .storage_writer
            .persist(move |dir| model_registry::persist_overrides(dir, &overrides));
    }

    async fn shutdown(mut self) -> ShutdownReport {
        let started = Instant::now();
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };
        let exit = self.sessions[self.focused].app.exit_request.clone();
        if !self.sessions[self.focused].app.exits_on_done() {
            self.clear_program_status();
        }
        // The loop already stopped draining `UiAction`, so close the channel
        // before the handlers run. The receiver lives until this returns, and
        // a handler touching the UI would otherwise park until the shared
        // deadline, taking every later tab's `SessionEnd` down with it.
        self.ui_action_rx.close();
        // `/reload` hands these same sessions to the next generation, so
        // their handlers must not hear that the process is quitting.
        let reason = match exit {
            ExitRequest::Reload => SessionEndReason::Reload,
            _ => SessionEndReason::Shutdown,
        };
        self.ctx
            .lua_event_handle
            .end_sessions(self.sessions.iter().map(SessionRuntime::id), reason)
            .await;
        let session_end_ms = lap();
        if let Some(ref h) = self.ctx.mcp_handle {
            mcp::kill_process_groups(&h.reader().load().pids);
        }
        for rt in &self.sessions {
            rt.handles.cancel_all();
        }
        let kill_mcp_ms = lap();
        let mut tabs = Vec::with_capacity(self.sessions.len());
        let mut mailboxes = Vec::with_capacity(self.sessions.len());
        let mut agent_tasks = Vec::with_capacity(self.sessions.len());
        for rt in self.sessions.drain(..) {
            let SessionRuntime {
                mut app, handles, ..
            } = rt;
            app.checkpoint_now();
            // `app` drops at the end of this iteration, closing the
            // channels the agent loop waits on, so `join_all` can finish.
            tabs.push(OpenSession {
                session: Arc::unwrap_or_clone(app.state.session),
                claim: app.state.claim,
            });
            mailboxes.push(handles.mailbox().clone());
            agent_tasks.push(handles.into_task());
        }
        let save_sessions_ms = lap();
        crate::agent::join_all(agent_tasks, AGENT_SHUTDOWN_TIMEOUT).await;
        let join_agents_ms = lap();
        if let Some(ref h) = self.ctx.mcp_handle {
            h.shutdown().await;
        }
        let mcp_shutdown_ms = lap();
        let unsaved = match Arc::try_unwrap(self.ctx.storage_writer) {
            Ok(writer) => writer.shutdown(AGENT_SHUTDOWN_TIMEOUT).await,
            Err(_) => {
                warn!("storage writer has outstanding references, skipping graceful shutdown");
                Vec::new()
            }
        };
        self.stored_writes.close();
        if timeout(AGENT_SHUTDOWN_TIMEOUT, self.stored_writes.wait())
            .await
            .is_err()
        {
            warn!("stored session writes did not finish within {AGENT_SHUTDOWN_TIMEOUT:?}");
        }
        let storage_drain_ms = lap();
        info!(
            session_end_ms,
            kill_mcp_ms,
            save_sessions_ms,
            join_agents_ms,
            mcp_shutdown_ms,
            storage_drain_ms,
            total_ms = started.elapsed().as_millis() as u64,
            "ui shutdown phases"
        );
        ShutdownReport {
            exit,
            tabs,
            focused: self.focused,
            unsaved,
            mailboxes,
        }
    }
}

fn scroll_delta(kind: MouseEventKind, lines: u32) -> i32 {
    if kind == MouseEventKind::ScrollUp {
        lines as i32
    } else {
        -(lines as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::model_slots::tests::{OTHER_SPEC, STARTUP_SPEC, slots, strict_policy};
    use crate::app::tests::{MOVED_ID, app_with_moved_call, end_turn};
    use crate::components::BackgroundOutcome;
    use maki_agent::DoneReason;
    use maki_providers::provider::BoxFuture;
    use maki_providers::{
        AgentError, ModelInfo, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse,
        TokenUsage, UsageLimit,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;
    use tokio::sync::oneshot;

    const OBSERVATION: &str = "failed";
    const SHELL_RESULT: &str = "command finished";

    fn done_event() -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            cost: None,
            list_cost: None,
            context_size: 0,
            context_window: 0,
            num_turns: 1,
            reason: DoneReason::EndTurn,
        }
    }

    fn due_completion() -> RunNotificationState {
        let mut state = RunNotificationState::default();
        state.on_done(&done_event());
        state.on_drain();
        state
    }

    const FOCUSED_TITLE: &str = "focused";
    const BACKGROUND_TITLE: &str = "background";

    #[test_case(ProgramState::Idle, ProgramState::Working, BACKGROUND_TITLE ; "background_working_beats_focused_idle")]
    #[test_case(ProgramState::Blocked(BlockedKind::Question), ProgramState::Working, FOCUSED_TITLE ; "blocked_beats_working")]
    #[test_case(ProgramState::Done, ProgramState::Error, BACKGROUND_TITLE ; "error_beats_done")]
    #[test_case(ProgramState::Done, ProgramState::Done, FOCUSED_TITLE ; "tie_goes_to_focused")]
    fn select_program_status_cases(
        focused: ProgramState,
        background: ProgramState,
        expected_title: &str,
    ) {
        let candidates = [
            (background, false, BACKGROUND_TITLE),
            (focused, true, FOCUSED_TITLE),
            (background, false, BACKGROUND_TITLE),
        ];
        let (_, title) = select_program_status(candidates.into_iter()).unwrap();
        assert_eq!(title, expected_title);
    }

    struct QuotaProvider {
        usage: Option<ProviderUsage>,
    }

    impl Provider for QuotaProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a serde_json::Value,
            _: &'a UnboundedSender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(std::future::pending())
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn fetch_usage(&self) -> BoxFuture<'_, Result<Option<ProviderUsage>, AgentError>> {
            let usage = self.usage.clone();
            Box::pin(async move { Ok(usage) })
        }
    }

    #[tokio::test]
    async fn provider_usage_reply_finds_the_session_running_that_slug() {
        let provider = QuotaProvider {
            usage: Some(ProviderUsage {
                plan: Some("lite".into()),
                limits: vec![UsageLimit {
                    label: "rotated key".into(),
                    percentage: Some(7),
                    reset_at: None,
                    detail: None,
                }],
                ..Default::default()
            }),
        };
        let model = Model::from_spec("anthropic/claude-opus-4-6").unwrap();
        let slots = vec![Arc::new(ArcSwap::from_pointee(ModelSlot {
            model,
            provider: Arc::new(provider),
        }))];

        let reply = provider_usage_reply(slots, "anthropic".into())
            .await
            .unwrap();
        assert_eq!(reply["plan"], json!("lite"));
        assert_eq!(reply["limits"][0]["label"], json!("rotated key"));
    }

    #[test]
    fn outcome_survives_drain_and_clears_on_next_run() {
        let mut state = due_completion();
        assert_eq!(state.outcome, Some(ProgramState::Done));
        state.on_queue_item_consumed();
        assert_eq!(state.outcome, None);
    }

    #[test]
    fn completion_waits_for_queue_drain() {
        let mut state = RunNotificationState {
            response_candidate: Some("done".into()),
            ..RunNotificationState::default()
        };

        state.on_done(&done_event());
        assert!(state.waiting_for_drain());
        assert_eq!(state.reconcile(None, SessionStatus::Idle), None);

        state.on_drain();
        assert!(!state.waiting_for_drain());
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle),
            Some(Notification::TurnComplete {
                response: Some("done".into())
            })
        );
    }

    #[test_case(false, true ; "fires_once_the_session_settles")]
    #[test_case(true, false ; "new_work_drops_it")]
    fn due_completion_waits_while_the_session_works(new_work: bool, fires: bool) {
        let mut state = due_completion();
        assert_eq!(state.reconcile(None, SessionStatus::Working), None);

        if new_work {
            state.on_queue_item_consumed();
        }

        assert_eq!(state.reconcile(None, SessionStatus::Idle).is_some(), fires);
        assert_eq!(state.reconcile(None, SessionStatus::Idle), None);
    }

    /// A background task can ask something after the turn ended. The turn
    /// still notifies once the user answers and the work is done.
    #[test]
    fn prompt_wins_over_completion_and_is_not_repeated_unchanged() {
        let prompt = Notification::QuestionRequested;
        let mut state = due_completion();

        assert_eq!(
            state.reconcile(Some(prompt.clone()), SessionStatus::NeedsInput),
            Some(prompt.clone())
        );
        assert_eq!(
            state.reconcile(Some(prompt), SessionStatus::NeedsInput),
            None
        );
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle),
            Some(Notification::TurnComplete { response: None })
        );
    }

    #[test_case(Focus::Focused, false ; "watching_user_already_saw_the_turn_end")]
    #[test_case(Focus::Unfocused, true ; "away_from_the_terminal")]
    #[test_case(Focus::default(), true ; "unproven_terminal_without_input_notifies")]
    #[test_case(stale_input(), true ; "unproven_terminal_left_alone_notifies")]
    #[test_case(fresh_input(), false ; "unproven_terminal_typed_into_just_now")]
    fn focus_suppresses_only_turn_completions(focus: Focus, completion_fires: bool) {
        let completion = Notification::TurnComplete { response: None };

        assert_eq!(focus.allows(&completion), completion_fires);
        assert!(focus.allows(&Notification::QuestionRequested));
    }

    fn stale_input() -> Focus {
        Focus::Unproven {
            last_input: Instant::now().checked_sub(INPUT_IMPLIES_WATCHING),
        }
    }

    fn fresh_input() -> Focus {
        let mut focus = Focus::default();
        focus.note_input();
        focus
    }

    #[test]
    fn input_never_latches_on_an_unproven_terminal() {
        let mut focus = fresh_input();

        focus.on_resume();

        assert_eq!(focus, Focus::default());
    }

    #[cfg(not(windows))]
    #[test]
    fn a_focus_report_makes_input_and_resume_meaningful() {
        let mut focus = Focus::default();

        focus.report(Focus::Focused);
        assert_eq!(focus, Focus::Focused);

        focus.on_resume();
        assert_eq!(focus, Focus::Unfocused);

        focus.note_input();
        assert_eq!(focus, Focus::Focused);

        focus.report(Focus::Unfocused);
        assert_eq!(focus, Focus::Unfocused);
    }

    #[test]
    fn notification_selection_prefers_priority_then_session_order() {
        let completion = Notification::TurnComplete { response: None };
        let first_prompt = Notification::QuestionRequested;
        let second_prompt = Notification::AuthenticationRequired;

        let selected = select_notification(None, Some(completion));
        let selected = select_notification(selected, Some(first_prompt.clone()));
        let selected = select_notification(selected, Some(second_prompt));

        assert_eq!(selected, Some(first_prompt));
    }

    #[test]
    fn shell_results_do_not_replace_existing_preamble() {
        let mut preamble = vec![Message::observation(OBSERVATION.into())];

        prepend_preamble(
            &mut preamble,
            vec![Message::observation(SHELL_RESULT.into())],
        );

        let text = preamble.iter().map(Message::user_text).collect::<Vec<_>>();
        assert_eq!(text, [Some(SHELL_RESULT), Some(OBSERVATION)]);
    }

    /// A turn that moved a call out is not over for the user: the session
    /// picker keeps its spinner until the task ends, with no "finished"
    /// flash in between.
    #[test]
    fn a_session_stays_working_while_its_background_task_runs() {
        let (mut app, _buf) = app_with_moved_call();
        end_turn(&mut app);
        assert_eq!(SessionStatus::of(&app), SessionStatus::Working);

        app.end_task(MOVED_ID, BackgroundOutcome::Done);
        assert_eq!(SessionStatus::of(&app), SessionStatus::Idle);
    }

    const PACK_ANSWER: &str = "prepared";

    #[tokio::test]
    async fn a_panicked_pack_task_still_answers_and_frees_the_slot() {
        let mut pack: Option<JoinHandle<PackPreparation>> =
            Some(tokio::spawn(async { panic!("the preparation broke") }));
        let answer = pack_done(&mut pack).await;
        assert_eq!(answer, PackPreparation::failed(PACK_PANIC_ERR.to_owned()));
        assert!(pack.is_none());
    }

    #[tokio::test]
    async fn a_dropped_pack_wait_keeps_the_task() {
        let (answer_tx, answer_rx) = oneshot::channel();
        let mut pack = Some(tokio::spawn(async move { answer_rx.await.unwrap() }));
        tokio::select! {
            biased;
            _ = pack_done(&mut pack) => panic!("the task has not answered yet"),
            () = ready(()) => {}
        }
        assert!(pack.is_some());

        let expected = PackPreparation::failed(PACK_ANSWER.to_owned());
        answer_tx.send(expected.clone()).unwrap();
        assert_eq!(pack_done(&mut pack).await, expected);
        assert!(pack.is_none());
    }

    const OPEN_ERROR: &str = "no such session";

    #[tokio::test]
    async fn a_panicked_open_answers_nothing_and_frees_the_slot() {
        let mut opening = Some(AbortOnDropHandle::new(tokio::spawn(async {
            panic!("the read broke")
        })));
        assert!(answered(&mut opening).await.is_none());
        assert!(opening.is_none());
    }

    #[tokio::test]
    async fn a_dropped_open_wait_keeps_the_read() {
        let (go_tx, go_rx) = oneshot::channel::<()>();
        let (reply_tx, _reply_rx) = oneshot::channel();
        let mut opening = Some(AbortOnDropHandle::new(tokio::spawn(async move {
            go_rx.await.unwrap();
            Box::new(OpenedSession(
                PendingFocus {
                    reply_tx,
                    from: MakiId::generate(),
                },
                Err(OPEN_ERROR.to_owned()),
            ))
        })));
        tokio::select! {
            biased;
            _ = answered(&mut opening) => panic!("the read has not answered yet"),
            () = ready(()) => {}
        }
        assert!(opening.is_some());

        go_tx.send(()).unwrap();
        let answer = answered(&mut opening).await.unwrap();
        assert!(matches!(&*answer, OpenedSession(_, Err(e)) if e == OPEN_ERROR));
        assert!(opening.is_none());
    }

    const THIRD_SPEC: &str = "opencode/third";
    const OUT_OF_ORDER: &str = "a pick must wait for the picks made before it";
    const SESSION_HELD: &str = "a session must not wait for a pick";
    const NO_STARTUP_SLOT: &str = "the startup spec has a slot from the start";
    const CREATED_CWD: &str = "/tmp";
    const NOT_BUILDING: &str = "a waiter with no slot has a build running";
    const NO_SLOT: &str = "every waiter here gets a slot";
    const STALE_HELD: &str = "a slot from before an invalidate must not reach a waiter";

    fn pick_for(tab: &Arc<ArcSwap<ModelSlot>>) -> Wanted {
        Wanted::Pick(Pick {
            tab: Arc::downgrade(tab),
            origin: PickOrigin::Picker,
        })
    }

    fn created(dir: &TempDir) -> Wanted {
        let (reply_tx, _reply_rx) = oneshot::channel();
        let storage = StateDir::from_path(dir.path().to_path_buf());
        Wanted::Create(CreatedSession {
            open: OpenSession::fresh(STARTUP_SPEC, CREATED_CWD, &storage),
            prompt: None,
            focus: false,
            reply_tx,
        })
    }

    async fn land_next(slots: &mut ModelSlots, waiting: &mut SlotWaiters) {
        let landed = slots.next_built().await.expect(NOT_BUILDING);
        if let Some((spec, built)) = slots.land(landed) {
            waiting.landed(&spec, &built);
        }
    }

    fn ready_specs(waiting: &mut SlotWaiters) -> Vec<(String, bool)> {
        waiting
            .take_ready()
            .into_iter()
            .map(|(waiter, built)| (waiter.spec, built.is_ok()))
            .collect()
    }

    #[test_case(ModelPolicy::default(), false, &[(OTHER_SPEC, true), (THIRD_SPEC, true)] ; "two_builds_apply_in_order")]
    #[test_case(ModelPolicy::default(), false, &[(OTHER_SPEC, true), (STARTUP_SPEC, true)] ; "a_built_spec_waits_behind_a_build")]
    #[test_case(ModelPolicy::default(), true, &[(OTHER_SPEC, true), (STARTUP_SPEC, true)] ; "a_build_from_before_an_invalidate_runs_again")]
    #[test_case(strict_policy(), false, &[(OTHER_SPEC, false), (STARTUP_SPEC, true)] ; "a_refused_pick_does_not_hold_up_the_queue")]
    #[tokio::test]
    async fn picks_settle_in_the_order_they_were_made(
        policy: ModelPolicy,
        invalidate_mid_build: bool,
        expected: &[(&str, bool)],
    ) {
        let mut slots = slots(policy);
        let tab = Arc::new(ArcSwap::from(
            slots.get(STARTUP_SPEC).expect(NO_STARTUP_SLOT),
        ));
        let mut waiting = SlotWaiters::default();
        for (spec, _) in expected {
            waiting.push(&mut slots, (*spec).to_owned(), pick_for(&tab));
        }
        assert!(ready_specs(&mut waiting).is_empty(), "{OUT_OF_ORDER}");
        assert_eq!(
            waiting.pending_pick(&tab),
            expected.last().map(|(spec, _)| *spec)
        );
        if invalidate_mid_build {
            slots.invalidate();
            waiting.restart(&mut slots);
        }

        let mut settled = Vec::new();
        while settled.len() < expected.len() {
            land_next(&mut slots, &mut waiting).await;
            settled.extend(ready_specs(&mut waiting));
        }

        let expected: Vec<(String, bool)> = expected
            .iter()
            .map(|(spec, built)| ((*spec).to_owned(), *built))
            .collect();
        assert_eq!(settled, expected, "{OUT_OF_ORDER}");
        assert_eq!(waiting.pending_pick(&tab), None);
    }

    #[tokio::test]
    async fn a_pick_held_through_an_invalidate_gets_a_slot_built_after_it() {
        let mut slots = slots(ModelPolicy::default());
        let before = slots.get(STARTUP_SPEC).expect(NO_STARTUP_SLOT);
        let tab = Arc::new(ArcSwap::from(Arc::clone(&before)));
        let mut waiting = SlotWaiters::default();
        waiting.push(&mut slots, OTHER_SPEC.to_owned(), pick_for(&tab));
        waiting.push(&mut slots, STARTUP_SPEC.to_owned(), pick_for(&tab));
        slots.invalidate();
        waiting.restart(&mut slots);

        let mut settled = Vec::new();
        while settled.len() < 2 {
            land_next(&mut slots, &mut waiting).await;
            settled.extend(waiting.take_ready());
        }
        let Some((_, Ok(startup))) = settled.last() else {
            panic!("{NO_SLOT}");
        };
        assert!(!Arc::ptr_eq(startup, &before), "{STALE_HELD}");
    }

    #[tokio::test]
    async fn a_session_comes_up_while_an_earlier_pick_still_builds() {
        let mut slots = slots(ModelPolicy::default());
        let tab = Arc::new(ArcSwap::from(
            slots.get(STARTUP_SPEC).expect(NO_STARTUP_SLOT),
        ));
        let mut waiting = SlotWaiters::default();
        waiting.push(&mut slots, OTHER_SPEC.to_owned(), pick_for(&tab));
        let dir = TempDir::new().unwrap();
        waiting.push(&mut slots, STARTUP_SPEC.to_owned(), created(&dir));

        assert_eq!(
            ready_specs(&mut waiting),
            [(STARTUP_SPEC.to_owned(), true)],
            "{SESSION_HELD}"
        );
    }

    #[tokio::test]
    async fn two_waiters_on_one_spec_share_its_build() {
        let mut slots = slots(ModelPolicy::default());
        let mut waiting = SlotWaiters::default();
        let dir = TempDir::new().unwrap();
        waiting.push(&mut slots, OTHER_SPEC.to_owned(), created(&dir));
        waiting.push(&mut slots, OTHER_SPEC.to_owned(), created(&dir));

        land_next(&mut slots, &mut waiting).await;
        let ready = waiting.take_ready();
        assert_eq!(ready.len(), 2);
        let [(_, Ok(first)), (_, Ok(second))] = &ready[..] else {
            panic!("{NO_SLOT}");
        };
        assert!(Arc::ptr_eq(first, second));
        assert!(
            timeout(Duration::ZERO, slots.next_built())
                .await
                .is_ok_and(|next| next.is_none())
        );
    }
}
