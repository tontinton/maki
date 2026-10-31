use std::future::Future;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use flume::{Receiver, Sender};
use maki_config::{ModelPolicy, ProjectConfig, SessionDefaults};
use maki_providers::model::Model;
use maki_providers::provider::{self, Provider};
use maki_providers::{Message, RequestOptions, Timeouts};
use maki_storage::StateDir;
use maki_storage::id::SessionRef;
use maki_storage::sessions::SessionClaim;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::{sleep_until, timeout};
use tokio_util::task::AbortOnDropHandle;
use tracing::{error, warn};

use crate::agent::{self, RunContext, RunContextBuilder};
use crate::cancel::{CancelMap, CancelToken};
use crate::permissions::{PermissionManager, PluginRuleStore};
use crate::prompt::ResolvedPrompts;
use crate::session::{Resumed, SessionTrack};
use crate::template;
use crate::tools::{
    BackgroundFor, FileAccess, LocalTools, RequestTools, ToolAudience, ToolRegistry,
};
use crate::types::EventSender;
use crate::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentSeed,
    EventStreamGuard, McpHandle, McpSession, ModelSlot, PermissionsConfig, RunLedger, RunTotals,
    SessionEvents, SessionMailbox, ToolDeferral, ToolOutputLines, event_stream,
};

const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const UNREPORTED_HEAD: &str = "background task result(s) never reached the model before";
const UNREPORTED_TAIL: &str = "Tasks still running are stopped, and resuming this session \
     tells the model their results are lost.";
const LAPSED_CAUSE: &str = "--background-timeout ran out";
const STOPPED_CAUSE: &str = "maki was interrupted";
const FAILED_CAUSE: &str = "the last run failed";

/// What a session does with tool calls that move off their turn.
#[derive(Clone, Debug)]
pub enum BackgroundMode {
    /// No seed is published, so nothing can move and every call blocks.
    Blocking,
    /// Results ride the next prompt. Closing the session stops the work.
    Detached,
    /// Like [`Self::Detached`], and once the input ends the session stays up
    /// to this long, running a turn for each result that wakes it.
    KeepAlive(Duration),
}

impl BackgroundMode {
    /// `None` is a zero `--background-timeout`: background work is off.
    pub fn from_wait(wait: Option<Duration>) -> Self {
        wait.map_or(Self::Blocking, Self::KeepAlive)
    }

    fn seeded(&self) -> bool {
        !matches!(self, Self::Blocking)
    }
}

/// Registered before the first run, so the receipts a stored session left
/// unsettled come back as lost notices that run reads.
fn open_mailbox(resumed: &Resumed) -> SessionMailbox {
    let mailbox = SessionMailbox::register(resumed.id.id());
    if let Some(session) = &resumed.session {
        mailbox.restore_lost(&session.meta.pending_background, session.messages());
    }
    mailbox
}

/// The registry holds the seed weakly, so the caller keeps it for the
/// session's life. Without one no call can move.
fn publish_seed(
    mailbox: &SessionMailbox,
    background: &BackgroundMode,
    seed: impl FnOnce() -> AgentSeed,
) -> Option<Arc<AgentSeed>> {
    background.seeded().then(|| mailbox.publish_seed(seed()))
}

/// Background work reads the chat's thinking and mode from the seed, and a
/// run's input is the chat's latest word on both.
fn adopt_chat_settings(seed: Option<&AgentSeed>, input: &AgentInput) {
    if let Some(seed) = seed {
        let opts = RequestOptions {
            thinking: input.thinking,
            fast: input.fast,
        };
        seed.set_chat_settings(opts, &input.mode);
    }
}

fn warn_unreported(unreported: usize, cause: &str) {
    warn!(unreported, cause, "background results unreported");
    if unreported > 0 {
        eprintln!("warning: {unreported} {UNREPORTED_HEAD} {cause}. {UNREPORTED_TAIL}");
    }
}

/// Holds a session open once its input ends while background work is out,
/// so each result can still wake the model. One deadline covers the whole
/// wait, so a chain of wakes cannot keep the process up for good.
struct KeepAlive {
    mailbox: SessionMailbox,
    /// `None` when the session ends with its input.
    wait: Option<Duration>,
    deadline: Option<Instant>,
    next: AfterInput,
}

/// What the session does once its input ends.
enum AfterInput {
    End,
    /// Wakes turns under the last run's settings.
    Wake(AgentInput),
    /// Ends, warning about what never reached the model.
    Fail,
}

impl KeepAlive {
    fn new(mailbox: SessionMailbox, background: BackgroundMode) -> Self {
        let wait = match background {
            BackgroundMode::KeepAlive(wait) => Some(wait),
            BackgroundMode::Blocking | BackgroundMode::Detached => None,
        };
        Self {
            mailbox,
            wait,
            deadline: None,
            next: AfterInput::End,
        }
    }

    fn follow(&mut self, input: &AgentInput) {
        if self.wait.is_some() {
            self.next = AfterInput::Wake(woken_input(input));
        }
    }

    /// Nobody is left to retry a run that failed once the input ends, and a
    /// woken turn would most likely fail the same way. A later prompt arms
    /// the keep-alive again.
    fn fail(&mut self) {
        if matches!(self.next, AfterInput::Wake(_)) {
            self.next = AfterInput::Fail;
        }
    }

