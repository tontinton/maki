//! Multi-session supervisor: every session owns an `App` + `AgentHandles` and
//! keeps draining agent events while backgrounded; only the focused session
//! renders and receives input. `SpawnCtx` carries the shared resources needed
//! to spawn session runtimes at any point.
//!
//! Terminal input arrives on a channel (see [`InputReader`]), so the loop
//! waits on every event source at once and wakes the moment a plugin action,
//! agent event, or keypress arrives instead of sleeping in `event::poll`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::{ArcSwap, ArcSwapOption};
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};

use crossterm::event::{
    Event, KeyEventKind, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use maki_agent::command::CustomCommand;
use maki_agent::permissions::PermissionManager;
use maki_agent::{
    AgentConfig, AgentEvent, CancelToken, Envelope, McpCommand, McpConfigErrors, McpHandle, mcp,
};
use maki_config::{ModelPolicy, UiConfig};
use maki_lua::session_snapshot::{
    MODE_BUILD, MODE_PLAN, STATUS_IDLE, STATUS_NEEDS_INPUT, STATUS_WORKING, SessionQueueSnapshot,
    SessionSnapshot,
};
use maki_lua::{
    EventHandle, HintReader, KeymapReader, LuaCommandReader, ModelRequest, PackCommand,
    PackPreparation, SessionEndReason, SessionRequest, TaskRequest, UiAction, UiAttachment,
    UiReply,
};
use maki_providers::Timeouts;
use maki_providers::provider::{Provider, fetch_all_models, from_model};
use maki_providers::{Message, Model};
use maki_storage::StateDir;
use maki_storage::StorageError;
use maki_storage::id::{MakiId, MakiIdParseError, SessionRef};
use maki_storage::sessions::{SessionError, normalize_title};
use ratatui::backend::Backend;
use serde_json::json;
use tracing::{info, warn};

use crate::AppSession;
use crate::agent::{
    AgentCommand, AgentHandles, ModelSlot,
    shared_queue::{Compaction, QueueItem, QueuedInput},
};
use crate::app::shell::{ShellEvent, spawn_shell};
use crate::app::tasks::{TaskStatus, diff_task_states};
use crate::app::{App, Msg, Notification, QueuedMessage, SubmitOutcome, turn_response};
use crate::color_compat;
use crate::components::input::Submission;
use crate::components::usage_modal::UsageFetchState;
use crate::components::{Action, ExitRequest, Status};
use crate::input::InputReader;
use crate::repaint::{Dirty, IDLE_POLL};

use crate::storage_writer::StorageWriter;
use crate::terminal;

/// Max events handled per frame so a flood cannot starve rendering.
const DRAIN_BUDGET: usize = 256;
const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DELETE_FOCUSED_ERR: &str = "cannot delete the focused session";
const MODEL_POLICY_ERR: &str = "Model is not allowed by policy";
const INVALID_MODEL_ERR: &str = "Invalid model";
const PROVIDER_INIT_ERR: &str = "Failed to create provider";
const NOT_LIVE_ERR: &str = "session not live";
const PACK_PREPARING: &str = "Checking packages...";
const PACK_BUSY_ERR: &str = "a package command is already running";
const PACK_PANIC_ERR: &str = "the package command stopped unexpectedly";

/// Tabs carry their in-memory sessions so `/reload` reopens them without a
/// disk round-trip; `session_has_content` tells which ones were saved.
pub(crate) struct ShutdownReport {
    pub exit: ExitRequest,
    pub tabs: Vec<AppSession>,
    pub focused: usize,
}

pub struct EventLoopParams {
    pub model: Model,
    pub needs_login: bool,
    pub commands: Vec<CustomCommand>,
    pub sessions: Vec<AppSession>,
    pub focused: usize,
    pub startup_warnings: Vec<String>,
    pub startup_notice: Option<String>,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: Timeouts,
    pub exit_on_done: bool,
    pub lua_command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: HintReader,
    pub ui_action_rx: flume::Receiver<UiAction>,
    pub ui_attachment: UiAttachment,
    pub lua_event_handle: EventHandle,
    pub model_policy: Arc<ModelPolicy>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
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
}

impl RunNotificationState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn on_queue_item_consumed(&mut self) {
        self.response_candidate = None;
        self.pending_completion = None;
    }

    fn on_turn_complete(&mut self, message: &Message) {
        self.response_candidate = turn_response(message);
    }

    fn on_done(&mut self, event: &AgentEvent) {
        let notification = match event {
            AgentEvent::Done { .. } => Notification::TurnComplete {
                response: self.response_candidate.take(),
            },
            AgentEvent::Error { .. } => {
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
        queue_empty: bool,
    ) -> Option<Notification> {
        let settled = attention.is_none() && status == SessionStatus::Idle && queue_empty;
        let prompt = (attention != self.last_attention)
            .then(|| attention.clone())
            .flatten();
        self.last_attention = attention;

        // A due completion is decided on its first reconcile: fire if the
        // session settled, otherwise drop it for good.
        let completion = match self.pending_completion.take() {
            Some(PendingCompletion::Due(notification)) => settled.then_some(notification),
            waiting => {
                self.pending_completion = waiting;
                None
            }
        };
        prompt.or(completion)
    }
}

impl SessionStatus {
    fn of(app: &App) -> Self {
        if app.awaiting_input() {
            Self::NeedsInput
        } else if app.status == Status::Streaming {
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

struct SessionRuntime {
    app: App,
    handles: AgentHandles,
    shell_tx: flume::Sender<ShellEvent>,
    shell_rx: flume::Receiver<ShellEvent>,
    last_status: SessionStatus,
    /// Keyed by task id, never by position: a session reset reuses positions,
    /// so a new task would inherit the old one's status.
    last_tasks: Vec<(Arc<str>, TaskStatus)>,
    notifications: RunNotificationState,
}

impl SessionRuntime {
    fn id(&self) -> MakiId {
        self.app.state.session.id
    }

    /// New work cancels an `exit_on_done` exit still waiting on its drain.
    fn reset_run_notifications(&mut self) {
        if self.notifications.waiting_for_drain() {
            self.app.clear_exit_request();
        }
        self.notifications.reset();
    }

    /// A wake may only start a background run when the session is fully
    /// quiescent. Idle status alone is not enough: restored queue items start
    /// runs without `start_run` (the app only learns of them via
    /// `QueueItemConsumed`), and `start_run` destroys text held for recovery
    /// after an agent error.
    fn quiescent(&self) -> bool {
        SessionStatus::of(&self.app) == SessionStatus::Idle
            && self.handles.queue.is_empty()
            && !self.app.holds_recovery_text()
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
    lua_event_handle: EventHandle,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    model_slot: Arc<ArcSwap<ModelSlot>>,
    available_models: Arc<ArcSwapOption<Vec<String>>>,
    storage_writer: Arc<StorageWriter>,
    model_policy: Arc<ModelPolicy>,
}

impl SpawnCtx {
    fn spawn_runtime(&self, session: AppSession) -> SessionRuntime {
        let resumed = !session.messages().is_empty();
        let permissions = Arc::new(self.permissions.fork());
        let handles = AgentHandles::spawn(
            &self.model_slot,
            session.messages().to_vec(),
            self.config.clone(),
            self.ui_config.tool_output_lines,
            &permissions,
            Some(SessionRef::from(session.id)),
            self.timeouts,
            self.lua_event_handle.clone(),
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
            Arc::clone(&self.model_policy),
        );
        let mut app = App::new(
            &self.model_slot.load().model,
            session,
            self.storage.clone(),
            Arc::clone(&self.available_models),
            handles.mcp_reader(),
            handles.mcp_config_errors.clone(),
            self.lua_command_reader.clone(),
            self.keymap_reader.clone(),
            self.hint_reader.clone(),
            Arc::clone(&self.storage_writer),
            self.ui_config.clone(),
            self.input_history_size,
            permissions,
            Arc::clone(&self.custom_commands),
            self.lua_event_handle.clone(),
            Arc::clone(&self.model_policy),
        );
        handles.apply_to_app(&mut app);
        if resumed {
            app.restore_resumed_session();
        }
        let (shell_tx, shell_rx) = flume::unbounded::<ShellEvent>();
        SessionRuntime {
            app,
            handles,
            shell_tx,
            shell_rx,
            last_status: SessionStatus::Idle,
            last_tasks: Vec::new(),
            notifications: RunNotificationState::default(),
        }
    }
}

pub(crate) struct EventLoop<'t> {
    terminal: &'t mut ratatui::DefaultTerminal,
    sessions: Vec<SessionRuntime>,
    focused: usize,
    last_focused: Option<MakiId>,
    focus: Focus,
    notifier: Option<terminal::TerminalNotifier>,
    ctx: SpawnCtx,
    input: InputReader,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    ui_action_rx: flume::Receiver<UiAction>,
    ui_attachment: UiAttachment,
    pack_tx: flume::Sender<Box<PackPreparation>>,
    pack_rx: flume::Receiver<Box<PackPreparation>>,
    /// One package command at a time. The work runs on its own thread, so
    /// without this a second `/packupdate` would race the first over the same
    /// clones and locks.
    pack_running: bool,
    _model_fetch_task: smol::Task<()>,
}

/// One item from any of the event loop's sources; `None` from `next_wake`
/// means the wait timed out (animation/idle tick).
enum Wake {
    Input(Event),
    InputGone,
    Ui(UiAction),
    Agent(usize, Box<maki_agent::Envelope>),
    Shell(usize, ShellEvent),
    Warn(String),
    Pack(Box<PackPreparation>),
}

struct BackgroundModels {
    available: Arc<ArcSwapOption<Vec<String>>>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    task: smol::Task<()>,
}

fn merge_batch(
    available: &Arc<ArcSwapOption<Vec<String>>>,
    batch: maki_providers::provider::ModelBatch,
    warn_tx: &flume::Sender<String>,
) {
    for w in batch.warnings {
        let _ = warn_tx.try_send(w);
    }
    if batch.models.is_empty() {
        return;
    }
    let mut merged = available.load().as_deref().cloned().unwrap_or_default();
    for spec in &batch.models {
        if !merged.contains(spec) {
            merged.push(spec.clone());
        }
    }
    available.store(Some(Arc::new(merged)));
}

fn spawn_model_fetch(
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    timeouts: Timeouts,
    policy: Arc<ModelPolicy>,
) -> BackgroundModels {
    let available: Arc<ArcSwapOption<Vec<String>>> = Arc::new(ArcSwapOption::empty());
    let bg = Arc::clone(&available);
    let (warn_tx, warn_rx) = flume::unbounded::<String>();
    let warn_tx_bg = warn_tx.clone();
    let model_slot = Arc::clone(model_slot);
    let task = smol::spawn(async move {
        let warn_tx = warn_tx_bg;
        let done = Box::new(move || {
            let spec = model_slot.load().model.spec();
            let mut resolved = match Model::from_spec(&spec) {
                Ok(m) => m,
                Err(e) => {
                    warn!(spec = %spec, error = %e, "failed to resolve model after discovery");
                    return;
                }
            };
            let provider = match from_model(&mut resolved, timeouts) {
                Ok(p) => p,
                Err(e) => {
                    warn!(spec = %spec, error = %e, "failed to create provider after discovery");
                    return;
                }
            };
            model_slot.store(Arc::new(ModelSlot {
                model: resolved,
                provider: Arc::from(provider),
            }));
        });
        fetch_all_models(
            &policy,
            |batch| merge_batch(&bg, batch, &warn_tx),
            Some(done),
        )
        .await;
    });
    BackgroundModels {
        available,
        warn_rx,
        warn_tx,
        task,
    }
}

impl<'t> EventLoop<'t> {
    pub(crate) fn new(
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
            storage,
            config,
            ui_config,
            input_history_size,
            permissions,
            timeouts,
            exit_on_done,
            lua_command_reader,
            keymap_reader,
            hint_reader,
            ui_action_rx,
            ui_attachment,
            lua_event_handle,
            model_policy,
        } = params;
        // A `/reload` generation inherits the handles of the one before it,
        // so every loop has to claim the UI back for itself.
        ui_attachment.attach();

        // Apply the config theme before the warmup thread spawns, or warmup
        // could bake the syntax palette from the old theme. Only the
        // in-memory name is set, so the user's saved pick survives.
        if let Some(ref name) = ui_config.theme {
            match crate::theme::load_by_name(name) {
                Ok(theme) => {
                    crate::theme::set_current_name(name);
                    crate::theme::set(theme);
                }
                Err(e) => startup_warnings.push(format!("config ui.theme: {e}")),
            }
        }

        static PROCESS_WARMUP: std::sync::Once = std::sync::Once::new();
        PROCESS_WARMUP.call_once(|| {
            maki_highlight::pool::spawn(crate::highlight::warmup);
            crate::update::spawn_check();
        });

        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let (mcp_handle, mcp_config_errors) = smol::block_on(mcp::start(&cwd));

        let provider: Arc<dyn Provider> = if needs_login {
            Arc::from(maki_providers::provider::from_model_fallback(
                &mut model, timeouts,
            ))
        } else {
            Arc::from(from_model(&mut model, timeouts).context("create provider")?)
        };
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: model.clone(),
            provider,
        }));
        let bg = spawn_model_fetch(&model_slot, timeouts, Arc::clone(&model_policy));
        let storage_writer = Arc::new(StorageWriter::new(storage.clone(), bg.warn_tx.clone()));

        let notifier = terminal::TerminalNotifier::new(ui_config.notifications);
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
            lua_event_handle,
            mcp_handle,
            mcp_config_errors,
            model_slot,
            available_models: bg.available,
            storage_writer,
            model_policy,
        };

        let mut runtimes: Vec<SessionRuntime> = sessions
            .into_iter()
            .map(|session| ctx.spawn_runtime(session))
            .collect();
        if runtimes.is_empty() {
            return Err(eyre!("event loop needs at least one session"));
        }
        let focused = focused.min(runtimes.len() - 1);
        let app = &mut runtimes[focused].app;
        app.exit_on_done = exit_on_done;
        if needs_login {
            app.login_picker.open(app.storage.clone());
        }
        if !ctx.mcp_config_errors.is_empty() {
            let msg = format!("MCP config error: {}", ctx.mcp_config_errors);
            app.flash(msg);
        }
        if let Some(notice) = startup_notice {
            app.flash(notice);
        }
        for warning in startup_warnings {
            app.flash(warning);
        }

        let (pack_tx, pack_rx) = flume::unbounded();
        Ok(Self {
            terminal,
            sessions: runtimes,
            focused,
            last_focused: None,
            focus: Focus::default(),
            notifier,
            ctx,
            input: InputReader::spawn(),
            warn_rx: bg.warn_rx,
            warn_tx: bg.warn_tx,
            ui_action_rx,
            ui_attachment,
            pack_tx,
            pack_rx,
            pack_running: false,
            _model_fetch_task: bg.task,
        })
    }

    fn focused_app(&mut self) -> &mut App {
        &mut self.sessions[self.focused].app
    }

    /// Paints a frame and parks the terminal cursor on the cell the input box
    /// reversed, without showing it. macOS anchors IME preedit text to the
    /// cursor, so it has to sit there, but a visible one would invert that
    /// already reversed cell back to plain text and ride along with every cell
    /// the next diff writes.
    ///
    /// `Frame::set_cursor_position` cannot do this: ratatui shows the cursor
    /// whenever a frame asks for a position. Hence the move after the draw,
    /// and the hide that goes with it, so a widget that does ask for one
    /// cannot bring the block cursor back.
    fn paint(&mut self) -> Result<()> {
        let app = &mut self.sessions[self.focused].app;
        let mut cursor = None;
        self.terminal.draw(|f| {
            cursor = app.view(f);
            color_compat::downgrade_if_needed(f.buffer_mut());
        })?;
        if let Some(pos) = cursor {
            self.terminal.hide_cursor()?;
            self.terminal.set_cursor_position(pos)?;
            self.terminal.backend_mut().flush()?;
        }
        Ok(())
    }

    pub(crate) fn run(mut self, initial_prompt: Option<String>) -> Result<ShutdownReport> {
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
            match self.drain_channels() {
                Ok(d) => dirty |= d,
                Err(e) => break Err(e),
            }
            self.checkpoint_all();
            if dirty.take()
                && let Err(e) = self.paint()
            {
                break Err(e);
            }

            if let Some(i) = self.sessions.iter().position(|rt| {
                rt.app.exit_request != ExitRequest::None && !rt.notifications.waiting_for_drain()
            }) {
                // A backgrounded session can finish an `exit_on_done` turn;
                // focus it so shutdown reports its exit code and id.
                self.focused = i;
                self.emit_notifications();
                break Ok(());
            }

            // Sleeping a whole frame instead of a fraction of one is what
            // makes a spinner cost 12 paints a second instead of 62.
            let cadence = self.sessions[self.focused].app.cadence();
            match self.next_wake(cadence.frame().unwrap_or(IDLE_POLL)) {
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
        let report = self.shutdown();
        result.map(|()| report)
    }

    /// Wait for the next event from any source, or time out so animations
    /// and periodic polls keep running. `Duration::ZERO` drains whatever is
    /// already pending.
    fn next_wake(&self, timeout: Duration) -> Option<Wake> {
        let mut sel = flume::Selector::new().recv(self.input.receiver(), |res| match res {
            Ok(ev) => Some(Wake::Input(ev)),
            Err(_) => Some(Wake::InputGone),
        });
        if !self.ui_action_rx.is_disconnected() {
            sel = sel.recv(&self.ui_action_rx, |res| res.ok().map(Wake::Ui));
        }
        sel = sel.recv(&self.warn_rx, |res| res.ok().map(Wake::Warn));
        sel = sel.recv(&self.pack_rx, |res| res.ok().map(Wake::Pack));
        for (i, rt) in self.sessions.iter().enumerate() {
            if !rt.handles.agent_rx.is_disconnected() {
                sel = sel.recv(&rt.handles.agent_rx, move |res| {
                    res.ok().map(|env| Wake::Agent(i, Box::new(env)))
                });
            }
            sel = sel.recv(&rt.shell_rx, move |res| {
                res.ok().map(|ev| Wake::Shell(i, ev))
            });
        }
        sel.wait_timeout(timeout).ok().flatten()
    }

    fn handle_wake(&mut self, wake: Wake) -> Result<()> {
        match wake {
            Wake::Input(ev) => self.handle_input(ev),
            Wake::InputGone => return Err(eyre!("terminal input reader stopped")),
            Wake::Ui(action) => self.handle_ui_action(action),
            Wake::Agent(i, envelope) => self.handle_agent(i, envelope),
            Wake::Shell(i, event) => self.sessions[i].app.handle_shell_event(event),
            Wake::Warn(warning) => self.focused_app().flash(warning),
            Wake::Pack(preparation) => self.finish_pack(*preparation),
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

    /// Only the focused session is drawn, so only it can owe a frame; focusing
    /// another is an event, and events always repaint. Background sessions
    /// still drain their floats, or a plugin writing to a window nobody is
    /// looking at would lose the output.
    fn tick(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            if i == self.focused {
                dirty |= rt.app.tick();
            } else {
                let _ = rt.app.float_mgr.tick();
            }
        }
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
                if rt.app.exit_on_done {
                    rt.app.clear_exit_request();
                }
            }
            AgentEvent::TurnComplete(turn) if current => {
                rt.notifications.on_turn_complete(&turn.message);
            }
            event if current => rt.notifications.on_done(event),
            _ => {}
        }
        let actions = self.sessions[idx].app.update(Msg::Agent(envelope));
        self.dispatch(idx, actions);
    }

    fn drain_channels(&mut self) -> Result<Dirty> {
        let mut dirty = Dirty::NO;
        // Leftovers beyond the budget are picked up right after the next draw.
        for _ in 0..DRAIN_BUDGET {
            match self.next_wake(Duration::ZERO) {
                Some(wake) => {
                    self.handle_wake(wake)?;
                    dirty = Dirty::YES;
                }
                None => break,
            }
        }

        let slot_model = self.ctx.model_slot.load();
        let spec = slot_model.model.spec();
        for rt in &mut self.sessions {
            if rt.app.state.session.model != spec
                || rt.app.state.model.context_window != slot_model.model.context_window
            {
                rt.app.update_model(&slot_model.model);
                dirty = Dirty::YES;
            }
            rt.app.emit_model_change();
        }
        drop(slot_model);

        // These two only fire Lua autocmds. Anything a handler does comes back
        // as a `UiAction` on the next wake, which repaints then.
        self.emit_focus_change();
        dirty |= self.start_mailbox_runs();
        self.emit_status_changes();
        self.emit_task_changes();
        self.emit_notifications();
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
                buf,
                config,
                focus,
                event_tx,
                cmd_rx,
            } => {
                let app = self.focused_app();
                app.float_mgr.open(buf, config, focus, event_tx, cmd_rx);
                if focus {
                    app.transition_plan(crate::app::mode::PlanTrigger::InteractivePrompt);
                }
            }
            UiAction::Session { req, reply_tx } => {
                self.handle_session_request(req, reply_tx);
            }
            UiAction::Model { req, reply_tx } => {
                let _ = reply_tx.send(self.handle_model_request(req));
            }
            UiAction::Task { req, reply_tx } => {
                let _ = reply_tx.send(self.handle_task_request(req));
            }
            UiAction::WinSaveView { reply_tx } => {
                let _ = reply_tx.send(self.focused_app().win_view());
            }
            UiAction::WinRestView { scroll_top } => {
                self.focused_app().scroll_to_row(scroll_top);
            }
            UiAction::Builtin(action) => {
                let actions = self.focused_app().run_builtin(action);
                self.dispatch(self.focused, actions);
            }
            UiAction::RunCommand {
                cmdline,
                depth,
                reply_tx,
            } => {
                // Answer before dispatching: the caller only waits on the name
                // resolving, and dispatch may take a while (or exit the app).
                match self.focused_app().run_cmdline(&cmdline, depth) {
                    Ok(actions) => {
                        let _ = reply_tx.send(Ok(()));
                        self.dispatch(self.focused, actions);
                    }
                    Err(e) => {
                        let _ = reply_tx.send(Err(e));
                    }
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
            handle.fire_autocmd(
                "SessionStatusChanged",
                json!({
                    "session_id": rt.id(),
                    "title": rt.app.state.session.title,
                    "status": status.as_str(),
                    "focused": i == self.focused,
                }),
            );
        }
    }

    /// One diff per frame covers every path that finishes, cancels or errors a
    /// chat, so none of them has to remember to fire an event.
    fn emit_task_changes(&mut self) {
        let handle = &self.ctx.lua_event_handle;
        for rt in &mut self.sessions {
            let session_id = rt.app.state.session.id;
            diff_task_states(&mut rt.last_tasks, rt.app.task_states(), |task| {
                handle.fire_autocmd(
                    "TaskStatusChanged",
                    json!({
                        "session_id": session_id,
                        "id": task.id,
                        "name": task.name,
                        "status": task.status,
                    }),
                );
            });
        }
    }

    fn emit_notifications(&mut self) {
        let Some(notifier) = &self.notifier else {
            return;
        };
        let mut selected = None;
        for rt in &mut self.sessions {
            let candidate = rt.notifications.reconcile(
                rt.app.attention(),
                rt.last_status,
                rt.handles.queue.is_empty(),
            );
            selected = select_notification(selected, candidate);
        }
        if let Some(notification) = selected.filter(|n| self.focus.allows(n))
            && let Err(error) = notifier.notify(&notification.message())
        {
            warn!(notifier = ?notifier.notifier(), %error, "terminal notifications disabled after write failure");
            self.notifier = None;
        }
    }

    fn emit_focus_change(&mut self) {
        let id = self.sessions[self.focused].id();
        if self.last_focused == Some(id) {
            return;
        }
        let mut data = json!({ "session_id": id });
        if let Some(previous) = self.last_focused {
            data["previous_session_id"] = json!(previous.to_string());
        }
        self.last_focused = Some(id);
        self.ctx
            .lua_event_handle
            .fire_autocmd("SessionFocusChanged", data);
    }

    fn start_mailbox_runs(&mut self) -> Dirty {
        let ready: Vec<_> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, runtime)| {
                if !runtime.quiescent() {
                    return None;
                }
                let preamble = runtime.handles.claim_mailbox_wake();
                (!preamble.is_empty()).then_some((index, preamble))
            })
            .collect();

        let dirty = Dirty::from(!ready.is_empty());
        for (index, preamble) in ready {
            let actions = self.sessions[index].app.start_mailbox_run(preamble);
            self.dispatch(index, actions);
        }
        dirty
    }

    /// `List` replies from a background task (the scan can be slow); every
    /// other request is answered synchronously by the event loop, which owns
    /// the live runtimes.
    fn handle_session_request(&mut self, req: SessionRequest, reply_tx: flume::Sender<UiReply>) {
        match req {
            SessionRequest::List => {
                let storage = self.ctx.storage.clone();
                smol::unblock(move || {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    let reply = AppSession::list(&cwd.to_string_lossy(), &storage)
                        .map_err(|e| e.to_string())
                        .and_then(|list| serde_json::to_value(list).map_err(|e| e.to_string()));
                    let _ = reply_tx.send(reply);
                })
                .detach();
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
                if let Some(i) = self.position(id) {
                    if i == self.focused {
                        let _ = reply_tx.send(Err(DELETE_FOCUSED_ERR.into()));
                        return;
                    }
                    let rt = self.remove_runtime(i);
                    self.ctx
                        .lua_event_handle
                        .end_session(rt.id(), SessionEndReason::Delete);
                    rt.handles.cancel();
                }
                self.ctx.storage_writer.delete(id, move |res| {
                    let reply = match res {
                        Ok(()) | Err(SessionError::Storage(StorageError::NotFound(_))) => {
                            Ok(json!(true))
                        }
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
            SessionRequest::Read { id } => {
                let reply = match self.resolve_session_index(id.as_deref()) {
                    Ok(idx) => Ok(self.session_snapshot_json(idx)),
                    Err(e) => Err(e),
                };
                let _ = reply_tx.send(reply);
            }
            SessionRequest::New { prompt, focus } => {
                let session = self.focused_app().blank_session();
                let idx = self.push_runtime(self.ctx.spawn_runtime(session));
                let id = self.sessions[idx].id();
                maki_otel::emit::session_started(
                    maki_otel::emit::START_FRESH,
                    Some(&id.to_string()),
                );
                if let Some(prompt) = prompt {
                    let _ = self.submit_text(idx, prompt);
                }
                if focus {
                    self.focused = idx;
                }
                let _ = reply_tx.send(Ok(json!(id)));
            }
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
            SessionRequest::Focus { id } => {
                let reply = parse_session_id(&id)
                    .and_then(|id| self.focus_session(id))
                    .map(|()| json!(true));
                let _ = reply_tx.send(reply);
            }
            SessionRequest::SetTitle { id, title } => {
                let title = normalize_title(&title);
                let reply = (|| {
                    let id = parse_session_id(&id)?;
                    if let Some(i) = self.position(id) {
                        self.sessions[i].app.state.session_mut().set_title(title);
                    } else {
                        let mut session =
                            AppSession::load(id, &self.ctx.storage).map_err(|e| e.to_string())?;
                        session.set_title(title);
                        self.ctx.storage_writer.send(Arc::new(session));
                    }
                    Ok(json!(true))
                })();
                let _ = reply_tx.send(reply);
            }
        }
    }

    /// Lua acts on the focused session, the same target the model picker and
    /// `/thinking` write to.
    fn handle_model_request(&mut self, req: ModelRequest) -> UiReply {
        match req {
            ModelRequest::Get => Ok(self.focused_app().model_state()),
            ModelRequest::Available => {
                let available = self.ctx.available_models.load();
                Ok(json!(
                    available.as_deref().map(Vec::as_slice).unwrap_or(&[])
                ))
            }
            ModelRequest::Set {
                spec,
                thinking,
                fast,
            } => {
                if let Some(spec) = spec {
                    self.change_model(&spec)?;
                }
                let app = self.focused_app();
                if let Some(thinking) = thinking {
                    app.set_thinking(&thinking)?;
                }
                if let Some(fast) = fast {
                    app.set_fast(fast)?;
                }
                Ok(app.model_state())
            }
        }
    }

    fn handle_task_request(&mut self, req: TaskRequest) -> UiReply {
        match req {
            TaskRequest::List => Ok(json!(self.focused_app().tasks())),
            TaskRequest::Focus { id } => self.focused_app().focus_task(&id).map(|()| json!(true)),
            TaskRequest::Remove { id } => self.focused_app().remove_task(&id).map(|()| json!(true)),
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

    fn position(&self, id: MakiId) -> Option<usize> {
        self.sessions.iter().position(|rt| rt.id() == id)
    }

    /// No id means the focused session. A plugin holding the id of a tab that
    /// has since closed gets `session not live` back, so it knows to stop.
    fn resolve_session_index(&self, id: Option<&str>) -> Result<usize, String> {
        let Some(id) = id else {
            return Ok(self.focused);
        };
        let parsed = parse_session_id(id)?;
        self.position(parsed).ok_or_else(|| NOT_LIVE_ERR.into())
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
            mode: if app.state.mode == crate::app::mode::Mode::Plan {
                MODE_PLAN
            } else {
                MODE_BUILD
            },
            status: SessionStatus::of(app).as_str(),
            focused: idx == self.focused,
            updated_at: app.state.session.updated_at,
            queue: Some(SessionQueueSnapshot {
                count: app.queue.text_messages().len(),
            }),
            usage: app.state.token_usage,
            context_size: app.state.context_size,
            context_window: app.state.model.context_window,
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

    fn push_runtime(&mut self, rt: SessionRuntime) -> usize {
        self.sessions.push(rt);
        self.sessions.len() - 1
    }

    /// Focus a live session, or bring a stored one up: in place when the
    /// focused session is a blank idle one (nothing worth keeping), otherwise
    /// as a new runtime so the session you came from stays live.
    fn focus_session(&mut self, id: MakiId) -> Result<(), String> {
        if let Some(i) = self.position(id) {
            self.focused = i;
            return Ok(());
        }
        let focused = &mut self.sessions[self.focused];
        if SessionStatus::of(&focused.app) == SessionStatus::Idle && !focused.app.has_content() {
            let actions = focused.app.load_session(id);
            self.dispatch(self.focused, actions);
            return Ok(());
        }
        let session = AppSession::load(id, &self.ctx.storage)
            .map_err(|e| format!("Failed to load session: {e}"))?;
        let idx = self.push_runtime(self.ctx.spawn_runtime(session));
        self.focused = idx;
        Ok(())
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
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.focus.note_input();
                (Some(Msg::Key(key)), None)
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
        while let Ok(next) = self.input.receiver().try_recv() {
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
        while let Ok(next) = self.input.receiver().try_recv() {
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

    fn dispatch(&mut self, idx: usize, actions: Vec<Action>) {
        for action in actions {
            self.handle_action(idx, action);
        }
    }

    fn respawn_agent(&mut self, idx: usize, history: Vec<Message>) {
        let rt = &mut self.sessions[idx];
        rt.reset_run_notifications();
        let lua_handle = rt.app.lua_event_handle.clone();
        let permissions = Arc::clone(&rt.app.permissions);
        rt.handles.respawn(
            history,
            &self.ctx.model_slot,
            self.ctx.config.clone(),
            self.ctx.ui_config.tool_output_lines,
            &permissions,
            &mut rt.app,
            lua_handle,
        );
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
                    image_count: input.images.len(),
                    input,
                    run_id,
                    displayed: true,
                }));
            }
            Action::CancelAgent { run_id } => {
                let rt = &mut self.sessions[idx];
                rt.notifications.reset();
                let _ = rt.handles.cmd_tx.try_send(AgentCommand::Cancel { run_id });
            }
            Action::CancelSubagent { tool_use_id } => {
                let _ = self.sessions[idx]
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::CancelSubagent { tool_use_id });
            }
            Action::NewSession => {
                self.respawn_agent(idx, Vec::new());
            }
            Action::LoadSession(loaded) => {
                let loaded = *loaded;
                if loaded.model_spec != self.ctx.model_slot.load().model.spec()
                    && self.ctx.model_policy.allows(&loaded.model_spec)
                    && let Ok(mut new_model) = Model::from_spec(&loaded.model_spec)
                    && let Ok(new_provider) = from_model(&mut new_model, self.ctx.timeouts)
                {
                    self.sessions[idx].app.usage_slot.store(None);
                    self.ctx.model_slot.store(Arc::new(ModelSlot {
                        model: new_model,
                        provider: Arc::from(new_provider),
                    }));
                }
                self.respawn_agent(idx, loaded.messages);
            }
            Action::ChangeModel(spec) => {
                if let Err(e) = self.change_model(&spec) {
                    self.focused_app().flash(e);
                }
            }
            Action::RefreshProvider { slug } => self.refresh_provider(slug),
            Action::AssignTier(spec, tier) => {
                maki_providers::model_registry::set_and_persist(spec, tier, &self.ctx.storage);
            }
            Action::UnassignTier(spec, tier) => {
                maki_providers::model_registry::unset_and_persist(&spec, tier, &self.ctx.storage);
            }
            Action::Compact(instructions) => {
                let rt = &mut self.sessions[idx];
                rt.reset_run_notifications();
                let run_id = rt.app.run_id;
                rt.handles.queue.push(QueueItem::Compact(Compaction {
                    run_id,
                    instructions,
                }));
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
            Action::EditInputInEditor => {
                let current_text = self.sessions[idx].app.input_box.buffer.value();
                let result = {
                    let _pause = self.input.pause();
                    terminal::edit_temp_content(&current_text, self.terminal)
                };
                self.focus.on_resume();
                match result {
                    Ok(edited) => self.sessions[idx].app.input_box.set_input(edited),
                    Err(e) => self.sessions[idx].app.flash(e),
                }
            }
            Action::Btw(question) => {
                let slot = self.ctx.model_slot.load();
                self.sessions[idx].app.start_btw(
                    question,
                    Arc::clone(&slot.provider),
                    slot.model.clone(),
                );
            }
            Action::PreparePack(command) => self.start_pack(idx, command),
            Action::Suspend => {
                let _pause = self.input.pause();
                terminal::suspend(self.terminal);
                self.focus.on_resume();
            }
            Action::RefreshModels => self.refresh_models(),
            Action::RefreshUsage => self.refresh_usage(),
            Action::ManualExit => self.sessions[idx].notifications.on_manual_exit(),
        }
    }

    fn change_model(&mut self, spec: &str) -> Result<(), String> {
        if !self.ctx.model_policy.allows(spec) {
            return Err(format!("{MODEL_POLICY_ERR}: {spec}"));
        }
        let mut new_model =
            Model::from_spec(spec).map_err(|e| format!("{INVALID_MODEL_ERR}: {e}"))?;
        let new_provider = from_model(&mut new_model, self.ctx.timeouts)
            .map_err(|e| format!("{PROVIDER_INIT_ERR}: {e}"))?;
        let app = self.focused_app();
        app.update_model(&new_model);
        app.record_recent_model(spec);
        app.usage_slot.store(None);
        self.ctx.model_slot.store(Arc::new(ModelSlot {
            model: new_model,
            provider: Arc::from(new_provider),
        }));
        Ok(())
    }

    /// Prepares a package command on its own thread.
    ///
    /// Preparation fetches over the network through a git child process nothing
    /// here can cancel. Inline it would freeze drawing and input for as long as
    /// the slowest remote takes, so the answer comes back as an event instead.
    fn start_pack(&mut self, idx: usize, command: PackCommand) {
        if self.pack_running {
            self.sessions[idx].app.flash(PACK_BUSY_ERR.to_owned());
            return;
        }
        self.pack_running = true;
        self.sessions[idx].app.flash(PACK_PREPARING.to_owned());
        let handle = self.ctx.lua_event_handle.clone();
        let tx = self.pack_tx.clone();
        std::thread::spawn(move || {
            // Without this a panic drops the sender with no answer, and
            // `pack_running` stays set for the rest of the process, so every
            // later package command reports "already running".
            let preparation = catch_unwind(AssertUnwindSafe(|| match handle.package_context() {
                Ok(context) => maki_lua::prepare_pack_command(&command, &context),
                Err(error) => PackPreparation::failed(error),
            }))
            .unwrap_or_else(|_| PackPreparation::failed(PACK_PANIC_ERR.to_owned()));
            let _ = tx.send(Box::new(preparation));
        });
    }

    /// The answer lands on the focused session, not on the one that asked: a
    /// package command reloads the whole Lua host, and only the focused session
    /// is read for an exit request or seen by whoever answers the review.
    fn finish_pack(&mut self, preparation: PackPreparation) {
        self.pack_running = false;
        let actions = self.focused_app().handle_pack_preparation(preparation);
        self.dispatch(self.focused, actions);
    }

    fn refresh_models(&self) {
        let available = Arc::clone(&self.ctx.available_models);
        let warn_tx = self.warn_tx.clone();
        let policy = Arc::clone(&self.ctx.model_policy);
        available.store(None);
        smol::spawn(async move {
            fetch_all_models(
                &policy,
                |batch| merge_batch(&available, batch, &warn_tx),
                None,
            )
            .await;
        })
        .detach();
    }

    fn refresh_usage(&mut self) {
        let provider = Arc::clone(&self.ctx.model_slot.load().provider);
        let slot = Arc::clone(&self.focused_app().usage_slot);
        slot.store(Some(Arc::new(UsageFetchState::Loading)));
        smol::spawn(async move {
            let state = match provider.fetch_usage().await {
                Ok(Some(usage)) => UsageFetchState::Ready(usage),
                Ok(None) => UsageFetchState::Unsupported,
                Err(e) => UsageFetchState::Error(e.user_message()),
            };
            slot.store(Some(Arc::new(state)));
        })
        .detach();
    }

    fn refresh_provider(&mut self, slug: String) {
        let mut model = self.ctx.model_slot.load().model.clone();
        if model.provider.to_string() == slug {
            if let Ok(provider) =
                maki_providers::provider::from_model(&mut model, self.ctx.timeouts)
            {
                self.focused_app().usage_slot.store(None);
                self.ctx.model_slot.store(Arc::new(ModelSlot {
                    model,
                    provider: Arc::from(provider),
                }));
            }
        } else if let Some(builtin) = maki_config::providers::builtin_provider(&slug)
            && let Err(e) = self.change_model(builtin.default_model)
        {
            self.focused_app().flash(e);
        }
    }

    fn shutdown(mut self) -> ShutdownReport {
        let started = Instant::now();
        let mut phase_start = started;
        let mut lap = || {
            let elapsed = phase_start.elapsed().as_millis() as u64;
            phase_start = Instant::now();
            elapsed
        };
        let exit = self.sessions[self.focused].app.exit_request.clone();
        // The loop already stopped draining `UiAction`, so say so before the
        // handlers run. Dropping the receiver does not, since the Lua runtime
        // holds one of its own, and a handler touching the UI would then park
        // until the shared deadline, taking every later tab's `SessionEnd`
        // down with it.
        self.ui_attachment.detach();
        // `/reload` hands these same sessions to the next generation, so
        // their handlers must not hear that the process is quitting.
        let reason = match exit {
            ExitRequest::Reload => SessionEndReason::Reload,
            _ => SessionEndReason::Shutdown,
        };
        self.ctx
            .lua_event_handle
            .end_sessions_blocking(self.sessions.iter().map(SessionRuntime::id), reason);
        let session_end_ms = lap();
        if let Some(ref h) = self.ctx.mcp_handle {
            mcp::kill_process_groups(&h.reader().load().pids);
        }
        for rt in &self.sessions {
            let _ = rt.handles.cmd_tx.try_send(AgentCommand::CancelAll);
        }
        let kill_mcp_ms = lap();
        let mut tabs = Vec::with_capacity(self.sessions.len());
        let mut agent_tasks = Vec::with_capacity(self.sessions.len());
        for rt in self.sessions.drain(..) {
            let SessionRuntime {
                mut app, handles, ..
            } = rt;
            app.checkpoint_now();
            // `app` drops at the end of this iteration, closing the
            // channels the agent loop waits on, so `join_all` can finish.
            tabs.push(Arc::unwrap_or_clone(app.state.session));
            agent_tasks.push(handles.into_task());
        }
        let save_sessions_ms = lap();
        crate::agent::join_all(agent_tasks, AGENT_SHUTDOWN_TIMEOUT);
        let join_agents_ms = lap();
        if let Some(ref h) = self.ctx.mcp_handle {
            smol::block_on(h.shutdown());
        }
        let mcp_shutdown_ms = lap();
        match Arc::try_unwrap(self.ctx.storage_writer) {
            Ok(writer) => writer.shutdown(AGENT_SHUTDOWN_TIMEOUT),
            Err(_) => {
                warn!("storage writer has outstanding references, skipping graceful shutdown")
            }
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
    use maki_agent::DoneReason;
    use maki_providers::TokenUsage;
    use test_case::test_case;

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

    #[test]
    fn completion_waits_for_queue_drain() {
        let mut state = RunNotificationState {
            response_candidate: Some("done".into()),
            ..RunNotificationState::default()
        };

        state.on_done(&done_event());
        assert!(state.waiting_for_drain());
        assert_eq!(state.reconcile(None, SessionStatus::Idle, true), None);

        state.on_drain();
        assert!(!state.waiting_for_drain());
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true),
            Some(Notification::TurnComplete {
                response: Some("done".into())
            })
        );
    }

    #[test_case(SessionStatus::Idle, true, true ; "fires_when_settled")]
    #[test_case(SessionStatus::Idle, false, false ; "queued_message_swallows")]
    #[test_case(SessionStatus::Working, true, false ; "busy_session_swallows")]
    fn due_completion_is_decided_on_first_reconcile(
        status: SessionStatus,
        queue_empty: bool,
        fires: bool,
    ) {
        let mut state = due_completion();

        let first = state.reconcile(None, status, queue_empty);
        assert_eq!(first.is_some(), fires);
        assert_eq!(state.reconcile(None, SessionStatus::Idle, true), None);
    }

    #[test]
    fn prompt_wins_over_completion_and_is_not_repeated_unchanged() {
        let prompt = Notification::QuestionRequested;
        let mut state = due_completion();

        assert_eq!(
            state.reconcile(Some(prompt.clone()), SessionStatus::Idle, true),
            Some(prompt.clone())
        );
        assert_eq!(
            state.reconcile(Some(prompt), SessionStatus::NeedsInput, true),
            None
        );
        assert_eq!(state.reconcile(None, SessionStatus::Idle, true), None);
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
}