    /// The host switched after the last run, so a woken turn runs in the
    /// mode it switched to.
    fn switch_mode(&mut self, mode: AgentMode) {
        if let AfterInput::Wake(next) = &mut self.next {
            next.mode = mode;
        }
    }

    /// The notices of the next wake, or `None` once nothing is out and
    /// nothing woke, or the deadline passed.
    async fn wake(&mut self) -> Option<Vec<Message>> {
        let wait = self.wait?;
        let deadline = *self.deadline.get_or_insert_with(|| Instant::now() + wait);
        let mailbox = &self.mailbox;
        // This also counts results that came in but would wake a turn
        // past the deadline: they die with the process all the same.
        let lapsed = || {
            warn_unreported(mailbox.unsettled().len(), LAPSED_CAUSE);
            None
        };
        // Lapsed first: a wake queued during the last turn is ready at once.
        // A deadline already behind us is checked against the clock, since
        // the timer only reports it after a trip through the driver.
        if Instant::now() >= deadline {
            return lapsed();
        }
        tokio::select! {
            biased;
            () = sleep_until(deadline.into()) => lapsed(),
            wake = mailbox.next_wake() => wake,
        }
    }

    /// The next woken turn, or `None` once the session may end. What still
    /// runs at a lapse dies with the process, and the session meta keeps its
    /// receipts.
    async fn next_input(&mut self) -> Option<AgentInput> {
        let mut input = match mem::replace(&mut self.next, AfterInput::End) {
            AfterInput::End => return None,
            AfterInput::Fail => {
                warn_unreported(self.mailbox.unsettled().len(), FAILED_CAUSE);
                return None;
            }
            AfterInput::Wake(input) => input,
        };
        input.preamble = self.wake().await?;
        Some(input)
    }
}

enum Next {
    Prompt(AgentInput),
    Woken(AgentInput),
}

impl Next {
    /// Whether the turn still runs, on the model it had, once a switch
    /// failed. A prompt's caller has its answer in the error. Woken notices
    /// are already out of the mailbox, so skipping the turn loses them.
    fn survives_failed_switch(&self) -> bool {
        matches!(self, Self::Woken(_))
    }

    fn into_input(self) -> AgentInput {
        match self {
            Self::Prompt(input) | Self::Woken(input) => input,
        }
    }
}

/// Where an interactive session's turns come from.
struct Inputs {
    prompts: Receiver<AgentInput>,
    modes: Receiver<AgentMode>,
    keep_alive: KeepAlive,
    /// Ends the session at once, whatever is still out.
    stop: CancelToken,
}

impl Inputs {
    /// While prompts can come, results ride them and nothing wakes a turn.
    /// Once they end, the keep-alive wakes turns, in the mode the host last
    /// switched to. Every prompt carries the mode current when it was sent,
    /// so the last switch is never older than the last prompt.
    ///
    /// The stop wins over an input that is ready in the same poll, so a
    /// stopped session never takes notices it would not deliver.
    async fn next(&mut self) -> Option<Next> {
        let stop = self.stop.clone();
        let next = async {
            if let Ok(input) = self.prompts.recv_async().await {
                return Some(Next::Prompt(input));
            }
            if let Some(mode) = self.modes.try_iter().last() {
                self.keep_alive.switch_mode(mode);
            }
            self.keep_alive.next_input().await.map(Next::Woken)
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => None,
            next = next => next,
        }
    }

    /// What never reached the model dies with the process once stopped.
    fn warn_if_stopped(&self) {
        if self.stop.is_cancelled() {
            warn_unreported(self.keep_alive.mailbox.unsettled().len(), STOPPED_CAUSE);
        }
    }
}

/// Nobody prompted this turn: the notices it is woken with are all it says.
fn woken_input(like: &AgentInput) -> AgentInput {
    AgentInput {
        message: String::new(),
        mode: like.mode.clone(),
        images: Vec::new(),
        preamble: Vec::new(),
        earlier: Vec::new(),
        thinking: like.thinking,
        fast: like.fast,
        workflow: like.workflow,
        prompt: None,
        source: like.source,
    }
}

/// Resolve a provider, or report the failure on the event stream and let the
/// caller give up. Every site that reaches a provider comes through here, so a
/// failed connect cannot be silent on one path and loud on another. What giving
/// up means is the caller's call: abandon the session, or skip this turn.
async fn connect(
    model: &mut Model,
    timeouts: Timeouts,
    event_tx: &EventSender,
) -> Option<Arc<dyn Provider>> {
    match provider::from_model_async(model, timeouts).await {
        Ok(p) => Some(Arc::from(p)),
        Err(e) => {
            error!(error = %e, "provider error");
            let _ = event_tx.send(AgentEvent::error(&e, RunTotals::default()));
            None
        }
    }
}

/// What a session task ends with, before its MCP is torn down: the track
/// is out of the task whatever that teardown does, so it never drops on an
/// executor thread after the caller stopped looking.
#[derive(Default)]
pub struct SessionEnd {
    /// `None` when no provider connected. Drop it once the session's
    /// background work is stopped: that writes what the work spent after the
    /// last turn.
    pub track: Option<SessionTrack>,
    mcp: Option<McpHandle>,
}

pub type SessionTask = AbortOnDropHandle<SessionEnd>;

/// Read once at startup. A headless host has no `/cd` and its slots come from
/// the caller, so nothing here changes mid-session.
struct ContextSpec {
    vars: template::Vars,
    instructions: String,
    slots: Arc<ResolvedPrompts>,
    config: AgentConfig,
    excluded_tools: Vec<&'static str>,
    mcp: bool,
    background: bool,
    system_override: Option<String>,
    append_system: Option<String>,
}

impl ContextSpec {
    /// Takes the handle rather than a `bool`. Two flags side by side are one
    /// silent swap away from a session that describes tools it cannot call.
    fn new(
        vars: template::Vars,
        instructions: &agent::Instructions,
        slots: Arc<ResolvedPrompts>,
        config: AgentConfig,
        excluded_tools: Vec<&'static str>,
        mcp: Option<&McpHandle>,
    ) -> Self {
        Self {
            vars,
            instructions: instructions.text.clone(),
            slots,
            config,
            excluded_tools,
            mcp: mcp.is_some(),
            background: true,
            system_override: None,
            append_system: None,
        }
    }

    fn render(&self, model: &Model, workflow: bool) -> RunContext {
        let tools = RequestTools::build_with_background(
            ToolRegistry::global(),
            &self.vars,
            model,
            &self.config,
            &self.excluded_tools,
            workflow,
            self.mcp,
            self.background,
        );
        let mut context = match &self.system_override {
            Some(system) => {
                RunContext::fixed(system.clone(), tools).with_prompt_policy(&self.slots)
            }
            None => RunContext::render(&self.vars, &self.instructions, &self.slots, model, tools),
        };
        if let Some(append) = &self.append_system {
            for text in [&mut context.system, &mut context.authored] {
                text.push('\n');
                text.push_str(append);
            }
        }
        context
    }

    fn builder(self) -> RunContextBuilder {
        Arc::new(move |model, workflow| self.render(model, workflow))
    }
}

/// Names advertised to SDK clients: base tools plus what the first request
/// would put in the model's context from MCP (always-load definitions and
/// `tool_search`). Probed as [`ToolDeferral::Client`] because the native
/// array lists every deferred definition too.
fn advertised_tool_names(tools: &Value, mcp: Option<&McpSession>) -> Vec<String> {
    let mut probe = tools.clone();
    if let Some(mcp) = mcp {
        mcp.extend_tools(&mut probe, ToolDeferral::Client);
    }
    extract_tool_names(&probe)
}

pub struct InteractiveParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt_slots: Arc<ResolvedPrompts>,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub resumed: Resumed,
    /// The right to write [`Self::resumed`]'s session, taken before its
    /// transcript was read.
    pub claim: SessionClaim,
    /// Where the transcript is written. The same dir the caller resolved the
    /// session from, so a run cannot read one session and write another.
    pub storage: StateDir,
    pub yolo: bool,
    /// Nobody answers permission prompts, so they are denied at once instead
    /// of waiting forever.
    pub unattended: bool,
    pub system_prompt_override: Option<String>,
    pub append_system_prompt: Option<String>,
    /// The `always_*` knobs. `workflow` picks the tool catalog here; the rest
    /// are what a host without toggles puts on every [`AgentInput`] it sends.
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub project_config: ProjectConfig,
    /// Host-side overrides that shadow a registered tool's execution while
    /// keeping its advertised schema (e.g. ACP answers `question` via elicitation).
    pub local_tools: LocalTools,
    /// While input is open, results ride the next prompt. Only
    /// [`BackgroundMode::KeepAlive`] wakes the model on its own, and only once
    /// the input has ended.
    pub background: BackgroundMode,
    /// Firing it cancels the run in flight and ends the session without
    /// waiting for background work. Prompts still queued never run.
    pub stop: CancelToken,
}

pub struct InteractiveHandle {
    pub tool_names: Vec<String>,
    pub input_tx: Sender<AgentInput>,
    pub answer_tx: Sender<String>,
    pub cancel_tx: Sender<()>,
    pub model_tx: Sender<Model>,
    /// Send through [`Self::switch_mode`], which also tells background work.
    pub mode_tx: Sender<AgentMode>,
    pub session_id: SessionRef,
    pub permissions: Arc<PermissionManager>,
    pub task: SessionTask,
    /// What the session spends in this process, background work included.
    /// Final once that work is stopped.
    pub spend: Arc<RunLedger>,
}

impl InteractiveHandle {
    /// A switch between prompts reaches background work at once, so a switch
    /// to plan mode blocks its next write, and a turn that work wakes once
    /// the input ended runs in the new mode.
    pub fn switch_mode(&self, mode: AgentMode) {
        if let Some(seed) = AgentSeed::live(self.session_id.id()) {
            seed.set_chat_settings(seed.chat_opts(), &mode);
        }
        let _ = self.mode_tx.send(mode);
    }
}

pub fn spawn_interactive(params: InteractiveParams) -> (InteractiveHandle, SessionEvents) {
    let vars = template::env_vars();
    let instructions = agent::load_instructions(&vars.apply("{cwd}"), &params.config.instructions);
    let spec = ContextSpec {
        system_override: params.system_prompt_override,
        append_system: params.append_system_prompt,
        background: params.background.seeded(),
        ..ContextSpec::new(
            vars,
            &instructions,
            Arc::clone(&params.prompt_slots),
            params.config.clone(),
            params.excluded_tools,
            params.mcp_handle.as_ref(),
        )
    };
    let initial = spec.render(&params.model, params.defaults.workflow);
    let context = spec.builder();

    let mcp = params
        .mcp_handle
        .clone()
        .map(|h| McpSession::new(h, &params.resumed.history));
    let tool_names = advertised_tool_names(initial.tools.definitions(), mcp.as_ref());

    let (guard, events) = event_stream();
    let base_tx = guard.sender(0);
    let (input_tx, input_rx) = flume::unbounded::<AgentInput>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (cancel_tx, cancel_rx) = flume::bounded::<()>(1);
    let (model_tx, model_rx) = flume::unbounded::<Model>();
    let (mode_tx, mode_rx) = flume::unbounded::<AgentMode>();

    let session_ref = params.resumed.id.clone();
    let mailbox = open_mailbox(&params.resumed);

    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let mut permissions_config = params.permissions_config;
    permissions_config.yolo |= params.yolo;
    let permissions = Arc::new(PermissionManager::new(
        permissions_config,
        params.initial_wd,
        params.project_config,
        Arc::clone(&params.plugin_rules),
    ));

    let answer_rx = Arc::new(Mutex::new(answer_rx));

    let session_ref_clone = session_ref.clone();
    let task_permissions = Arc::clone(&permissions);
    let spend = Arc::<RunLedger>::default();
    let track_spend = Arc::clone(&spend);
    let task = tokio::spawn(run_session(guard, params.mcp_handle.clone(), async move {
        let mut model = params.model;
        let mut provider = connect(&mut model, params.timeouts, &base_tx).await?;
        let mut track = SessionTrack::open(
            params.resumed,
            params.claim,
            params.storage,
            mailbox.clone(),
            track_spend,
            &working_dir,
        );
        let seed = publish_seed(&mailbox, &params.background, || {
            AgentSeed::new(
                session_ref_clone.clone(),
                Arc::new(ArcSwap::from_pointee(ModelSlot {
                    model: model.clone(),
                    provider: Arc::clone(&provider),
                })),
                params.config.clone(),
                ToolOutputLines::default(),
                Arc::clone(&task_permissions),
                mcp.clone(),
                params.timeouts,
                Arc::clone(&params.model_policy),
            )
        });
        let file_access = seed
            .as_ref()
            .map_or_else(FileAccess::fresh, |seed| Arc::clone(&seed.file_access));
        let mut inputs = Inputs {
            prompts: input_rx,
            modes: mode_rx,
            keep_alive: KeepAlive::new(mailbox.clone(), params.background),
            stop: params.stop.clone(),
        };
        let mut run_id: u64 = 0;

        while let Some(next) = inputs.next().await {
            let (trigger, cancel) = params.stop.child();
            let cancel_task = tokio::spawn({
                let cancel_rx = cancel_rx.clone();
                async move {
                    if cancel_rx.recv_async().await.is_ok() {
                        trigger.cancel();
                    }
                }
            });

            // MCP connects in the background, so a prompt that beats it waits
            // here instead of shipping a turn without the MCP tools. The wait
            // is racing cancel: a slow server must not pin the whole session.
            if let Some(mcp) = &mcp {
                let _ = cancel.race(mcp.ready()).await;
            }

            let event_tx = base_tx.with_run_id(run_id);
            let error_tx = event_tx.clone();

            if let Some(mut new_model) = model_rx
                .try_iter()
                .last()
                .filter(|candidate| params.model_policy.allows(&candidate.spec()))
                && new_model.spec() != model.spec()
            {
                match connect(&mut new_model, params.timeouts, &error_tx).await {
                    Some(switched) => {
                        provider = switched;
                        model = new_model;
                        if let Some(seed) = &seed {
                            seed.model_slot.store(Arc::new(ModelSlot {
                                model: model.clone(),
                                provider: Arc::clone(&provider),
                            }));
                        }
                    }
                    None if !next.survives_failed_switch() => {
                        run_id += 1;
                        continue;
                    }
                    None => {}
                }
            }
            let input = next.into_input();

            // An ask holds this lock from before its request goes out until
            // its answer lands, so a free lock means nothing queued has a
            // taker. A call moved to the background can still be parked on
            // it after its run ended: then whatever is queued may be its
            // answer, and the new run must not wait for it to arrive.
            if let Ok(answers) = answer_rx.try_lock() {
                while answers.try_recv().is_ok() {}
            }

            adopt_chat_settings(seed.as_deref(), &input);
            inputs.keep_alive.follow(&input);

            // Outlives `agent`, so this turn is on disk before the next prompt
            // is read, without the loop body remembering to write it.
            let mut turn = track.turn(model.spec(), &input);
            let mut agent = Agent::new(
                AgentParams {
                    provider: Arc::clone(&provider),
                    model: model.clone(),
                    config: params.config.clone(),
                    tool_output_lines: ToolOutputLines::default(),
                    permissions: Arc::clone(&task_permissions),
                    session_id: Some(session_ref_clone.clone()),
                    background_for: BackgroundFor::None,
                    task_id: None,
                    mailbox: Some(mailbox.clone()),
                    timeouts: params.timeouts,
                    file_access: Arc::clone(&file_access),
                    prompt_slots: Arc::clone(&params.prompt_slots),
                    subagent_cancels: Arc::new(CancelMap::new()),
                    ledger: turn.ledger(),
                    registry: Arc::clone(ToolRegistry::global_arc()),
                    audience: ToolAudience::MAIN,
                    model_policy: Arc::clone(&params.model_policy),
                },
                turn.run_params(event_tx, Arc::clone(&context)),
            )
            .with_loaded_instructions(instructions.loaded.clone(), instructions.files.clone())
            .with_cancel(cancel)
            .with_local_tools(Arc::clone(&params.local_tools))
            .with_mcp(mcp.clone());
            if !params.unattended {
                agent = agent.with_user_response_rx(Arc::clone(&answer_rx));
            }

            let result = agent.run(input).await;
            drop(agent);
            cancel_task.abort();
            let _ = cancel_task.await;

            if let Err(ref e) = result {
                error!(error = %e, "agent error");
                let _ = error_tx.send(AgentEvent::error(e, turn.ledger().totals()));
                inputs.keep_alive.fail();
            }

            run_id += 1;
        }
        inputs.warn_if_stopped();
        Some(track)
    }));

    (
        InteractiveHandle {
            tool_names,
            input_tx,
            answer_tx,
            cancel_tx,
            model_tx,
            mode_tx,
            session_id: session_ref,
            permissions,
            task: AbortOnDropHandle::new(task),
            spend,
        },
        events,
    )
}

/// Waits for a session task, then tears its MCP down. The task itself is
/// never bounded: one with work left would drop prompts stdin already queued.
pub async fn await_shutdown(task: SessionTask) -> Option<SessionTrack> {
    task.await.expect("session task panicked").shut_down().await
}

impl SessionEnd {
    /// Gives up on a teardown that wedges. The track comes back either way.
    async fn shut_down(self) -> Option<SessionTrack> {
        if let Some(mcp) = self.mcp {
            let _ = timeout(SESSION_SHUTDOWN_TIMEOUT, mcp.shutdown()).await;
        }
        self.track
    }
}

/// Runs a session body and ends its event stream with it. Teardown is
/// [`await_shutdown`]'s: a wedged shutdown must neither keep a consumer
/// waiting for events that can no longer come nor hold on to the track.
async fn run_session(
    guard: EventStreamGuard,
    mcp: Option<McpHandle>,
    body: impl Future<Output = Option<SessionTrack>>,
) -> SessionEnd {
    let track = body.await;
    drop(guard);
    SessionEnd { track, mcp }
}

fn extract_tool_names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::FutureExt;
    use maki_storage::id::MakiId;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::live_session::BACKGROUND_PLAN_WRITE_DENIED;
    use crate::mcp::McpCommand;
    use crate::prompt::{PromptId, Slot, SlotEntry};
    use crate::session::StoredSession;
    use crate::tools::test_support::stub_seed;
    use crate::{BackgroundNotice, InputSource, TaskEnd};

    const RUN_ID: u64 = 7;
    const TASK_ID: &str = "toolu_bg";
    const NOTICE_BODY: &str = "build finished";
    const PROMPT: &str = "start the build";
    const PLAN_PATH: &str = "/project/plan.md";
    const CWD: &str = "/project";
    const LONG_WAIT: Duration = Duration::from_secs(3600);
    const WAITS_FOR_WORK: &str = "the keep-alive must wait while a receipt is outstanding";
    const ENDS_WHEN_IDLE: &str = "nothing out and nothing woke: the session may end";
    const KEPT_FOR_META: &str = "a lapsed task stays unsettled, so the session meta keeps it";
    const STREAM_ENDED: &str = "the stream must end with the run, not with teardown";
    const SHUTDOWN_WEDGED: &str = "shutting down must send the MCP shutdown and wait for its ack";
    const TEARDOWN_AFTER_TRACK: &str =
        "MCP teardown must not start before the task hands back its track";
    const TRACK_RETURNED: &str = "shutting down must hand back the track";
    const PROVIDER_ERROR: &str = "provider error";
    const BODY_EVENT: &str = "the body's event must be delivered before the close";
    const STILL_WORKING: &str = "await_shutdown must not return while the task still works";
    const TASK_DROPPED: &str = "await_shutdown dropped a task that had work left";
    const PROMPT_ARRIVES: &str = "a sent prompt is the next input";
    const PROMPT_ANSWERED_BY_ERROR: &str = "a failed switch answers a prompt with its error";
    const NO_WAKE_WHILE_OPEN: &str = "a result must ride the next prompt while input is open";
    const WAKES_AFTER_INPUT_END: &str = "a waking result runs a turn once the input ended";
    const WOKEN_NOTICES_KEPT: &str = "a woken turn's notices must survive a failed switch";
    const RUNS_IN_CURRENT_MODE: &str = "a woken turn must run in the mode the host switched to";
    const STOP_ENDS_SESSION: &str = "a stop must end the session without another turn";
    const FAILED_RUN_ENDS_SESSION: &str = "a failed run must not be woken once the input ended";

    /// A host-written prompt never had maki's facts in it. If the frame
    /// claimed it did, a later edit would be announced to a model that never
    /// saw the original.
    #[test_case(None, true ; "rendered_prompt_states_them")]
    #[test_case(Some("custom"), false ; "override_leaves_them_out")]
    fn override_states_no_facts(system_override: Option<&str>, tracked: bool) {
        let mut slots = ResolvedPrompts::default();
        slots.insert(
            PromptId::System,
            Slot::AfterInstructions,
            SlotEntry {
                plugin: Arc::from("memory"),
                content: "tags: a".into(),
            },
        );
        let instructions = agent::Instructions {
            text: "\n\nProject instructions:\nbe nice".into(),
            ..Default::default()
        };
        let spec = ContextSpec {
            system_override: system_override.map(String::from),
            ..ContextSpec::new(
                template::Vars::new(),
                &instructions,
                Arc::new(slots),
                AgentConfig::default(),
                Vec::new(),
                None,
            )
        };
        let model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let facts = spec.render(&model, false).facts;
        assert_eq!(
            facts.is_some_and(|f| !f.instructions.is_empty() && !f.hints.is_empty()),
            tracked
        );
    }

    #[test_case(None, "You analyse information." ; "plugin_template")]
    #[test_case(Some("Explicit SDK policy."), "Explicit SDK policy." ; "sdk_precedence")]
    fn headless_prompt_replacement_keeps_explicit_sdk_precedence(
        explicit: Option<&str>,
        expected: &str,
    ) {
        const PLUGIN_SYSTEM: &str = "You analyse information.";
        const OMITTED_INSTRUCTIONS: &str = "Project instructions must not be appended.";
        let mut prompts = ResolvedPrompts::default();
        prompts.set_template(PromptId::System, PLUGIN_SYSTEM.into());
        let spec = ContextSpec {
            system_override: explicit.map(String::from),
            ..ContextSpec::new(
                template::Vars::new(),
                &agent::Instructions {
                    text: OMITTED_INSTRUCTIONS.into(),
                    ..Default::default()
                },
                Arc::new(prompts),
                AgentConfig::default(),
                Vec::new(),
                None,
            )
        };
        let model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let context = spec.render(&model, false);
        assert_eq!(context.system, expected);
        assert_eq!(context.facts.is_none(), explicit.is_some());
        assert!(!context.system.contains(OMITTED_INSTRUCTIONS));
    }

    #[test]
    fn extract_tool_names_filters_valid_entries() {
        let tools = serde_json::json!([{"name": "read"}, {"type": "function"}, {"name": "bash"}]);
        assert_eq!(extract_tool_names(&tools), vec!["read", "bash"]);
    }

    #[test]
    fn advertised_names_show_tool_search_not_deferred_tools() {
        let base = serde_json::json!([{"name": "read"}]);
        let mcp =
            crate::mcp::test_support::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
        let names = advertised_tool_names(&base, Some(&mcp));
        assert_eq!(
            names,
            vec!["read", crate::mcp::TOOL_SEARCH_TOOL_NAME],
            "clients must see the search tool, not deferred definitions"
        );
        assert_eq!(
            base,
            serde_json::json!([{"name": "read"}]),
            "probing must not bake MCP entries into the base tools"
        );
        assert_eq!(advertised_tool_names(&base, None), vec!["read"]);
    }

    /// The ordering the SDK and `-p` exits rest on: the stream ends and the
    /// track is out of the task when the run ends, before MCP teardown
    /// starts, so a wedged teardown can neither hold the consumer nor drop
    /// the track on an executor thread. The retained sender is the Lua tool
    /// context an idle VM never collects.
    #[tokio::test]
    async fn the_run_ends_the_stream_and_hands_back_its_track_before_mcp_teardown() {
        let tmp = TempDir::new().unwrap();
        let id = MakiId::generate();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let claim = SessionClaim::acquire(id, &dir).expect("nothing else holds it");
        let mailbox = SessionMailbox::register(id);
        let track = SessionTrack::open(
            Resumed::empty(SessionRef::from(id)),
            claim,
            dir,
            mailbox,
            Arc::default(),
            CWD,
        );
        let (guard, mut events) = event_stream();
        let retained = guard.sender(RUN_ID);
        let event_tx = retained.clone();
        let (cmd_tx, cmd_rx) = flume::unbounded();

        let ended = run_session(guard, Some(McpHandle::for_test(cmd_tx)), async move {
            let _ = event_tx.send(AgentEvent::Error {
                message: PROVIDER_ERROR.into(),
                auth: false,
                totals: RunTotals::default(),
            });
            Some(track)
        })
        .await;
        assert!(cmd_rx.is_empty(), "{TEARDOWN_AFTER_TRACK}");

        let envelope = events.next().await.expect(BODY_EVENT);
        assert!(matches!(
            envelope.event,
            AgentEvent::Error { message, .. } if message == PROVIDER_ERROR
        ));
        assert!(
            matches!(events.next().now_or_never(), Some(None)),
            "{STREAM_ENDED}"
        );

        let mut shutdown = pin!(ended.shut_down());
        assert!(
            shutdown.as_mut().now_or_never().is_none(),
            "{SHUTDOWN_WEDGED}"
        );
        let Ok(McpCommand::Shutdown { ack }) = cmd_rx.try_recv() else {
            panic!("{SHUTDOWN_WEDGED}");
        };
        ack.send(()).unwrap();
        assert!(shutdown.await.is_some(), "{TRACK_RETURNED}");
        assert!(retained.send(AgentEvent::Nudge).is_ok());
    }

    fn prompt_input() -> AgentInput {
        prompt_in(plan_mode())
    }

    fn plan_mode() -> AgentMode {
        AgentMode::Plan(PLAN_PATH.into())
    }

    fn prompt_in(mode: AgentMode) -> AgentInput {
        AgentInput::from_defaults(
            PROMPT.into(),
            mode,
            Vec::new(),
            SessionDefaults::default(),
            InputSource::Headless,
        )
    }

    fn settle_waking(mailbox: &SessionMailbox) {
        mailbox.settle(
            TASK_ID,
            Some(BackgroundNotice {
                text: NOTICE_BODY.into(),
                display: None,
                wake: true,
                end: TaskEnd::Finished,
            }),
        );
    }

    fn inputs_for(
        mailbox: &SessionMailbox,
        background: BackgroundMode,
        stop: CancelToken,
    ) -> (Inputs, Sender<AgentInput>, Sender<AgentMode>) {
        let (prompt_tx, prompts) = flume::unbounded();
        let (mode_tx, modes) = flume::unbounded();
        let inputs = Inputs {
            prompts,
            modes,
            keep_alive: KeepAlive::new(mailbox.clone(), background),
            stop,
        };
        (inputs, prompt_tx, mode_tx)
    }

    /// While the host can still prompt, a result rides its next prompt. Once
    /// the input ends, the result wakes a turn of its own, and that turn's
    /// notices are already out of the mailbox, so a failed model switch must
    /// not skip it the way it skips a prompt.
    #[tokio::test]
    async fn only_input_end_wakes_a_turn_and_a_failed_switch_keeps_it() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        let (mut inputs, prompt_tx, _mode_tx) = inputs_for(
            &mailbox,
            BackgroundMode::KeepAlive(LONG_WAIT),
            CancelToken::none(),
        );
        mailbox.track(TASK_ID);

        prompt_tx.send(prompt_in(AgentMode::Build)).unwrap();
        let prompt = inputs.next().await.expect(PROMPT_ARRIVES);
        assert!(
            !prompt.survives_failed_switch(),
            "{PROMPT_ANSWERED_BY_ERROR}"
        );
        inputs.keep_alive.follow(&prompt.into_input());

        settle_waking(&mailbox);
        assert!(
            inputs.next().now_or_never().is_none(),
            "{NO_WAKE_WHILE_OPEN}"
        );

        drop(prompt_tx);
        let woken = inputs.next().await.expect(WAKES_AFTER_INPUT_END);
        assert!(woken.survives_failed_switch(), "{WOKEN_NOTICES_KEPT}");
        let woken = woken.into_input();
        assert_eq!(woken.preamble.len(), 1);
        assert_eq!(woken.preamble[0].first_text_content(), Some(NOTICE_BODY));
    }

    /// The host switched to plan mode after its last prompt, then ended its
    /// input. The woken turn must run in plan mode, and adopting its settings
    /// must leave background work blocked from writing.
    #[tokio::test]
    async fn a_woken_turn_runs_in_the_mode_switched_to_after_the_last_prompt() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let background = BackgroundMode::KeepAlive(LONG_WAIT);
        let seed = publish_seed(&mailbox, &background, || stub_seed(id));
        let (mut inputs, prompt_tx, mode_tx) =
            inputs_for(&mailbox, background, CancelToken::none());
        mailbox.track(TASK_ID);

        let woken = async {
            prompt_tx.send(prompt_in(AgentMode::Build)).unwrap();
            let prompt = inputs.next().await.expect(PROMPT_ARRIVES).into_input();
            adopt_chat_settings(seed.as_deref(), &prompt);
            inputs.keep_alive.follow(&prompt);

            mode_tx.send(plan_mode()).unwrap();
            drop(prompt_tx);
            settle_waking(&mailbox);
            inputs
                .next()
                .await
                .expect(WAKES_AFTER_INPUT_END)
                .into_input()
        }
        .await;
        adopt_chat_settings(seed.as_deref(), &woken);

        assert_eq!(woken.mode, plan_mode(), "{RUNS_IN_CURRENT_MODE}");
        assert_eq!(
            AgentSeed::background_write_block(id).as_deref(),
            Some(BACKGROUND_PLAN_WRITE_DENIED),
            "{RUNS_IN_CURRENT_MODE}"
        );
    }

    /// `-p` is this with one prompt. A failed run is never woken again, so
    /// the session ends at once, and the result it would have woken with
    /// stays unsettled for the warning and the session meta.
    #[tokio::test]
    async fn a_failed_run_ends_the_session_once_the_input_ends() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        let (mut inputs, prompt_tx, _mode_tx) = inputs_for(
            &mailbox,
            BackgroundMode::KeepAlive(LONG_WAIT),
            CancelToken::none(),
        );
        mailbox.track(TASK_ID);

        prompt_tx.send(prompt_in(AgentMode::Build)).unwrap();
        drop(prompt_tx);
        let prompt = inputs.next().await.expect(PROMPT_ARRIVES).into_input();
        inputs.keep_alive.follow(&prompt);
        inputs.keep_alive.fail();

        settle_waking(&mailbox);
        assert!(inputs.next().await.is_none(), "{FAILED_RUN_ENDS_SESSION}");
        assert_eq!(mailbox.unsettled(), [TASK_ID], "{KEPT_FOR_META}");
    }

    #[test]
    fn open_mailbox_restores_the_receipts_a_stored_session_left() {
        let mut stored = StoredSession::new("", CWD);
        stored.meta.pending_background = vec![TASK_ID.into()];
        let resumed = Resumed::stored(SessionRef::from(MakiId::generate()), stored);

        let mailbox = open_mailbox(&resumed);

        assert_eq!(mailbox.unsettled(), [TASK_ID]);
    }

    #[test_case(BackgroundMode::from_wait(None), false ; "zero_timeout")]
    #[test_case(BackgroundMode::Detached, true ; "detached")]
    #[test_case(BackgroundMode::from_wait(Some(LONG_WAIT)), true ; "keep_alive")]
    fn only_a_zero_timeout_publishes_no_seed(background: BackgroundMode, seeded: bool) {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);

        let seed = publish_seed(&mailbox, &background, || stub_seed(id));

        assert_eq!(seed.is_some(), seeded);
        assert_eq!(AgentSeed::live(id).is_some(), seeded);
    }

    #[tokio::test]
    async fn keep_alive_runs_a_woken_turn_after_a_settled_receipt_then_ends() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        let mut keep_alive = KeepAlive::new(mailbox.clone(), BackgroundMode::KeepAlive(LONG_WAIT));
        mailbox.track(TASK_ID);
        keep_alive.follow(&prompt_input());

        let woken = {
            let mut next = pin!(keep_alive.next_input());
            assert!(next.as_mut().now_or_never().is_none(), "{WAITS_FOR_WORK}");
            settle_waking(&mailbox);
            next.await.expect(WAKES_AFTER_INPUT_END)
        };

        assert!(woken.message.is_empty());
        assert_eq!(woken.mode, plan_mode());
        assert_eq!(woken.preamble.len(), 1);
        assert_eq!(woken.preamble[0].first_text_content(), Some(NOTICE_BODY));

        keep_alive.follow(&woken);
        assert!(keep_alive.next_input().await.is_none(), "{ENDS_WHEN_IDLE}");
    }

    /// With a zero wait the deadline has passed by the first wake. A wake
    /// queued while the last turn ran is ready at once too, and it must not
    /// outrun the deadline, or a chain of them would keep the process up.
    #[test_case(false ; "while_work_is_still_out")]
    #[test_case(true ; "over_a_queued_wake")]
    #[tokio::test]
    async fn keep_alive_lapses(wake_queued: bool) {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        if wake_queued {
            settle_waking(&mailbox);
        }
        let mut keep_alive =
            KeepAlive::new(mailbox.clone(), BackgroundMode::KeepAlive(Duration::ZERO));
        keep_alive.follow(&prompt_input());

        assert!(keep_alive.next_input().await.is_none());
        assert_eq!(mailbox.unsettled(), [TASK_ID], "{KEPT_FOR_META}");
    }

    #[tokio::test]
    async fn a_stop_ends_the_keep_alive_wait_while_work_is_out() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        let (trigger, stop) = CancelToken::new();
        let (mut inputs, prompt_tx, _mode_tx) =
            inputs_for(&mailbox, BackgroundMode::KeepAlive(LONG_WAIT), stop);
        drop(prompt_tx);
        inputs.keep_alive.follow(&prompt_input());

        let mut next = pin!(inputs.next());
        assert!(next.as_mut().now_or_never().is_none(), "{WAITS_FOR_WORK}");
        trigger.cancel();
        assert!(next.await.is_none(), "{STOP_ENDS_SESSION}");
        assert_eq!(mailbox.unsettled(), [TASK_ID], "{KEPT_FOR_META}");
    }

    /// A stopped session takes nothing it would not deliver: no prompt still
    /// queued, and no woken notices, which then stay unsettled for the meta.
    #[test_case(true ; "a_queued_prompt")]
    #[test_case(false ; "a_queued_wake")]
    #[tokio::test]
    async fn a_stop_wins_over_a_ready_input(prompt_queued: bool) {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        let (trigger, stop) = CancelToken::new();
        let (mut inputs, prompt_tx, _mode_tx) =
            inputs_for(&mailbox, BackgroundMode::KeepAlive(LONG_WAIT), stop);
        if prompt_queued {
            prompt_tx.send(prompt_input()).unwrap();
        } else {
            drop(prompt_tx);
            inputs.keep_alive.follow(&prompt_input());
        }
        settle_waking(&mailbox);
        trigger.cancel();

        assert!(inputs.next().await.is_none(), "{STOP_ENDS_SESSION}");
        assert_eq!(mailbox.unsettled(), [TASK_ID], "{KEPT_FOR_META}");
    }

    /// ACP's mode: results ride the next prompt, and nothing holds the
    /// session open once its input ends.
    #[tokio::test]
    async fn detached_never_keeps_the_session_alive() {
        let mailbox = SessionMailbox::register(MakiId::generate());
        mailbox.track(TASK_ID);
        let mut keep_alive = KeepAlive::new(mailbox, BackgroundMode::Detached);

        keep_alive.follow(&prompt_input());

        assert!(keep_alive.next_input().await.is_none());
    }

    /// `await_shutdown` bounds teardown, not the session: a task with work left
    /// runs to completion. Dropping it here would cancel prompts stdin already
    /// queued.
    #[tokio::test]
    async fn await_shutdown_waits_for_a_task_that_still_works() {
        let (release_tx, release_rx) = flume::bounded::<()>(1);
        let finished = Arc::new(AtomicBool::new(false));
        let task = AbortOnDropHandle::new(tokio::spawn({
            let finished = Arc::clone(&finished);
            async move {
                let _ = release_rx.recv_async().await;
                finished.store(true, Ordering::SeqCst);
                SessionEnd::default()
            }
        }));

        let mut shutdown = pin!(await_shutdown(task));
        assert!(
            shutdown.as_mut().now_or_never().is_none(),
            "{STILL_WORKING}"
        );
        release_tx.send(()).unwrap();
        shutdown.await;
        assert!(finished.load(Ordering::SeqCst), "{TASK_DROPPED}");
    }
}
