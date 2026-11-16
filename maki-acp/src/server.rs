use std::collections::HashMap;
use std::fmt::Display;
use std::iter;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::v1::{
    AgentNotification, AgentRequest, AgentResponse, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, ConfigOptionUpdate, ContentBlock, CurrentModeUpdate,
    EmbeddedResourceResource, Error as AcpError, ImageContent, InitializeRequest, JsonRpcMessage,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, McpServer, NewSessionRequest,
    Notification, PromptRequest, PromptResponse, Request, RequestId, RequestPermissionRequest,
    RequestPermissionResponse, Response, SessionConfigOptionValue, SessionId, SessionInfo,
    SessionModeId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
    TextContent,
};
use color_eyre::eyre::Context;
use futures::future::join_all;
use maki_agent::command::{CustomCommand, discover_commands};
use maki_agent::headless::{
    self, BackgroundMode, InteractiveHandle, InteractiveParams, SessionInput,
};
use maki_agent::mcp::config::{RawHttpFields, RawStdioFields, RawTransport};
use maki_agent::mcp::{self, McpHandle};
use maki_agent::permissions::{PermissionAnswer, TaggedAnswer};
use maki_agent::session::{Resumed, StoredSession};
use maki_agent::tools::{LocalTool, LocalTools, QUESTION_TOOL_NAME, ToolAudience, local_tool};
use maki_agent::types::AgentEvent;
use maki_agent::{
    AgentInput, AgentMode, CancelToken, CostTally, Envelope, HostPrompt, ImageMediaType,
    ImageSource, InputSource, McpPromptRef, McpSnapshotReader, SessionEndReason, SessionEvents,
};
use maki_config::project::{self, TrustAnswer, TrustMode, policy_grant};
use maki_config::{MAX_SERVER_NAME_LEN, ModelPolicy, ProjectConfig, TrustConfig};
use maki_providers::model::Model;
use maki_providers::provider::{available_model_specs, fetch_all_models};
use maki_providers::settle_session;
use maki_storage::StateDir;
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::{SessionClaim, SessionError};
use maki_storage::stdin;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, stdout};
use tokio::sync::mpsc::{UnboundedSender, WeakUnboundedSender, unbounded_channel};
use tokio::task::{JoinHandle, spawn_blocking};
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, info, warn};

use crate::commands::{self, Invocation};
use crate::router::{EventSink, Relay, RelayEvent};
use crate::{AcpParams, ProjectStack, elicitation, methods, permissions, translate};

const FIRST_OUTGOING_REQUEST_ID: i64 = 1000;
/// Turns already recorded were priced when they ran. `always_fast` is a live
/// config value, so reading it here would reprice history the user paid for at
/// standard rates. New turns still honour it.
const RESTORED_FAST: bool = false;
/// From JSON-RPC's range for application errors. A session another maki holds
/// is not an internal error (`-32603`), nothing broke here, and the message
/// already says which session and why.
const SESSION_BUSY_CODE: i32 = -32000;
const CWD_UNUSABLE: &str = "session cwd is not a usable folder";

/// Ids come from here and are never reused, so a late answer for a closed
/// session cannot match a request of the session that replaced it.
static NEXT_OUTGOING_REQUEST_ID: AtomicI64 = AtomicI64::new(FIRST_OUTGOING_REQUEST_ID);

/// What the client still owes us. Every agent waits on its own answer channel,
/// so a child's permission can be outstanding next to the main agent's.
#[derive(Default)]
struct Pending {
    prompt: Option<RequestId>,
    asks: HashMap<i64, Ask>,
}

/// How to read the answer and who gets it. A subagent's permission carries the
/// channel its own agent waits on, everything else answers the main agent.
enum Ask {
    /// `request_id` is the agent-side id of the ask the tool is parked on, not
    /// the JSON-RPC id and not the client-facing tool call: the answer is
    /// tagged with it so a waiter on any other ask refuses it.
    Permission {
        request_id: String,
        answer_tx: Option<UnboundedSender<String>>,
    },
    Elicitation,
}

type PendingState = Arc<Mutex<Pending>>;

/// Keyed by the id the client names the session with.
type Sessions = HashMap<SessionId, SessionState>;

struct SessionState {
    handle: InteractiveHandle,
    /// Stopped before a close is answered, so no update for a closed session
    /// and no update of a replaced run reaches the client.
    pump: AbortOnDropHandle<()>,
    mcp: Option<McpHandle>,
    /// Read once when the session opens, like the TUI does at launch.
    commands: Arc<[CustomCommand]>,
    /// The prompts of `mcp`, empty without it.
    mcp_prompts: McpSnapshotReader,
    /// Sends the command list again once MCP prompts land. Stopped with the
    /// pump, so no list goes out after a close is answered.
    commands_watch: Option<AbortOnDropHandle<()>>,
    current_mode: AgentMode,
    current_model: String,
    pending: PendingState,
    /// Shared with the run, so reloading the same session reads under this
    /// claim instead of clashing with it.
    claim: SessionClaim,
}

struct Server {
    out_tx: UnboundedSender<Value>,
    model_specs: Vec<String>,
    client_elicits_form: bool,
    /// Clients keep one agent process and open every thread as its own
    /// session on it, so sessions live side by side.
    sessions: Sessions,
    /// Teardowns still running for closed sessions. Kept so a reload of one
    /// waits for its old run, and so shutdown waits for all of them.
    closing: HashMap<MakiId, JoinHandle<()>>,
    stack: ProjectStack,
    /// Runs the sessions of other folders in child processes.
    relay: Relay,
}

/// What every session of the process shares, whatever folder it runs in.
struct Settings {
    /// Sessions in this folder run in this process.
    launch_dir: PathBuf,
    /// `false` runs sessions of other folders here too.
    relay: bool,
    storage: StateDir,
    trust_mode: TrustMode,
    /// `false` keeps every tool call blocking. Otherwise calls can move to the
    /// background and their results ride the next prompt.
    background: bool,
    host_prompt: HostPrompt,
    commands: bool,
}

impl Server {
    fn respond(&self, id: RequestId, result: Result<AgentResponse, AcpError>) {
        send(&self.out_tx, Response::new(id, result));
    }
}

enum Incoming {
    Line(String),
    Models(Vec<String>),
    Relay(RelayEvent),
}

/// Where a `session/new` or `session/load` runs.
enum Route {
    Here,
    Child(PathBuf),
}

/// A session that opened, and the response that announces it.
struct Opened {
    sid: SessionId,
    response: AgentResponse,
}

pub async fn serve(params: AcpParams) -> color_eyre::Result<()> {
    let AcpParams {
        stack,
        launch_dir,
        child_env,
        relay,
        trust_mode,
        storage,
        background,
        host_prompt,
        commands,
        stop,
    } = params;
    let settings = Settings {
        launch_dir,
        relay,
        storage,
        trust_mode,
        background,
        host_prompt,
        commands,
    };
    let (out_tx, mut out_rx) = unbounded_channel::<Value>();

    let writer_task = tokio::spawn(async move {
        let mut out = stdout();
        while let Some(msg) = out_rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&msg) else {
                continue;
            };
            line.push(b'\n');
            let _ = out.write_all(&line).await;
            let _ = out.flush().await;
        }
    });

    let (in_tx, mut in_rx) = unbounded_channel::<Incoming>();
    // Weak, so a discovery still in flight cannot keep the loop alive once stdin closes.
    discover_models(Arc::clone(&stack.model_policy), in_tx.downgrade());
    let relay = Relay::new(child_env, out_tx.clone(), relay_sink(in_tx.downgrade()));
    let mut server = Server {
        out_tx,
        model_specs: available_model_specs(&stack.model_policy),
        client_elicits_form: false,
        sessions: Sessions::new(),
        closing: HashMap::new(),
        stack,
        relay,
    };
    let reader_task = tokio::spawn(read_stdin(in_tx));

    while let Ok(Some(incoming)) = stop.race(in_rx.recv()).await {
        match incoming {
            Incoming::Line(line) => handle_line(&mut server, &line, &settings).await,
            Incoming::Models(specs) => refresh_models(&mut server, specs),
            Incoming::Relay(event) => server.relay.on_event(event),
        }
    }

    close_all_sessions(&mut server, SessionEndReason::Shutdown).await;
    server.relay.shutdown().await;
    drop(server);
    writer_task.await.context("stdout writer")?;
    // A stop leaves stdin open, so its read would never end.
    if stop.is_cancelled() {
        reader_task.abort();
    } else {
        reader_task
            .await
            .context("stdin reader")?
            .context("read stdin")?;
    }

    Ok(())
}

async fn read_stdin(tx: UnboundedSender<Incoming>) -> std::io::Result<()> {
    while let Some(line) = stdin::read_line().await? {
        if tx.send(Incoming::Line(line)).is_err() {
            break;
        }
    }
    Ok(())
}

/// Weak, so running children cannot keep the loop alive once stdin closes.
fn relay_sink(tx: WeakUnboundedSender<Incoming>) -> EventSink {
    Arc::new(move |event: RelayEvent| {
        if let Some(tx) = tx.upgrade() {
            let _ = tx.send(Incoming::Relay(event));
        }
    })
}

/// Static manifests miss providers that only list their models over the wire
/// (OpenRouter and friends), so the same discovery the TUI runs happens here,
/// in the background. Each batch leaves the moment it lands: the slowest source
/// is a cold catalog download, and a provider the client could already pick from
/// should not wait behind it.
fn discover_models(policy: Arc<ModelPolicy>, tx: WeakUnboundedSender<Incoming>) {
    tokio::spawn(async move {
        fetch_all_models(
            &policy,
            |batch| {
                if let Some(tx) = tx.upgrade() {
                    let _ = tx.send(Incoming::Models(batch.models));
                }
            },
            None,
        )
        .await;
    });
}

/// Discovery lands in batches after the client built its selector from the
/// offline list, so every batch that adds something announces the fuller list.
/// Running sessions need no push, each one reads its model from the registry
/// again before its next turn.
fn refresh_models(srv: &mut Server, batch: Vec<String>) {
    let known = srv.model_specs.len();
    for spec in batch {
        if !srv.model_specs.contains(&spec) {
            srv.model_specs.push(spec);
        }
    }
    if srv.model_specs.len() == known {
        return;
    }
    // Merged even with no session yet, since session/new builds its selector from this list.
    for (sid, session) in &srv.sessions {
        let option = methods::model_config_option(&session.current_model, &srv.model_specs);
        session_update(
            &srv.out_tx,
            sid,
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![option])),
        );
    }
}

async fn handle_line(server: &mut Server, line: &str, settings: &Settings) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }

    let raw: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "invalid JSON on stdin");
            server.respond(RequestId::Null, Err(AcpError::parse_error()));
            return;
        }
    };

    let id = raw.get("id").map(request_id);

    if raw.get("result").is_some() || raw.get("error").is_some() {
        if !server.relay.on_client_response(&raw) {
            handle_incoming_response(server, &raw);
        }
    } else if let Some(method) = raw.get("method").and_then(Value::as_str) {
        if server.relay.forward(&raw) {
            return;
        }
        match id {
            Some(id) => handle_request(server, method, id, &raw, settings).await,
            None => handle_notification(server, method, &raw),
        }
    } else if let Some(id) = id {
        server.respond(id, Err(AcpError::invalid_request()));
    }
}

fn request_id(v: &Value) -> RequestId {
    serde_json::from_value(v.clone()).unwrap_or(RequestId::Null)
}

async fn handle_request(
    srv: &mut Server,
    method: &str,
    id: RequestId,
    raw: &Value,
    settings: &Settings,
) {
    let result = match method {
        "initialize" => {
            srv.client_elicits_form = parse_params::<InitializeRequest>(raw)
                .is_ok_and(|req| elicitation::supports_form(&req.client_capabilities));
            srv.relay.remember_initialize(raw);
            Ok(AgentResponse::InitializeResponse(
                methods::initialize_response(),
            ))
        }
        "session/new" | "session/load" => match route_open(srv, raw, settings) {
            Ok(Route::Here) => {
                let opened = match method {
                    "session/new" => new_session(srv, raw, settings).await,
                    _ => load_session(srv, raw, settings).await,
                };
                return answer_open(srv, id, opened);
            }
            // The child answers the client.
            Ok(Route::Child(folder)) => {
                srv.relay.open(&folder, raw);
                return;
            }
            Err(e) => Err(e),
        },
        "session/list" => list_sessions(raw, &settings.storage).await,
        "session/prompt" => match handle_prompt(srv, raw, &id).transpose() {
            Some(result) => result,
            None => return,
        },
        "session/set_mode" => handle_set_mode(srv, raw),
        "session/set_config_option" => handle_set_config(srv, raw).await,
        "session/close" => handle_close(srv, raw).await,
        _ => Err(AcpError::method_not_found()),
    };
    srv.respond(id, result);
}

/// A session a child already runs stays with it, whatever cwd the client sends.
/// A request with no cwd stays here, where parsing it reports the error.
/// A session already open somewhere stays there, whatever cwd the request
/// names: a second process could not take its claim.
fn route_open(srv: &Server, raw: &Value, settings: &Settings) -> Result<Route, AcpError> {
    if let Some(sid) = raw.pointer("/params/sessionId").and_then(Value::as_str) {
        if let Some(folder) = srv.relay.session_folder(sid) {
            return Ok(Route::Child(folder.to_path_buf()));
        }
        if srv.sessions.contains_key(&SessionId::from(sid.to_owned())) {
            return Ok(Route::Here);
        }
    }
    let Some(cwd) = raw.pointer("/params/cwd").and_then(Value::as_str) else {
        return Ok(Route::Here);
    };
    let folder = Path::new(cwd).canonicalize().map_err(|e| {
        AcpError::invalid_params().data(json_str(&format!("{CWD_UNUSABLE} {cwd}: {e}")))
    })?;
    if folder == settings.launch_dir {
        return Ok(Route::Here);
    }
    if !settings.relay {
        warn!(
            cwd = %folder.display(),
            launch_dir = %settings.launch_dir.display(),
            "relayed acp process runs a session of another folder"
        );
        return Ok(Route::Here);
    }
    Ok(Route::Child(folder))
}

/// The command list goes out after the response: a client drops updates for a
/// session id it does not know yet. A load's replay went out before both.
fn answer_open(srv: &mut Server, id: RequestId, opened: Result<Opened, AcpError>) {
    match opened {
        Ok(Opened { sid, response }) => {
            srv.respond(id, Ok(response));
            advertise_commands(srv, &sid);
        }
        Err(e) => srv.respond(id, Err(e)),
    }
}

/// Every update replaces the client's list, so a session with MCP sends it
/// again once its servers are up, if their prompts moved meanwhile.
fn advertise_commands(srv: &mut Server, sid: &SessionId) {
    let Some(session) = srv.sessions.get_mut(sid) else {
        return;
    };
    let snapshot = session.mcp_prompts.load_full();
    let update = commands::available_commands(&session.commands, &snapshot.prompts);
    session_update(&srv.out_tx, sid, update);
    if let Some(mcp) = &session.mcp {
        let watch = watch_prompt_commands(
            mcp.clone(),
            snapshot.generation,
            Arc::clone(&session.commands),
            sid.clone(),
            srv.out_tx.downgrade(),
        );
        session.commands_watch = Some(AbortOnDropHandle::new(watch));
    }
}

/// Weak, so a watch on a server that never connects cannot keep `serve()`
/// alive.
fn watch_prompt_commands(
    mcp: McpHandle,
    seen: u64,
    custom: Arc<[CustomCommand]>,
    sid: SessionId,
    out_tx: WeakUnboundedSender<Value>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        mcp.ready().await;
        let snapshot = mcp.reader().load_full();
        if snapshot.generation == seen {
            return;
        }
        if let Some(out_tx) = out_tx.upgrade() {
            let update = commands::available_commands(&custom, &snapshot.prompts);
            session_update(&out_tx, &sid, update);
        }
    })
}

/// Reads the disk, so it runs on the blocking pool.
async fn discover_custom_commands(enabled: bool, cwd: PathBuf) -> Arc<[CustomCommand]> {
    if !enabled {
        return Arc::default();
    }
    match spawn_blocking(move || discover_commands(&cwd)).await {
        Ok(found) => Arc::from(found),
        Err(e) => {
            warn!(error = %e, "custom command discovery failed");
            Arc::default()
        }
    }
}

async fn new_session(
    srv: &mut Server,
    raw: &Value,
    settings: &Settings,
) -> Result<Opened, AcpError> {
    let req: NewSessionRequest = parse_params(raw)?;
    let project_config = session_project_config(srv, settings, &req.cwd);
    let mcp = start_mcp(&req.cwd, &req.mcp_servers, project_config.clone()).await;
    let claim = SessionClaim::fresh(&settings.storage);
    let sid = start_session(
        srv,
        settings,
        req.cwd,
        Resumed::empty(SessionRef::from(claim.id())),
        claim,
        mcp,
        project_config,
        None,
        Vec::new(),
    )
    .await;
    maki_otel::emit::session_started(maki_otel::emit::START_FRESH, Some(sid.0.as_ref()));
    let spec = srv.stack.model.spec();
    let resp = methods::new_session_response(&sid.0)
        .config_options(vec![methods::model_config_option(&spec, &srv.model_specs)]);
    Ok(Opened {
        sid,
        response: AgentResponse::NewSessionResponse(resp),
    })
}

async fn load_session(
    srv: &mut Server,
    raw: &Value,
    settings: &Settings,
) -> Result<Opened, AcpError> {
    let req: LoadSessionRequest = parse_params(raw)?;
    let session_ref: SessionRef = req
        .session_id
        .0
        .parse()
        .map_err(|_| AcpError::session_not_found(req.session_id.clone()))?;
    // Reloading a session we already run keeps its claim, so no other process
    // slips in. Any other session is read first, so a failed load leaves every
    // open one alone.
    let (restored, claim) = match release_session(srv, session_ref.id()).await {
        Some(claim) => (reload_history(&settings.storage, &claim).await?, claim),
        None => claim_history(&settings.storage, session_ref.id()).await?,
    };
    let project_config = session_project_config(srv, settings, &req.cwd);
    let mcp = start_mcp(&req.cwd, &req.mcp_servers, project_config.clone()).await;
    let home = maki_storage::paths::home();
    let replay_cwd = restored.cwd.as_deref().unwrap_or(&req.cwd);
    let replay =
        translate::replay_history(restored.session.messages(), replay_cwd, home.as_deref());
    // Priced against the model the session recorded, not the one selected now
    // (which may cost 10x more or less). Later turns add their own exact cost.
    let recorded_model = Model::from_spec_or_warm_catalog(&restored.session.model)
        .await
        .unwrap_or_else(|_| srv.stack.model.clone());
    // Settling writes today's estimate into entries that never recorded a cost.
    // The run saves this session back, so we settle a copy and keep that
    // estimate off disk.
    let mut by_model = restored.session.usage_by_model().clone();
    let restored_cost = settle_session(
        &restored.session.token_usage,
        &mut by_model,
        &recorded_model,
        RESTORED_FAST,
    );
    let sid = start_session(
        srv,
        settings,
        req.cwd,
        Resumed::stored(session_ref, restored.session),
        claim,
        mcp,
        project_config,
        restored_cost,
        replay,
    )
    .await;
    maki_otel::emit::session_started(maki_otel::emit::START_RESUME, Some(sid.0.as_ref()));
    let spec = srv.stack.model.spec();
    let resp = methods::load_session_response()
        .config_options(vec![methods::model_config_option(&spec, &srv.model_specs)]);
    Ok(Opened {
        sid,
        response: AgentResponse::LoadSessionResponse(resp),
    })
}

/// Permissions, where a saved answer lands, and MCP config follow the
/// session's folder and its own trust decision.
fn session_project_config(srv: &Server, settings: &Settings, cwd: &Path) -> ProjectConfig {
    trusted_project_config(
        cwd,
        &settings.storage,
        settings.trust_mode,
        &srv.stack.trust_policy,
    )
}

/// Ends the run of `id` if one is open, and waits until it and any earlier
/// run of `id` are down: their claim released, transcript saved, and MCP
/// servers stopped, so the next run never fights them over a port or a lock
/// file. Returns the claim of the open run, which a reload keeps. Matched by
/// id, since a legacy hex id and its base58 form name the same session.
async fn release_session(srv: &mut Server, id: MakiId) -> Option<SessionClaim> {
    let held = srv
        .sessions
        .iter()
        .find(|(_, state)| state.claim.id() == id)
        .map(|(sid, state)| (sid.clone(), state.claim.clone()));
    if let Some((sid, _)) = &held {
        close_session(srv, sid, SessionEndReason::Replaced).await;
    }
    if let Some(teardown) = srv.closing.remove(&id) {
        let _ = teardown.await;
    }
    held.map(|(_, claim)| claim)
}

/// `session/list`: stored sessions, newest first. Reads the store, not
/// the live session, so it works before any session is open. The
/// optional cwd filters by exact match against the stored cwd, like the
/// CLI and TUI pickers: a path spelled differently (symlink, trailing
/// slash) matches nothing. Without a cwd all stored sessions are
/// listed, like `maki session list --global`. `cursor` is ignored; the
/// response is a single page.
async fn list_sessions(raw: &Value, storage: &StateDir) -> Result<AgentResponse, AcpError> {
    let req: ListSessionsRequest = parse_params(raw)?;
    // The scan stats every session file and tail-reads those with a cold
    // cache, so it runs on a blocking thread, like the TUI picker.
    let storage = storage.clone();
    let summaries = spawn_blocking(move || match req.cwd {
        Some(cwd) => StoredSession::list(&cwd.to_string_lossy(), &storage),
        None => StoredSession::list_all(&storage),
    })
    .await
    .map_err(|e| internal_error(&e))?
    .map_err(|e| internal_error(&e))?;
    let sessions = summaries
        .into_iter()
        .map(|s| {
            let info = SessionInfo::new(s.id.to_string(), s.cwd).title(s.title);
            match iso_from_epoch(s.updated_at) {
                Some(ts) => info.updated_at(ts),
                None => info,
            }
        })
        .collect();
    Ok(AgentResponse::ListSessionsResponse(
        ListSessionsResponse::new(sessions),
    ))
}

/// Maki stores epoch seconds; ACP requires an ISO 8601 string. The
/// store writes 0 when a transcript's tail meta could not be read,
/// meaning "unknown", which must not be rendered as 1970-01-01.
fn iso_from_epoch(secs: u64) -> Option<String> {
    if secs == 0 {
        return None;
    }
    let secs = i64::try_from(secs).ok()?;
    jiff::Timestamp::from_second(secs)
        .ok()
        .map(|t| t.to_string())
}

/// Spawns a session, sends `replay` for it, and adds it to the server's open
/// ones under the id it returns. Spawning alone is not a useful state: the
/// event stream has exactly one reader, so it must be handed to the pump here
/// rather than travel any further.
#[allow(clippy::too_many_arguments)]
async fn start_session(
    srv: &mut Server,
    settings: &Settings,
    cwd: PathBuf,
    resumed: Resumed,
    claim: SessionClaim,
    mcp: Option<McpHandle>,
    project_config: ProjectConfig,
    initial_cost: Option<f64>,
    replay: Vec<SessionUpdate>,
) -> SessionId {
    let pending = PendingState::default();
    let commands = discover_custom_commands(settings.commands, cwd.clone()).await;
    // Without form elicitation the question tool would spin forever waiting
    // for a TUI that does not exist, so it is dropped and the model asks in
    // plain text instead.
    let (excluded_tools, local_tools) = if srv.client_elicits_form {
        let tool = question_tool(srv.out_tx.downgrade(), Arc::clone(&pending));
        let map: LocalTools = Arc::new(HashMap::from([(QUESTION_TOOL_NAME.to_owned(), tool)]));
        (Vec::new(), map)
    } else {
        (vec![QUESTION_TOOL_NAME], LocalTools::default())
    };
    let project_trusted = project_config.is_trusted();
    let permissions_config = maki_config::load_permissions(&project_config);
    let stack = &srv.stack;
    let (handle, events) = headless::spawn_interactive(InteractiveParams {
        model: stack.model.clone(),
        config: stack.config.clone(),
        permissions_config,
        timeouts: stack.timeouts,
        prompt_slots: Arc::clone(&stack.prompt_slots),
        excluded_tools,
        mcp_handle: mcp.clone(),
        initial_wd: cwd.clone(),
        resumed,
        claim: claim.clone(),
        yolo: stack.yolo,
        unattended: false,
        host_prompt: settings.host_prompt.clone(),
        defaults: stack.defaults,
        model_policy: Arc::clone(&stack.model_policy),
        plugin_rules: Arc::clone(&stack.plugin_rules),
        project_config,
        local_tools,
        // Results ride the next prompt. The client never asked for a turn,
        // so nothing wakes one.
        background: if settings.background {
            BackgroundMode::Detached
        } else {
            BackgroundMode::Blocking
        },
        // `close_session` ends it, on a stop as on end of input.
        stop: CancelToken::none(),
    })
    .await;
    let sid = client_id(&handle.session_id);
    // Before the pump starts, so nothing the new run says lands inside the
    // history the client is still rebuilding.
    for update in replay {
        session_update(&srv.out_tx, &sid, update);
    }
    let pump = start_event_pump(
        events,
        sid.clone(),
        srv.out_tx.clone(),
        Arc::clone(&pending),
        cwd,
        maki_storage::paths::home(),
        project_trusted,
        initial_cost,
    );
    let state = SessionState {
        handle,
        pump: AbortOnDropHandle::new(pump),
        mcp_prompts: mcp
            .as_ref()
            .map_or_else(McpSnapshotReader::empty, McpHandle::reader),
        mcp,
        commands,
        commands_watch: None,
        current_mode: AgentMode::Build,
        current_model: srv.stack.model.spec(),
        pending,
        claim,
    };
    srv.sessions.insert(sid.clone(), state);
    sid
}

/// The id the client names a run's session with.
fn client_id(session_ref: &SessionRef) -> SessionId {
    SessionId::from(session_ref.to_string())
}

/// Registers each ask before sending so a response cannot race past it.
fn ask_client(
    out_tx: &UnboundedSender<Value>,
    pending: &PendingState,
    ask: Ask,
    request: AgentRequest,
) -> i64 {
    let id = NEXT_OUTGOING_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    pending.lock().unwrap().asks.insert(id, ask);
    send(
        out_tx,
        Request {
            id: RequestId::Number(id),
            method: Arc::from(request.method()),
            params: Some(request),
        },
    );
    id
}

/// Shadows the Lua `question` tool: sends `elicitation/create` to the client
/// and blocks the tool call until the form comes back. Serializes on the same
/// answer channel as main-agent permissions.
/// Weak, because `LocalTools` is captured verbatim by every `LuaCtx` a tool
/// call parks and an idle VM never collects it. A strong clone here outlives
/// `serve()` and leaves `writer_task` waiting on a sender nobody will drop.
fn question_tool(out_tx: WeakUnboundedSender<Value>, pending: PendingState) -> LocalTool {
    // The audience the Lua `question` tool carries: shadowing a tool must not
    // widen who may call it.
    local_tool(ToolAudience::MAIN, move |input, ctx| {
        let out_tx = out_tx.clone();
        let pending = Arc::clone(&pending);
        Box::pin(async move {
            let session_id = ctx
                .session_id
                .as_ref()
                .map(ToString::to_string)
                .ok_or("no session")?;
            // Batch/code_execution children dispatch with an empty id; a
            // scope pointing at a tool call the client never saw would get
            // the elicitation rejected or dropped.
            let tool_call_id = ctx.tool_use_id.filter(|id| !id.is_empty());
            let out_tx = out_tx.upgrade().ok_or("connection closed")?;
            let request = elicitation::form_request(&session_id, tool_call_id, &input)?;
            let rx = ctx.user_response_rx.as_ref().ok_or("no answer channel")?;

            let mut guard = rx.lock().await;
            let request = AgentRequest::CreateElicitationRequest(request);
            let id = ask_client(&out_tx, &pending, Ask::Elicitation, request);
            let response = ctx.cancel.race(guard.recv()).await;
            // Cleared while still holding the channel, so a stale id cannot
            // clobber whatever ask comes next.
            pending.lock().unwrap().asks.remove(&id);
            drop(guard);

            Ok(match response {
                Ok(Some(raw)) => elicitation::format_response(&input, &raw),
                _ => elicitation::DISMISSED.to_owned(),
            })
        })
    })
}

/// Servers the client injects on `session/new` and `session/load`. A transport we
/// cannot speak is dropped like a broken `mcp.toml` entry: losing one server beats
/// losing the session.
fn injected_servers(servers: &[McpServer]) -> Vec<(String, RawTransport)> {
    servers
        .iter()
        .filter_map(|server| match server {
            McpServer::Http(http) => Some((
                server_name(&http.name),
                RawTransport::Http(RawHttpFields {
                    url: http.url.clone(),
                    headers: pairs(&http.headers, |h| (&h.name, &h.value)),
                    oauth: None,
                    ca_file: None,
                }),
            )),
            McpServer::Stdio(stdio) => Some((
                server_name(&stdio.name),
                RawTransport::Stdio(RawStdioFields {
                    command: iter::once(stdio.command.to_string_lossy().into_owned())
                        .chain(stdio.args.iter().cloned())
                        .collect(),
                    environment: pairs(&stdio.env, |e| (&e.name, &e.value)),
                }),
            )),
            _ => {
                warn!("ignoring injected MCP server, only http and stdio are supported");
                None
            }
        })
        .collect()
}

/// Clients name their servers freely, maki names them like `mcp.toml` does.
fn server_name(name: &str) -> String {
    name.chars()
        .take(MAX_SERVER_NAME_LEN)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn pairs<T>(items: &[T], split: impl Fn(&T) -> (&String, &String)) -> HashMap<String, String> {
    items
        .iter()
        .map(|item| {
            let (name, value) = split(item);
            (name.clone(), value.clone())
        })
        .collect()
}

/// MCP is per session: the client picks the cwd and may inject its own servers.
/// Returns as soon as the config is read, the first prompt waits for the tools.
async fn start_mcp(
    cwd: &Path,
    servers: &[McpServer],
    project_config: ProjectConfig,
) -> Option<McpHandle> {
    let (handle, errors) =
        mcp::start_with_extra(cwd, project_config, injected_servers(servers)).await;
    if !errors.is_empty() {
        warn!(%errors, "MCP config errors");
    }
    handle
}

fn trusted_project_config(
    cwd: &Path,
    storage: &StateDir,
    mode: TrustMode,
    policy: &TrustConfig,
) -> ProjectConfig {
    let mut decision = project::resolve(storage, cwd, mode);
    if let Some(pattern) = policy_grant(&decision.state, policy) {
        // ACP has no card, so policy is the only yes a cwd with no stored
        // decision can get. Recorded like any other yes so `maki trust list`
        // shows what this server trusted on the client's behalf.
        info!(%pattern, cwd = %cwd.display(), "ACP folder trusted by trust.paths policy");
        decision = project::apply_answer(storage, decision, TrustAnswer::Trust);
    }
    // ACP never asks, so the restriction notice is part of what it reports.
    for warning in decision.notices() {
        warn!(%warning, "ACP project configuration trust warning");
    }
    decision.project_config
}

async fn close_session(srv: &mut Server, sid: &SessionId, reason: SessionEndReason) {
    if let Some(state) = srv.sessions.remove(sid) {
        end_session(srv, state, reason).await;
    }
}

/// Every teardown runs at once, so shutdown takes about as long as the
/// slowest session rather than the sum of them.
async fn close_all_sessions(srv: &mut Server, reason: SessionEndReason) {
    for state in mem::take(&mut srv.sessions).into_values() {
        end_session(srv, state, reason).await;
    }
    finish_teardowns(srv).await;
}

async fn finish_teardowns(srv: &mut Server) {
    join_all(srv.closing.drain().map(|(_, teardown)| teardown)).await;
}

/// Stops all the client can see before the close is answered: the turn is
/// cancelled, the pump stopped, and the prompt answered. The slow rest (the
/// `SessionEnd` hook, the run, MCP) goes to a task in [`Server::closing`], so
/// every other session keeps being served meanwhile.
async fn end_session(srv: &mut Server, state: SessionState, reason: SessionEndReason) {
    let SessionState {
        handle,
        pump,
        commands_watch,
        mcp,
        pending,
        ..
    } = state;
    info!(session_id = %handle.session_id, %reason, "closing ACP session");
    let _ = handle.cancel_tx.send(());
    // Awaited, since an aborted task may still be inside a poll that sends.
    pump.abort();
    if let Some(watch) = &commands_watch {
        watch.abort();
    }
    let _ = pump.await;
    if let Some(watch) = commands_watch {
        let _ = watch.await;
    }
    // The pump owed this prompt its answer, and nothing else will give it.
    if let Some(id) = pending.lock().unwrap().prompt.take() {
        let resp = PromptResponse::new(StopReason::Cancelled);
        srv.respond(id, Ok(AgentResponse::PromptResponse(resp)));
    }
    let id = handle.session_id.id();
    let on_session_end = srv.stack.on_session_end.clone();
    let teardown = tokio::spawn(async move {
        if let Some(hook) = on_session_end {
            hook(id, reason).await;
        }
        // Only after the hook, which stops the session's background work, so
        // the track this drops saves the full bill.
        handle.task.abort();
        let _ = handle.task.await;
        if let Some(mcp) = mcp {
            mcp.shutdown().await;
        }
    });
    srv.closing.retain(|_, teardown| !teardown.is_finished());
    srv.closing.insert(id, teardown);
}

#[derive(Debug)]
struct Restored {
    /// Handed to the run whole, so the turns that follow write back into the
    /// session this read rather than into a blank one under the same id.
    session: StoredSession,
    /// Only set when the session recorded an absolute cwd.
    cwd: Option<PathBuf>,
}

/// History plus the absolute cwd the session recorded in its header. Tool
/// inputs from a past run resolve against that cwd, not the client's current
/// one; a non-absolute recording falls back to the caller's cwd.
///
/// Reads from the server's state dir, the one the run writes back to, rather
/// than resolving a second answer to where sessions live.
///
/// Claims before it reads, since the turns that follow replace this transcript.
async fn claim_history(
    storage: &StateDir,
    session_id: MakiId,
) -> Result<(Restored, SessionClaim), AcpError> {
    let claim = SessionClaim::acquire_async(session_id, storage)
        .await
        .map_err(|e| load_error(e, session_id))?;
    Ok((reload_history(storage, &claim).await?, claim))
}

/// [`claim_history`] for the session this server already holds. Parsing a
/// long transcript is CPU work, so it runs on the blocking pool.
async fn reload_history(storage: &StateDir, claim: &SessionClaim) -> Result<Restored, AcpError> {
    let (storage, held) = (storage.clone(), claim.clone());
    spawn_blocking(move || StoredSession::load_claimed(&held, &storage))
        .await
        .map_err(|e| internal_error(&e))?
        .map(Restored::from)
        .map_err(|e| load_error(e, claim.id()))
}

fn load_error(e: SessionError, session_id: MakiId) -> AcpError {
    match e.is_busy() {
        // `not found` would send the client looking for a session it can see
        // right there in the list.
        true => AcpError::new(SESSION_BUSY_CODE, e.to_string()),
        false => {
            warn!(error = %e, %session_id, "session/load found no session to resume");
            AcpError::session_not_found(SessionId::from(session_id.to_string()))
        }
    }
}

impl From<StoredSession> for Restored {
    fn from(session: StoredSession) -> Self {
        let cwd = Path::new(&session.cwd)
            .is_absolute()
            .then(|| PathBuf::from(&session.cwd));
        Self { cwd, session }
    }
}

/// `None` when the run answers the prompt as it ends. A slash command can only
/// lead the prompt, so only a first text block is parsed as one.
fn handle_prompt(
    srv: &mut Server,
    raw: &Value,
    id: &RequestId,
) -> Result<Option<AgentResponse>, AcpError> {
    let req: PromptRequest = parse_params(raw)?;
    let session = open_session(&mut srv.sessions, &req.session_id)?;

    // One outstanding id per session, checked and set under the same guard:
    // `input_tx` is unbounded, so a second prompt would queue happily and
    // overwrite the first id, leaving that request unanswered forever.
    let mut pending = session.pending.lock().unwrap();
    if pending.prompt.is_some() {
        return Err(AcpError::new(-32603, "a prompt is already running"));
    }

    let (command_line, rest) = match req.prompt.split_first() {
        Some((ContentBlock::Text(TextContent { text, .. }), rest)) => (text.as_str(), rest),
        _ => ("", req.prompt.as_slice()),
    };
    let snapshot = session.mcp_prompts.load_full();
    let invocation = match commands::parse(command_line, &session.commands, &snapshot.prompts) {
        Ok(invocation) => invocation,
        Err(usage) => {
            session_update(&srv.out_tx, &req.session_id, translate::text_delta(&usage));
            let resp = PromptResponse::new(StopReason::EndTurn);
            return Ok(Some(AgentResponse::PromptResponse(resp)));
        }
    };
    let prompt_input = |lead: &str, blocks: &[ContentBlock], prompt: Option<McpPromptRef>| {
        let (attached, images) = extract_prompt_content(blocks);
        let mut message = lead.to_owned();
        if !attached.is_empty() {
            append(&mut message, &attached);
        }
        let mut input = AgentInput::from_defaults(
            message,
            session.current_mode.clone(),
            images,
            srv.stack.defaults,
            InputSource::Acp,
        );
        input.prompt = prompt.map(Box::new);
        SessionInput::Prompt(input)
    };
    let input = match invocation {
        None => prompt_input("", &req.prompt, None),
        Some(Invocation::Compact { instructions }) => SessionInput::Compact(instructions),
        Some(Invocation::Prompt { message }) => prompt_input(&message, rest, None),
        Some(Invocation::McpPrompt(prompt)) => {
            prompt_input(command_line.trim(), rest, Some(prompt))
        }
    };

    session
        .handle
        .input_tx
        .send(input)
        .map_err(|_| AcpError::new(-32603, "session ended"))?;
    pending.prompt = Some(id.clone());
    Ok(None)
}

fn handle_set_mode(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionModeRequest = parse_params(raw)?;
    let session = open_session(&mut srv.sessions, &req.session_id)?;
    let mode_str = req.mode_id.0.to_string();
    let new_mode = methods::mode_id_to_agent_mode(&mode_str)
        .ok_or_else(|| AcpError::new(-32602, format!("unknown mode: {mode_str}")))?;

    session.handle.switch_mode(new_mode.clone());
    session.current_mode = new_mode;

    session_update(
        &srv.out_tx,
        &req.session_id,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(mode_str))),
    );
    Ok(AgentResponse::SetSessionModeResponse(
        SetSessionModeResponse::new(),
    ))
}

async fn handle_set_config(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionConfigOptionRequest = parse_params(raw)?;
    let session = open_session(&mut srv.sessions, &req.session_id)?;
    if req.config_id.0.as_ref() != methods::MODEL_CONFIG_ID {
        let detail = format!("unknown config option: {}", req.config_id);
        return Err(AcpError::invalid_params().data(json_str(&detail)));
    }

    let SessionConfigOptionValue::ValueId { value } = req.value else {
        return Err(AcpError::invalid_params().data(json_str(&"model must be a value id")));
    };
    let spec = value.0.to_string();
    let model = Model::from_spec_with_policy(&spec, &srv.stack.model_policy)
        .await
        .map_err(|e| AcpError::invalid_params().data(json_str(&e)))?;

    session
        .handle
        .model_tx
        .send(model)
        .map_err(|_| AcpError::new(-32603, "session ended"))?;
    session.current_model = spec.clone();

    Ok(AgentResponse::SetSessionConfigOptionResponse(
        SetSessionConfigOptionResponse::new(vec![methods::model_config_option(
            &spec,
            &srv.model_specs,
        )]),
    ))
}

/// `session/close`: the client is done with the session for good. An id we
/// do not run is already closed, so it succeeds too.
async fn handle_close(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: CloseSessionRequest = parse_params(raw)?;
    close_session(srv, &req.session_id, SessionEndReason::Delete).await;
    Ok(AgentResponse::CloseSessionResponse(
        CloseSessionResponse::new(),
    ))
}

fn handle_notification(srv: &Server, method: &str, raw: &Value) {
    match method {
        "session/cancel" => match parse_params::<CancelNotification>(raw) {
            // Asks stay: a call moved to the background outlives the turn
            // and may be parked on one, and a stale answer is tagged, so
            // whoever waits next drops it.
            Ok(req) => match srv.sessions.get(&req.session_id) {
                Some(session) => {
                    let _ = session.handle.cancel_tx.send(());
                }
                None => warn!(session_id = %req.session_id, "cancel for an unknown session"),
            },
            Err(e) => warn!(error = ?e, "unreadable session/cancel"),
        },
        _ => debug!(method, "unknown notification"),
    }
}

/// Ask ids are unique across sessions, so whichever session holds the id owns
/// the answer.
fn handle_incoming_response(srv: &Server, raw: &Value) {
    let id = raw.get("id").map(request_id).unwrap_or(RequestId::Null);
    let owner = ask_id(&id).and_then(|id| {
        srv.sessions.values().find_map(|session| {
            let ask = session.pending.lock().unwrap().asks.remove(&id)?;
            Some((session, ask))
        })
    });
    let Some((session, ask)) = owner else {
        warn!(%id, "response for an unknown request id");
        return;
    };
    let (answer, answer_tx) = match &ask {
        Ask::Permission {
            request_id,
            answer_tx,
        } => (
            TaggedAnswer::new(request_id.clone(), permission_answer(raw)).encode(),
            answer_tx.as_ref(),
        ),
        // The waiting question tool parses this; an error response decodes to
        // nothing and counts as a dismissal.
        Ask::Elicitation => (
            raw.get("result")
                .cloned()
                .unwrap_or(Value::Null)
                .to_string(),
            None,
        ),
    };
    let _ = answer_tx.unwrap_or(&session.handle.answer_tx).send(answer);
}

/// Our asks go out with a numeric id, but JSON-RPC ids are `string | number`
/// and a client may echo ours back as a string. Dropping such a response would
/// park the waiting tool forever.
fn ask_id(id: &RequestId) -> Option<i64> {
    match id {
        RequestId::Number(id) => Some(*id),
        RequestId::Str(id) => id.parse().ok(),
        RequestId::Null => None,
    }
}

/// A response we cannot read still has to answer the agent, or the tool waits
/// on a permission that will never come.
fn permission_answer(raw: &Value) -> PermissionAnswer {
    match raw
        .get("result")
        .map(|result| serde_json::from_value::<RequestPermissionResponse>(result.clone()))
    {
        Some(Ok(resp)) => permissions::outcome_to_answer(&resp.outcome),
        _ => PermissionAnswer::Deny,
    }
}

fn extract_prompt_content(blocks: &[ContentBlock]) -> (String, Vec<ImageSource>) {
    let mut text = String::new();
    let mut images = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text(TextContent { text: t, .. }) => append(&mut text, t),
            ContentBlock::Image(ImageContent {
                data, mime_type, ..
            }) => images.push(ImageSource::new(
                image_media_type(mime_type),
                Arc::from(data.as_str()),
            )),
            ContentBlock::Resource(res) => {
                if let EmbeddedResourceResource::TextResourceContents(trc) = &res.resource {
                    append(&mut text, &format!("--- {} ---\n{}", trc.uri, trc.text));
                }
            }
            ContentBlock::ResourceLink(rl) => append(&mut text, &format!("[Resource: {}]", rl.uri)),
            _ => {}
        }
    }

    (text, images)
}

fn append(text: &mut String, part: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(part);
}

fn image_media_type(mime: &str) -> ImageMediaType {
    match mime {
        "image/png" => ImageMediaType::Png,
        "image/gif" => ImageMediaType::Gif,
        "image/webp" => ImageMediaType::Webp,
        _ => ImageMediaType::Jpeg,
    }
}

#[allow(clippy::too_many_arguments)]
fn start_event_pump(
    mut events: SessionEvents,
    sid: SessionId,
    out_tx: UnboundedSender<Value>,
    pending: PendingState,
    cwd: PathBuf,
    home: Option<PathBuf>,
    project_trusted: bool,
    initial_cost: Option<f64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut cost = CostTally::starting_at(initial_cost);
        // A permission request only carries scopes, which are matching keys and
        // not always paths, so the file context a client needs has to come from
        // the tool call that asked for permission. Kept for the turn, like
        // sdk_mode does: the history holds these inputs anyway.
        let mut tool_inputs: HashMap<String, Value> = HashMap::new();

        while let Some(Envelope {
            event, subagent, ..
        }) = events.next().await
        {
            // A subagent's turn spends session money even though its events
            // stay out of the transcript.
            if let AgentEvent::TurnComplete(tc) = &event {
                cost.turn(tc.cost);
            }

            let update = match event {
                AgentEvent::PermissionRequest {
                    id,
                    tool,
                    scopes,
                    reason,
                } => {
                    let tool = tool.to_string();
                    let scope = format!("{tool}: {}", scopes.join(", "));
                    // A plugin escalated this call, and its reason is what the
                    // user most needs to read.
                    let scope = match reason {
                        Some(reason) => format!("{reason} ({scope})"),
                        None => scope,
                    };
                    // The cache is keyed by the call that asked for permission,
                    // so look it up before the remap below. A subagent is left
                    // out on purpose: nothing makes a child's ids unique
                    // against the parent's, and naming the wrong file in a
                    // security prompt is worse than naming none.
                    let raw_input = subagent.is_none().then(|| tool_inputs.get(&id)).flatten();
                    // A child's own tool call never reached the client, nor
                    // this cache, so its permission rides on the `task` call
                    // the client can see, only the child's channel may take the
                    // answer, and the title is all the dialog gets.
                    let request_id = id.clone();
                    let (id, title, answer_tx) = match subagent {
                        Some(info) => {
                            let Some(answer_tx) = info.answer_tx else {
                                warn!(
                                    subagent = info.name,
                                    parent_tool_use_id = info.parent_tool_use_id,
                                    tool_use_id = id,
                                    scope,
                                    "dropping subagent permission with no answer channel"
                                );
                                continue;
                            };
                            let title = format!("{}: {scope}", info.name);
                            (info.parent_tool_use_id, title, Some(answer_tx))
                        }
                        None => (id, scope, None),
                    };
                    let request =
                        AgentRequest::RequestPermissionRequest(RequestPermissionRequest::new(
                            sid.clone(),
                            translate::permission_update(
                                id,
                                title,
                                &tool,
                                raw_input,
                                &cwd,
                                home.as_deref(),
                            )
                            .await,
                            permissions::permission_options(project_trusted),
                        ));
                    ask_client(
                        &out_tx,
                        &pending,
                        Ask::Permission {
                            request_id,
                            answer_tx,
                        },
                        request,
                    );
                    continue;
                }
                _ if subagent.is_some() => continue,
                AgentEvent::TextDelta { text } => translate::text_delta(&text),
                AgentEvent::ThinkingDelta { text } => translate::thinking_delta(&text),
                AgentEvent::ToolPending { id, name } => translate::tool_pending(&id, &name),
                AgentEvent::ToolStart(event) => {
                    let update = translate::tool_start(&event, &cwd, home.as_deref());
                    if let Some(raw_input) = event.raw_input {
                        tool_inputs.insert(event.id, raw_input);
                    }
                    update
                }
                AgentEvent::ToolOutput { id, content } => translate::tool_output(&id, &content),
                AgentEvent::ToolDone(event) => translate::tool_done(&event, &cwd, home.as_deref()),
                AgentEvent::TurnComplete(event) => translate::usage_update(&event, cost.total()),
                AgentEvent::ObservationLine { text, .. } => {
                    translate::notice(&MakiId::generate().to_string(), &text)
                }
                AgentEvent::Done {
                    reason,
                    cost: ledger,
                    ..
                } => {
                    cost.settle(ledger);
                    tool_inputs.clear();
                    if let Some(id) = finish_turn(&pending) {
                        let resp = PromptResponse::new(translate::map_done_reason(reason));
                        send(
                            &out_tx,
                            Response::<AgentResponse>::new(
                                id,
                                Ok(AgentResponse::PromptResponse(resp)),
                            ),
                        );
                    }
                    continue;
                }
                AgentEvent::Error {
                    message,
                    auth,
                    totals,
                } => {
                    // A turn that dies on a provider 500 never reaches `Done`,
                    // and the pump lives as long as the session, so without this the
                    // whole file a `write` was carrying stays pinned forever.
                    tool_inputs.clear();
                    cost.settle(totals.cost);
                    if let Some(id) = finish_turn(&pending) {
                        let mut error = if auth {
                            AcpError::auth_required()
                        } else {
                            AcpError::internal_error()
                        };
                        // Clients show `message`, not `data`.
                        if !auth {
                            error.message.clone_from(&message);
                        }
                        let error = error.data(Value::String(message));
                        send(&out_tx, Response::<AgentResponse>::new(id, Err(error)));
                    }
                    continue;
                }
                _ => continue,
            };
            session_update(&out_tx, &sid, update);
        }
    })
}

/// Takes the client's outstanding `session/prompt`, if any. Its asks stay
/// until answered: a call moved to the background can still be parked on one.
fn finish_turn(pending: &PendingState) -> Option<RequestId> {
    pending.lock().unwrap().prompt.take()
}

fn send(out_tx: &UnboundedSender<Value>, msg: impl Serialize) {
    if let Ok(json) = serde_json::to_value(JsonRpcMessage::wrap(msg)) {
        let _ = out_tx.send(json);
    }
}

fn session_update(out_tx: &UnboundedSender<Value>, sid: &SessionId, update: SessionUpdate) {
    let notification =
        AgentNotification::SessionNotification(SessionNotification::new(sid.clone(), update));
    send(
        out_tx,
        Notification {
            method: Arc::from("session/update"),
            params: Some(notification),
        },
    );
}

fn open_session<'a>(
    sessions: &'a mut Sessions,
    sid: &SessionId,
) -> Result<&'a mut SessionState, AcpError> {
    sessions
        .get_mut(sid)
        .ok_or_else(|| AcpError::session_not_found(sid.clone()))
}

fn parse_params<T: serde::de::DeserializeOwned>(raw: &Value) -> Result<T, AcpError> {
    serde_json::from_value(raw.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|e| AcpError::invalid_params().data(json_str(&e)))
}

fn json_str(e: &impl Display) -> Value {
    Value::String(e.to_string())
}

fn internal_error(e: &impl Display) -> AcpError {
    AcpError::internal_error().data(json_str(e))
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;
    use std::sync::mpsc::sync_channel;

    use agent_client_protocol_schema::v1::ErrorCode;
    use maki_agent::command::{COMPACT_NAME, CommandScope};
    use maki_agent::headless::SessionEnd;
    use maki_agent::mcp::{USAGE_PREFIX, test_support};
    use maki_agent::permissions::{PermissionCheck, PermissionError, PermissionManager};
    use maki_agent::tools::PermissionScopes;
    use maki_agent::{
        CancelToken, DoneReason, EventSender, HostPrompt, RunTotals, SubagentInfo, ToolStartEvent,
        TurnCompleteEvent,
    };
    use maki_agent::{McpPromptInfo, McpSnapshot};
    use maki_config::project::TrustQuestion;
    use maki_config::{AgentConfig, Effect, ToolKey, TrustFileConfig};
    use maki_providers::{ContentBlock as MsgBlock, Message, Role, Timeouts, TokenUsage};
    use maki_storage::StateDir;
    use maki_storage::sessions::{Session, StoredTokenUsage};
    use maki_storage::trusted_folders::{CanonicalFolder, TrustStatus, TrustedFolders};
    use std::future;
    use std::iter;
    use tokio::sync::watch;

    use maki_config::SessionDefaults;
    use tempfile::TempDir;
    use test_case::test_case;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::time::timeout;
    use tokio_util::task::AbortOnDropHandle;

    use super::*;
    use crate::SessionEndHook;
    use crate::router::test_support::{FakeChild, event_channel, fake_spawner};

    const ANSWERED_ID: i64 = 1001;
    const UNKNOWN_ID: i64 = 1002;
    const DISCOVERED_SPEC: &str = "openrouter/discovered-model";
    const OFFLINE_SPEC: &str = "openai/gpt-5";
    const SELECTED_SPEC: &str = "openai/gpt-5.6-sol";
    const FAILED_LOAD_KEEPS_SESSION: &str = "a load that failed must not close the running session";
    /// Neither resolves in the price tables, so nothing can re-price a restored
    /// session back onto the recorded number by luck.
    const RETIRED_SPEC: &str = "retired-vendor/retired-model-9000";
    const RETIRED_MODEL_ID: &str = "retired-model-9000";
    const RECORDED_COST: f64 = 1.25;
    /// Generous on purpose: the work under test is a few file reads, so any
    /// wait near this long is the deadlock and not a slow machine.
    const STDIN_DEADLOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const DENY_SCOPE: &str = "acp-session-trust-boundary-test-deny";
    const ALLOW_SCOPE: &str = "acp-session-trust-boundary-test-allow";
    const POLICY_MATCH_GLOB: &str = "**";
    const POLICY_MISS_GLOB: &str = "/nowhere/*";
    const GATED_INIT_SOURCE: &str = "return {}";
    /// The tool a later turn asks about. Nothing allows `bash` by default, so
    /// an `Ok` here can only come from an answer that was applied.
    const NEXT_TURN_TOOL: &str = "bash";
    const NEXT_TURN_SCOPE: &str = "rm -rf /";
    const NEXT_TURN_TOOL_USE_ID: &str = "toolu_next_turn";
    const STALE_ALLOW: &str = "a stale answer must never allow the next turn's tool";
    const CLOSE_WAITED: &str = "closing a session must not wait for its background work";
    const SESSION_INSTALLED: &str = "a session is installed";
    const PROMPT_TEXT: &str = "rename foo to bar";
    const UNKNOWN_SESSION_ID: &str = "no-such-session";
    const REQUEST_ID: i64 = 8;
    const OTHER_SESSIONS_UNTOUCHED: &str = "a request for one session leaves the others alone";
    const MODE_BUILD_ID: &str = "build";
    const STOP_CANCELLED: &str = "cancelled";
    const REQUEST_ANSWERED: &str = "the request is answered";
    const PROMPT_ANSWERED_ON_TURN_END: &str = "an accepted prompt is answered when its turn ends";
    const NAMED_SESSION_REACHED: &str = "the named session gets the request";
    const CLOSE_TAKES_SESSION: &str = "close must take the session";
    const CLOSED_SESSION_SILENT: &str = "no update for a session goes out after its close";
    const CLOSED_ASK_DROPPED: &str = "an answer for a closed session reaches nobody";
    const HOOK_MARKER: &str = "session_end_hook";

    /// Where fixture servers claim their placeholder session. One dir is enough,
    /// since fresh ids never collide and nothing is written under them.
    static FIXTURE_CLAIMS: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().unwrap());

    /// The client picks the session cwd, so that folder's stored trust decides
    /// whether its `.maki` may widen permissions. Its deny rules need no trust:
    /// a repository can only narrow what the agent may do.
    #[test]
    fn session_project_config_follows_stored_folder_trust() {
        let state = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let maki_dir = project.path().join(".maki");
        std::fs::create_dir(project.path().join(".git")).unwrap();
        std::fs::create_dir(&maki_dir).unwrap();
        std::fs::write(
            maki_dir.join("permissions.toml"),
            format!("[bash]\ndeny = [\"{DENY_SCOPE}\"]\nallow = [\"{ALLOW_SCOPE}\"]\n"),
        )
        .unwrap();
        let storage = StateDir::from_path(state.path().to_path_buf());

        let untrusted = trusted_project_config(
            project.path(),
            &storage,
            TrustMode::Consult,
            &TrustConfig::default(),
        );
        assert!(!untrusted.is_trusted());
        let rules = maki_config::load_permissions(&untrusted).rules;
        assert!(
            rules
                .iter()
                .any(|rule| bash_rule(rule, DENY_SCOPE, Effect::Deny))
        );
        assert!(
            !rules
                .iter()
                .any(|rule| rule.scope.as_deref() == Some(ALLOW_SCOPE))
        );

        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        project::grant(&storage, &TrustQuestion::for_folder(&folder)).unwrap();

        let trusted = trusted_project_config(
            project.path(),
            &storage,
            TrustMode::Consult,
            &TrustConfig::default(),
        );
        assert!(trusted.is_trusted());
        assert_eq!(
            trusted.config_root(),
            ProjectConfig::discover(project.path()).config_root()
        );
        let rules = maki_config::load_permissions(&trusted).rules;
        assert!(
            rules
                .iter()
                .any(|rule| bash_rule(rule, DENY_SCOPE, Effect::Deny))
        );
        assert!(
            rules
                .iter()
                .any(|rule| bash_rule(rule, ALLOW_SCOPE, Effect::Allow))
        );
    }

    /// ACP resolves folder trust from its dispatch loop, on the same executor
    /// that another thread is blocking on a whole stdin read. Reaching for
    /// stdin on a path that can never ask a question parked the loop until the
    /// client sent bytes it was only going to send after our answer, so the
    /// first session hung forever.
    #[test]
    fn resolving_trust_without_a_prompt_does_not_wait_for_stdin() {
        let state = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        std::fs::create_dir(project.path().join(".git")).unwrap();
        std::fs::create_dir(project.path().join(".maki")).unwrap();
        std::fs::write(project.path().join(".maki/init.lua"), "return {}").unwrap();
        let storage = StateDir::from_path(state.path().to_path_buf());
        let cwd = project.path().to_path_buf();

        let held = std::io::stdin().lock();
        let (done_tx, done_rx) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            let config =
                trusted_project_config(&cwd, &storage, TrustMode::Consult, &TrustConfig::default());
            let _ = done_tx.send(config.is_trusted());
        });

        let finished = done_rx.recv_timeout(STDIN_DEADLOCK_TIMEOUT);
        drop(held);
        worker.join().unwrap();

        assert_eq!(
            finished,
            Ok(false),
            "a non-interactive trust resolution must not touch stdin"
        );
    }

    /// A session cwd shipping one gated file, so a start there has a real
    /// question for the policy to answer.
    fn gated_project(project: &Path) {
        std::fs::create_dir(project.join(".git")).unwrap();
        std::fs::create_dir(project.join(".maki")).unwrap();
        std::fs::write(project.join(".maki/init.lua"), GATED_INIT_SOURCE).unwrap();
    }

    fn trust_policy(pattern: &str) -> TrustConfig {
        TrustConfig::from_file(TrustFileConfig {
            paths: Some(vec![pattern.to_owned()]),
            prompt: Some(false),
        })
        .expect("valid trust pattern")
    }

    /// The container image's global `init.lua` is the only thing that can
    /// answer for an ACP run, and its answer has to reach the store: the next
    /// start consults that alone, and `maki trust list` has to show it.
    #[test_case(POLICY_MATCH_GLOB, true ; "a_matching_pattern_grants_without_asking")]
    #[test_case(POLICY_MISS_GLOB, false ; "a_non_matching_pattern_leaves_the_folder_untrusted")]
    fn policy_answers_a_session_cwd(pattern: &str, expected_trusted: bool) {
        let state = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        gated_project(project.path());
        let storage = StateDir::from_path(state.path().to_path_buf());

        let config = trusted_project_config(
            project.path(),
            &storage,
            TrustMode::Consult,
            &trust_policy(pattern),
        );

        assert_eq!(config.is_trusted(), expected_trusted);
        let recorded = project::resolve(&storage, project.path(), TrustMode::Consult);
        assert_eq!(recorded.project_config.is_trusted(), expected_trusted);
    }

    /// A recorded `Never` is an answer, and `grant` replaces a rejection in the
    /// store. A policy that overturned one would delete the decision the user
    /// went out of their way to give, on every session the client opens.
    #[test]
    fn policy_does_not_overturn_a_recorded_rejection() {
        let state = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        gated_project(project.path());
        let storage = StateDir::from_path(state.path().to_path_buf());
        let folder = CanonicalFolder::resolve(project.path()).unwrap();
        project::deny(&storage, &TrustQuestion::for_folder(&folder)).unwrap();

        let config = trusted_project_config(
            project.path(),
            &storage,
            TrustMode::Consult,
            &trust_policy(POLICY_MATCH_GLOB),
        );

        assert!(!config.is_trusted());
        assert_eq!(
            TrustedFolders::new(&storage).status(&folder).unwrap(),
            TrustStatus::Rejected,
            "the rejection must survive a matching policy"
        );
    }

    fn bash_rule(rule: &maki_config::PermissionRule, scope: &str, effect: Effect) -> bool {
        rule.tool == ToolKey::native("bash")
            && rule.scope.as_deref() == Some(scope)
            && rule.effect == effect
    }

    fn allow_once(id: i64) -> Value {
        serde_json::json!({
            "id": id,
            "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } },
        })
    }

    #[test_case(allow_once(ANSWERED_ID), PermissionAnswer::AllowOnce ; "selected_option")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "result": { "outcome": { "outcome": "cancelled" } } }), PermissionAnswer::Deny ; "cancelled_outcome")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "result": { "nonsense": true } }), PermissionAnswer::Deny ; "unparsable_result")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "error": { "code": -32603 } }), PermissionAnswer::Deny ; "jsonrpc_error")]
    fn permission_answer_maps_response(raw: Value, expected: PermissionAnswer) {
        assert_eq!(permission_answer(&raw), expected);
    }

    fn drain<T>(rx: &mut UnboundedReceiver<T>) -> Vec<T> {
        iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// A session plus the ends of its channels the agent would hold, so a test
    /// can see what reached which session.
    struct FixtureSession {
        state: SessionState,
        answer_rx: UnboundedReceiver<String>,
        input_rx: UnboundedReceiver<SessionInput>,
        cancel_rx: watch::Receiver<()>,
        model_rx: UnboundedReceiver<Model>,
        mode_rx: UnboundedReceiver<AgentMode>,
    }

    fn fixture_session() -> FixtureSession {
        let (answer_tx, answer_rx) = unbounded_channel();
        let (input_tx, input_rx) = unbounded_channel();
        let (cancel_tx, cancel_rx) = watch::channel(());
        let (model_tx, model_rx) = unbounded_channel();
        let (mode_tx, mode_rx) = unbounded_channel();
        let claim = SessionClaim::fresh(&StateDir::from_path(FIXTURE_CLAIMS.path().to_path_buf()));
        let handle = InteractiveHandle {
            tool_names: Vec::new(),
            input_tx,
            answer_tx,
            cancel_tx,
            model_tx,
            mode_tx,
            session_id: SessionRef::from(claim.id()),
            permissions: Arc::new(PermissionManager::new(
                maki_config::PermissionsConfig::default(),
                PathBuf::from(PUMP_CWD),
                project::resolve_noninteractive(Path::new(PUMP_CWD), TrustMode::Session)
                    .project_config,
                Arc::default(),
            )),
            task: AbortOnDropHandle::new(tokio::spawn(async { SessionEnd::default() })),
            spend: Arc::default(),
        };
        FixtureSession {
            state: SessionState {
                handle,
                pump: AbortOnDropHandle::new(tokio::spawn(future::ready(()))),
                mcp: None,
                commands: Arc::default(),
                mcp_prompts: McpSnapshotReader::empty(),
                commands_watch: None,
                current_mode: AgentMode::Build,
                current_model: String::new(),
                pending: Arc::default(),
                claim,
            },
            answer_rx,
            input_rx,
            cancel_rx,
            model_rx,
            mode_rx,
        }
    }

    fn test_stack() -> ProjectStack {
        ProjectStack {
            model: Model::from_spec(SELECTED_SPEC).expect("a shipped model"),
            config: AgentConfig::default(),
            timeouts: Timeouts::default(),
            prompt_slots: Arc::default(),
            defaults: SessionDefaults::default(),
            model_policy: Arc::default(),
            plugin_rules: Arc::default(),
            trust_policy: Arc::default(),
            yolo: false,
            on_session_end: None,
        }
    }

    /// A server whose relay starts its children in memory.
    struct RelayFixture {
        srv: Server,
        out_rx: UnboundedReceiver<Value>,
        events_rx: UnboundedReceiver<RelayEvent>,
        spawned_rx: UnboundedReceiver<FakeChild>,
    }

    impl RelayFixture {
        fn new() -> Self {
            let (out_tx, out_rx) = unbounded_channel();
            let (sink, events_rx) = event_channel();
            let (spawner, spawned_rx) = fake_spawner();
            let srv = Server {
                relay: Relay::with_spawner(spawner, out_tx.clone(), sink),
                out_tx,
                model_specs: Vec::new(),
                client_elicits_form: false,
                sessions: Sessions::new(),
                closing: HashMap::new(),
                stack: test_stack(),
            };
            Self {
                srv,
                out_rx,
                events_rx,
                spawned_rx,
            }
        }

        async fn pump(&mut self) {
            let event = self.events_rx.recv().await.unwrap();
            self.srv.relay.on_event(event);
        }

        fn next_to_client(&mut self) -> Value {
            self.out_rx.try_recv().expect(REQUEST_ANSWERED)
        }
    }

    fn empty_server() -> (Server, UnboundedReceiver<Value>) {
        let RelayFixture { srv, out_rx, .. } = RelayFixture::new();
        (srv, out_rx)
    }

    fn install(srv: &mut Server, state: SessionState) -> SessionId {
        let sid = client_id(&state.handle.session_id);
        srv.sessions.insert(sid.clone(), state);
        sid
    }

    /// Records every `SessionEnd` the server fires.
    fn record_session_ends(srv: &mut Server) -> UnboundedReceiver<(MakiId, SessionEndReason)> {
        let (ended_tx, ended_rx) = unbounded_channel();
        let hook: SessionEndHook = Arc::new(move |id, reason| {
            let _ = ended_tx.send((id, reason));
            Box::pin(future::ready(()))
        });
        srv.stack.on_session_end = Some(hook);
        ended_rx
    }

    fn test_server() -> (Server, UnboundedReceiver<String>, UnboundedReceiver<Value>) {
        let (mut server, out_rx) = empty_server();
        let fixture = fixture_session();
        install(&mut server, fixture.state);
        (server, fixture.answer_rx, out_rx)
    }

    fn only_sid(srv: &Server) -> SessionId {
        srv.sessions.keys().next().expect(SESSION_INSTALLED).clone()
    }

    fn only_session(srv: &Server) -> &SessionState {
        srv.sessions.values().next().expect(SESSION_INSTALLED)
    }

    fn with_params(params: impl Serialize) -> Value {
        serde_json::json!({ "params": params })
    }

    fn cancel_notification(sid: &SessionId) -> Value {
        with_params(CancelNotification::new(sid.clone()))
    }

    fn prompt_request(sid: &SessionId) -> Value {
        with_params(PromptRequest::new(
            sid.clone(),
            vec![ContentBlock::from(PROMPT_TEXT)],
        ))
    }

    fn set_mode_request(sid: &SessionId) -> Value {
        with_params(SetSessionModeRequest::new(sid.clone(), MODE_BUILD_ID))
    }

    fn set_model_request(sid: &SessionId) -> Value {
        with_params(SetSessionConfigOptionRequest::new(
            sid.clone(),
            methods::MODEL_CONFIG_ID,
            SELECTED_SPEC,
        ))
    }

    fn close_request(sid: &SessionId) -> Value {
        with_params(CloseSessionRequest::new(sid.clone()))
    }

    fn settings(storage: StateDir) -> Settings {
        Settings {
            launch_dir: PathBuf::from(PUMP_CWD),
            relay: true,
            storage,
            trust_mode: TrustMode::Consult,
            background: false,
            host_prompt: HostPrompt::default(),
            commands: false,
        }
    }

    fn load_request(id: MakiId) -> Value {
        serde_json::json!({
            "params": { "sessionId": id.to_string(), "cwd": "/project", "mcpServers": [] }
        })
    }

    /// A load that cannot happen must not take the running session down with
    /// it: the client still holds that session, and every prompt it sends next
    /// would otherwise answer "no active session".
    #[test_case(false, AcpError::resource_not_found(None).code ; "a session that does not exist")]
    #[test_case(true, SESSION_BUSY_CODE.into() ; "a session another maki holds")]
    #[tokio::test]
    async fn a_failed_load_leaves_the_running_session_alone(held_elsewhere: bool, code: ErrorCode) {
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let mut stored: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new(SELECTED_SPEC, "/project");
        let claim = SessionClaim::acquire(stored.id, &storage).unwrap();
        stored.save(&claim, &storage).unwrap();
        let _elsewhere = held_elsewhere.then_some(claim);
        let target = match held_elsewhere {
            true => stored.id,
            false => MakiId::generate(),
        };
        let (mut srv, _answer_rx, _out_rx) = test_server();
        let running = only_sid(&srv);

        let result = load_session(&mut srv, &load_request(target), &settings(storage)).await;

        let Err(error) = result else {
            panic!("{FAILED_LOAD_KEEPS_SESSION}");
        };
        assert_eq!(error.code, code);
        assert_eq!(
            srv.sessions.keys().collect::<Vec<_>>(),
            [&running],
            "{FAILED_LOAD_KEEPS_SESSION}"
        );
    }

    fn pending(srv: &Server) -> &PendingState {
        &only_session(srv).pending
    }

    fn permission_ask(answer_tx: Option<UnboundedSender<String>>) -> Ask {
        Ask::Permission {
            request_id: PARENT_TOOL_USE_ID.to_owned(),
            answer_tx,
        }
    }

    fn server_with_ask(ask: Ask) -> (Server, UnboundedReceiver<String>, UnboundedReceiver<Value>) {
        let (server, answer_rx, out_rx) = test_server();
        pending(&server)
            .lock()
            .unwrap()
            .asks
            .insert(ANSWERED_ID, ask);
        (server, answer_rx, out_rx)
    }

    const PUMP_CWD: &str = "/project";
    const QUEUED_TEXT: &str = "queued before close";
    const NOTICE_LINE: &str = "Background agent finished: find auth";
    const CHILD_NOTICE_LINE: &str = "a child's own notice";
    const HOST_NOTICE: &str = "the prompt changed, so the cache starts over";
    const PROMPT_ID: i64 = 7;
    const SUBAGENT_COST: f64 = 0.25;
    const TURN_COST: f64 = 0.5;
    const LEDGER_COST: f64 = 0.75;
    const CONTEXT_WINDOW: u32 = 200_000;
    const PARENT_TOOL_USE_ID: &str = "toolu_1";
    /// A string id no ask can ever carry, since ours are minted as numbers.
    const UNANSWERABLE_ID: &str = "not-an-id";
    const SUBAGENT_NAME: &str = "task";
    const CHILD_TOOL_USE_IDS: [&str; 2] = ["child_write_1", "child_write_2"];
    const PERMISSION_TOOL: &str = "write";
    const MAIN_PERMISSION_TITLE: &str = "write: /project";
    const CHILD_PERMISSION_TITLE: &str = "task: write: /project";
    const ESCALATION_REASON: &str = "plugin flagged this write";
    const ESCALATED_PERMISSION_TITLE: &str = "plugin flagged this write (write: /project)";
    const TURN_ERROR: &str = "provider returned 500";
    /// One tool id arriving from two runs of the same session. `openai_compat`
    /// mints unique ids now, but a provider can still repeat one, and the pump
    /// has to keep the runs apart either way.
    const COLLIDING_TOOL_USE_ID: &str = "maki_unnamed_0";
    /// These cover cost and transcript plumbing, not the trust-scoped wording
    /// of permission options.
    const PUMP_TRUSTED: bool = true;
    async fn run_pump(srv: &Server, initial_cost: Option<f64>, feed: impl FnOnce(&EventSender)) {
        let session = only_session(srv);
        let (guard, events) = maki_agent::event_stream();
        let pump = start_event_pump(
            events,
            client_id(&session.handle.session_id),
            srv.out_tx.clone(),
            Arc::clone(&session.pending),
            PathBuf::from(PUMP_CWD),
            None,
            PUMP_TRUSTED,
            initial_cost,
        );
        let sender = guard.sender(0);
        feed(&sender);
        drop(guard);
        pump.await.unwrap();
    }

    fn turn_complete(cost: f64) -> Box<TurnCompleteEvent> {
        Box::new(TurnCompleteEvent {
            message: Message::user(String::new()),
            usage: TokenUsage::default(),
            model: SELECTED_SPEC.to_owned(),
            cost: Some(cost),
            subsidised_list_cost: None,
            context_size: None,
            context_window: CONTEXT_WINDOW,
        })
    }

    fn subagent(answer_tx: Option<UnboundedSender<String>>) -> SubagentInfo {
        SubagentInfo {
            parent_tool_use_id: PARENT_TOOL_USE_ID.to_owned(),
            name: SUBAGENT_NAME.to_owned(),
            prompt: None,
            model: None,
            opts: None,
            answer_tx,
            control: Arc::default(),
            parent_call: None,
        }
    }

    fn done_event() -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            cost: None,
            list_cost: None,
            context_size: 0,
            context_window: CONTEXT_WINDOW,
            num_turns: 1,
            reason: DoneReason::EndTurn,
        }
    }

    fn write_tool_start(tool_use_id: &str) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: tool_use_id.to_owned(),
            tool: Arc::from(PERMISSION_TOOL),
            summary: String::new(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: Some(serde_json::json!({"path": PUMP_CWD, "content": QUEUED_TEXT})),
            output: None,
        }))
    }

    fn permission_request(tool_use_id: &str) -> AgentEvent {
        AgentEvent::PermissionRequest {
            id: tool_use_id.to_owned(),
            tool: ToolKey::native(PERMISSION_TOOL),
            scopes: vec![PUMP_CWD.to_owned()],
            reason: None,
        }
    }

    #[test_case(false ; "answers_reach_the_agent_that_asked_even_out_of_order")]
    #[test_case(true ; "cancel_keeps_the_asks_a_moved_call_may_wait_on")]
    #[tokio::test]
    async fn concurrent_subagent_permissions(cancel: bool) {
        let (srv, mut main_rx, mut out_rx) = test_server();
        let children = [unbounded_channel(), unbounded_channel()];
        run_pump(&srv, None, |sender| {
            for ((answer_tx, _), child_id) in children.iter().zip(CHILD_TOOL_USE_IDS) {
                let info = subagent(Some(answer_tx.clone()));
                for event in [
                    AgentEvent::TextDelta {
                        text: QUEUED_TEXT.to_owned(),
                    },
                    permission_request(child_id),
                ] {
                    sender
                        .send_envelope(Envelope {
                            event,
                            subagent: Some(info.clone()),
                            run_id: 0,
                        })
                        .unwrap();
                }
            }
            sender.send(permission_request(PARENT_TOOL_USE_ID)).unwrap();
        })
        .await;

        let requests: Vec<_> = drain(&mut out_rx);
        assert_eq!(
            requests.len(),
            3,
            "of the subagent events only permissions reach the client"
        );
        for (request, title) in requests.iter().zip([
            CHILD_PERMISSION_TITLE,
            CHILD_PERMISSION_TITLE,
            MAIN_PERMISSION_TITLE,
        ]) {
            assert_eq!(request["method"], "session/request_permission");
            let call = &request["params"]["toolCall"];
            assert_eq!(
                call["toolCallId"], PARENT_TOOL_USE_ID,
                "a child permission rides on the task call the client knows"
            );
            assert_eq!(call["title"], title);
        }

        if cancel {
            handle_notification(
                &srv,
                "session/cancel",
                &cancel_notification(&only_sid(&srv)),
            );
        }
        for (request, option) in requests
            .iter()
            .zip(["reject_once", "allow_once", "allow_always"])
            .rev()
        {
            handle_incoming_response(
                &srv,
                &serde_json::json!({
                    "id": request["id"],
                    "result": {"outcome": {"outcome": "selected", "optionId": option}},
                }),
            );
        }
        let mut child_rx = children.map(|(_, rx)| rx);
        for ((rx, expected), asked_id) in child_rx
            .iter_mut()
            .chain([&mut main_rx])
            .zip([
                PermissionAnswer::Deny,
                PermissionAnswer::AllowOnce,
                PermissionAnswer::AllowSession,
            ])
            .zip(CHILD_TOOL_USE_IDS.iter().chain([&PARENT_TOOL_USE_ID]))
        {
            let expected = TaggedAnswer::new(*asked_id, expected).encode();
            assert_eq!(rx.try_recv().ok(), Some(expected));
            assert!(rx.is_empty(), "each answer is delivered only once");
        }
    }

    /// Without a channel of its own a child's answer would land in the main
    /// agent's, and the main agent's next wait would eat it as its own.
    #[tokio::test]
    async fn a_subagent_permission_without_an_answer_channel_is_never_asked() {
        let (srv, main_rx, out_rx) = test_server();
        run_pump(&srv, None, |sender| {
            sender
                .send_envelope(Envelope {
                    event: permission_request(CHILD_TOOL_USE_IDS[0]),
                    subagent: Some(subagent(None)),
                    run_id: 0,
                })
                .unwrap();
        })
        .await;

        assert!(out_rx.is_empty(), "the client is never asked");
        assert!(main_rx.is_empty(), "the main agent keeps its own turn");
        assert!(
            pending(&srv).lock().unwrap().asks.is_empty(),
            "nothing is left waiting for an answer"
        );
    }

    /// Runs one permission wait the way a turn does, on the session's own
    /// answer channel. Whatever is queued when this is called is what the
    /// waiter finds, which is the shape of the bug: answers outlive the turn
    /// that asked for them.
    async fn wait_for_permission(
        srv: &Server,
        answer_rx: &AsyncMutex<UnboundedReceiver<String>>,
        request_id: &str,
    ) -> Result<(), PermissionError> {
        let session = only_session(srv);
        let (guard, _events) = maki_agent::event_stream();
        let event_tx = guard.sender(0);
        session
            .handle
            .permissions
            .enforce(
                &ToolKey::native(NEXT_TURN_TOOL),
                &PermissionScopes::single(NEXT_TURN_SCOPE.to_owned()),
                None,
                &event_tx,
                Some(answer_rx),
                request_id,
                &CancelToken::none(),
                None,
                None,
                None,
            )
            .await
            // No layers here, so nothing can have answered in a plugin's name.
            .map(|_| ())
    }

    fn queue_answer(srv: &Server, request_id: &str, answer: PermissionAnswer) {
        only_session(srv)
            .handle
            .answer_tx
            .send(TaggedAnswer::new(request_id, answer).encode())
            .unwrap();
    }

    fn next_turn_is_allowed(srv: &Server) -> bool {
        matches!(
            only_session(srv).handle.permissions.check(
                &ToolKey::native(NEXT_TURN_TOOL),
                NEXT_TURN_SCOPE,
                None
            ),
            PermissionCheck::Allowed
        )
    }

    /// The race the ids exist for: the cancel lands while the permission
    /// envelope is still queued in the pump, so the pump still asks and the
    /// client answers it for real. With nobody parked, that answer waits in
    /// the channel for the next turn.
    #[tokio::test]
    async fn an_answer_for_a_cancelled_ask_is_not_consumed_by_the_next_turn() {
        let (srv, answer_rx, mut out_rx) = test_server();
        let answer_rx = AsyncMutex::new(answer_rx);
        handle_notification(
            &srv,
            "session/cancel",
            &cancel_notification(&only_sid(&srv)),
        );
        run_pump(&srv, None, |sender| {
            sender.send(permission_request(PARENT_TOOL_USE_ID)).unwrap();
        })
        .await;

        let request = out_rx.try_recv().expect("the pump still asked the client");
        handle_incoming_response(
            &srv,
            &serde_json::json!({
                "id": request["id"],
                "result": {"outcome": {"outcome": "selected", "optionId": "allow_always"}},
            }),
        );

        queue_answer(&srv, NEXT_TURN_TOOL_USE_ID, PermissionAnswer::Deny);
        let outcome = wait_for_permission(&srv, &answer_rx, NEXT_TURN_TOOL_USE_ID).await;

        assert!(outcome.is_err(), "{STALE_ALLOW}");
        assert!(!next_turn_is_allowed(&srv), "{STALE_ALLOW}");
        assert!(
            answer_rx.into_inner().is_empty(),
            "the stale answer was dropped, not left"
        );
    }

    /// A mismatched answer is dropped without ending the wait: the tool is
    /// still blocked on the ask the user was shown, and only that ask's answer
    /// may end it.
    #[tokio::test]
    async fn an_answer_naming_another_ask_is_discarded_and_the_wait_continues() {
        let (srv, answer_rx, ..) = test_server();
        let answer_rx = AsyncMutex::new(answer_rx);
        queue_answer(&srv, PARENT_TOOL_USE_ID, PermissionAnswer::Deny);
        queue_answer(&srv, NEXT_TURN_TOOL_USE_ID, PermissionAnswer::AllowOnce);

        let outcome = wait_for_permission(&srv, &answer_rx, NEXT_TURN_TOOL_USE_ID).await;

        assert!(
            outcome.is_ok(),
            "the deny named another ask and must not have ended this wait"
        );
        assert!(
            answer_rx.into_inner().is_empty(),
            "both answers were taken off the queue"
        );
    }

    #[test_case(PermissionAnswer::AllowSession, true ; "allow_applies_the_decision")]
    #[test_case(PermissionAnswer::Deny, false ; "deny_applies_the_decision")]
    #[tokio::test]
    async fn a_matching_answer_is_applied(answer: PermissionAnswer, allowed: bool) {
        let (srv, answer_rx, ..) = test_server();
        let answer_rx = AsyncMutex::new(answer_rx);
        queue_answer(&srv, NEXT_TURN_TOOL_USE_ID, answer);

        let outcome = wait_for_permission(&srv, &answer_rx, NEXT_TURN_TOOL_USE_ID).await;

        assert_eq!(outcome.is_ok(), allowed);
        assert_eq!(next_turn_is_allowed(&srv), allowed);
    }

    /// The close marker rides the same FIFO as the events, so a turn still
    /// streaming when the session got replaced is reported in full and the
    /// client's outstanding `session/prompt` is answered instead of hanging.
    #[tokio::test]
    async fn event_pump_delivers_everything_queued_before_the_close() {
        let (srv, .., mut out_rx) = test_server();
        pending(&srv).lock().unwrap().prompt = Some(RequestId::Number(PROMPT_ID));
        run_pump(&srv, None, |sender| {
            sender
                .send(AgentEvent::TextDelta {
                    text: QUEUED_TEXT.to_owned(),
                })
                .unwrap();
            sender.send(done_event()).unwrap();
        })
        .await;

        let chunk = out_rx.try_recv().expect("the queued text reaches the wire");
        let update = &chunk["params"]["update"];
        assert_eq!(update["sessionUpdate"], "agent_message_chunk");
        assert_eq!(update["content"]["text"], QUEUED_TEXT);

        let answer = out_rx.try_recv().expect("the pending prompt is answered");
        assert_eq!(answer["id"], PROMPT_ID);
        assert_eq!(answer["result"]["stopReason"], "end_turn");
        assert!(pending(&srv).lock().unwrap().prompt.is_none());
    }

    /// An accepted observation's line arrives as a finished card, in the order
    /// the agent sent it, and the turn's permission and answer routing stay as
    /// they were. A host notice is not sent, and a child's line stays in the
    /// child, like the rest of its stream.
    #[tokio::test]
    async fn an_accepted_notice_reaches_the_client_in_stream_order() {
        let (srv, .., mut out_rx) = test_server();
        pending(&srv).lock().unwrap().prompt = Some(RequestId::Number(PROMPT_ID));
        run_pump(&srv, None, |sender| {
            sender
                .send(AgentEvent::ObservationLine {
                    text: NOTICE_LINE.to_owned(),
                    detail: None,
                    markdown: false,
                    end: None,
                })
                .unwrap();
            sender
                .send(AgentEvent::Notice {
                    text: HOST_NOTICE.to_owned(),
                })
                .unwrap();
            sender
                .send_envelope(Envelope {
                    event: AgentEvent::ObservationLine {
                        text: CHILD_NOTICE_LINE.to_owned(),
                        detail: None,
                        markdown: false,
                        end: None,
                    },
                    subagent: Some(subagent(None)),
                    run_id: 0,
                })
                .unwrap();
            sender
                .send(AgentEvent::TextDelta {
                    text: QUEUED_TEXT.to_owned(),
                })
                .unwrap();
            sender.send(write_tool_start(PARENT_TOOL_USE_ID)).unwrap();
            sender.send(permission_request(PARENT_TOOL_USE_ID)).unwrap();
            sender.send(done_event()).unwrap();
        })
        .await;

        let wire: Vec<Value> = drain(&mut out_rx);
        let text = serde_json::to_value(translate::text_delta(QUEUED_TEXT)).unwrap();
        assert_eq!(wire.len(), 5, "{wire:?}");
        let card = &wire[0]["params"]["update"];
        assert_eq!(card["sessionUpdate"], "tool_call");
        assert_eq!(card["title"], NOTICE_LINE);
        assert_eq!(card["status"], "completed");
        assert_eq!(wire[1]["params"]["update"], text);
        assert_eq!(
            wire[2]["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(wire[3]["method"], "session/request_permission");
        assert_eq!(
            wire[3]["params"]["toolCall"]["title"],
            MAIN_PERMISSION_TITLE
        );
        assert_eq!(wire[4]["id"], PROMPT_ID);
        assert_eq!(wire[4]["result"]["stopReason"], "end_turn");
    }

    /// Clients only prompt a login on `auth_required`, so a rejected credential
    /// must not come back as a generic failure.
    #[test_case(true, AcpError::auth_required() ; "auth_error_asks_for_a_login")]
    #[test_case(false, AcpError::internal_error() ; "other_error_is_internal")]
    #[tokio::test]
    async fn a_failed_turn_answers_the_prompt_with_its_error_kind(auth: bool, expected: AcpError) {
        let (srv, .., mut out_rx) = test_server();
        pending(&srv).lock().unwrap().prompt = Some(RequestId::Number(PROMPT_ID));
        run_pump(&srv, None, |sender| {
            sender
                .send(AgentEvent::Error {
                    message: TURN_ERROR.to_owned(),
                    auth,
                    totals: RunTotals::default(),
                })
                .unwrap();
        })
        .await;

        let answer = out_rx.try_recv().expect("the pending prompt is answered");
        assert_eq!(answer["id"], PROMPT_ID);
        assert_eq!(answer["error"]["code"], i32::from(expected.code));
        assert_eq!(answer["error"]["data"], TURN_ERROR);
        let shows_cause = answer["error"]["message"] == TURN_ERROR;
        assert_eq!(shows_cause, !auth);
    }

    /// A cached input holds the whole file a `write` is about to lay down, and
    /// this pump lives as long as the session does, so a turn that died on a
    /// provider error has to let go of it just like a turn that finished.
    #[test_case(None, true ; "a_live_turn_shows_the_file_its_tool_call_named")]
    #[test_case(Some(done_event()), false ; "a_finished_turn_releases_its_tool_inputs")]
    #[test_case(Some(AgentEvent::Error { message: TURN_ERROR.to_owned(), auth: false, totals: RunTotals::default() }), false ; "a_failed_turn_releases_its_tool_inputs")]
    #[tokio::test]
    async fn a_terminal_event_releases_the_turns_tool_inputs(
        terminal: Option<AgentEvent>,
        keeps_input: bool,
    ) {
        let (srv, .., mut out_rx) = test_server();
        run_pump(&srv, None, |sender| {
            sender.send(write_tool_start(PARENT_TOOL_USE_ID)).unwrap();
            if let Some(event) = terminal {
                sender.send(event).unwrap();
            }
            sender.send(permission_request(PARENT_TOOL_USE_ID)).unwrap();
        })
        .await;

        let request = drain(&mut out_rx)
            .pop()
            .expect("the permission request reaches the client");
        let call = &request["params"]["toolCall"];
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(call["title"], MAIN_PERMISSION_TITLE);
        assert_eq!(!call["rawInput"].is_null(), keeps_input, "{request}");
    }

    /// Ids are only unique inside one run, so a child can ask about a call whose
    /// id the parent already cached. The dialog has to stay title only, or it
    /// would name a file the subagent is not touching.
    #[tokio::test]
    async fn a_subagent_permission_ignores_a_colliding_cached_input() {
        let (srv, .., mut out_rx) = test_server();
        let (answer_tx, _answer_rx) = unbounded_channel();
        run_pump(&srv, None, |sender| {
            sender
                .send(write_tool_start(COLLIDING_TOOL_USE_ID))
                .unwrap();
            sender
                .send_envelope(Envelope {
                    event: permission_request(COLLIDING_TOOL_USE_ID),
                    subagent: Some(subagent(Some(answer_tx))),
                    run_id: 0,
                })
                .unwrap();
        })
        .await;

        let request = drain(&mut out_rx)
            .pop()
            .expect("the permission request reaches the client");
        let call = &request["params"]["toolCall"];
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(call["toolCallId"], PARENT_TOOL_USE_ID);
        assert_eq!(call["title"], CHILD_PERMISSION_TITLE);
        for field in ["kind", "locations", "rawInput", "content"] {
            assert!(call[field].is_null(), "{field} must stay unset: {request}");
        }
    }

    #[test_case(Some(ESCALATION_REASON), ESCALATED_PERMISSION_TITLE ; "a_plugin_reason_leads_the_title")]
    #[test_case(None, MAIN_PERMISSION_TITLE ; "no_reason_keeps_the_plain_title")]
    #[tokio::test]
    async fn permission_title_carries_the_escalation_reason(reason: Option<&str>, title: &str) {
        let (srv, .., mut out_rx) = test_server();
        run_pump(&srv, None, |sender| {
            sender
                .send(AgentEvent::PermissionRequest {
                    id: PARENT_TOOL_USE_ID.to_owned(),
                    tool: ToolKey::native(PERMISSION_TOOL),
                    scopes: vec![PUMP_CWD.to_owned()],
                    reason: reason.map(str::to_owned),
                })
                .unwrap();
        })
        .await;

        let request = out_rx
            .try_recv()
            .expect("the permission request reaches the client");
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(request["params"]["toolCall"]["title"], title);
    }

    /// A resumed session opens with a bill, and subagent turns spend against it
    /// even though their events never enter the transcript.
    #[tokio::test]
    async fn event_pump_folds_restored_and_subagent_cost_into_the_usage_update() {
        let (srv, .., mut out_rx) = test_server();
        run_pump(&srv, Some(RECORDED_COST), |sender| {
            sender
                .send_envelope(Envelope {
                    event: AgentEvent::TurnComplete(turn_complete(SUBAGENT_COST)),
                    subagent: Some(subagent(None)),
                    run_id: 0,
                })
                .unwrap();
            sender
                .send(AgentEvent::TurnComplete(turn_complete(TURN_COST)))
                .unwrap();
        })
        .await;

        let usage = out_rx.try_recv().expect("the session's own turn reports");
        let update = &usage["params"]["update"];
        assert_eq!(update["sessionUpdate"], "usage_update");
        assert_eq!(
            update["cost"]["amount"].as_f64(),
            Some(RECORDED_COST + SUBAGENT_COST + TURN_COST)
        );
        assert!(
            out_rx.is_empty(),
            "the subagent turn pays but stays out of the transcript"
        );
    }

    /// A run's `Done` or `Error` carries its ledger, which also bills
    /// compaction and moved work, so it replaces what the run's turns added
    /// up to.
    #[test_case(false ; "a_finished_run")]
    #[test_case(true ; "a_failed_run")]
    #[tokio::test]
    async fn event_pump_settles_a_run_on_its_ledger(failed: bool) {
        let (srv, .., mut out_rx) = test_server();
        let ledger = RunTotals {
            cost: Some(LEDGER_COST),
            ..RunTotals::default()
        };
        let ending = if failed {
            AgentEvent::Error {
                message: TURN_ERROR.to_owned(),
                auth: false,
                totals: ledger,
            }
        } else {
            AgentEvent::Done {
                usage: ledger.usage,
                cost: ledger.cost,
                list_cost: None,
                context_size: 0,
                context_window: CONTEXT_WINDOW,
                num_turns: 1,
                reason: DoneReason::EndTurn,
            }
        };
        run_pump(&srv, Some(RECORDED_COST), |sender| {
            sender
                .send(AgentEvent::TurnComplete(turn_complete(TURN_COST)))
                .unwrap();
            sender.send(ending).unwrap();
            sender
                .send(AgentEvent::TurnComplete(turn_complete(TURN_COST)))
                .unwrap();
        })
        .await;

        let last = drain(&mut out_rx).pop().expect("the next turn reports");
        assert_eq!(
            last["params"]["update"]["cost"]["amount"].as_f64(),
            Some(RECORDED_COST + LEDGER_COST + TURN_COST)
        );
    }

    /// Results ride the next prompt and nothing wakes a turn, so a session
    /// closed with work out stops it rather than waiting on it. The task that
    /// never ends stands in for that work.
    #[tokio::test]
    async fn shutdown_returns_at_once_with_background_work_out() {
        let (mut srv, ..) = test_server();
        let sid = only_sid(&srv);
        srv.sessions
            .get_mut(&sid)
            .expect(SESSION_INSTALLED)
            .handle
            .task = AbortOnDropHandle::new(tokio::spawn(future::pending()));

        let closed = timeout(
            STDIN_DEADLOCK_TIMEOUT,
            close_all_sessions(&mut srv, SessionEndReason::Shutdown),
        )
        .await
        .is_ok();

        assert!(closed, "{CLOSE_WAITED}");
    }

    #[tokio::test]
    async fn shutdown_awaits_every_session_end_hook() {
        let (mut srv, _out_rx) = empty_server();
        let mut ended_rx = record_session_ends(&mut srv);
        let sessions = [fixture_session(), fixture_session()];
        let mut expected: Vec<_> = sessions
            .iter()
            .map(|fixture| {
                (
                    fixture.state.handle.session_id.id(),
                    SessionEndReason::Shutdown,
                )
            })
            .collect();
        for fixture in sessions {
            install(&mut srv, fixture.state);
        }

        close_all_sessions(&mut srv, SessionEndReason::Shutdown).await;

        let mut ended: Vec<_> = iter::from_fn(|| ended_rx.try_recv().ok()).collect();
        ended.sort_by_key(|(id, _)| *id);
        expected.sort_by_key(|(id, _)| *id);
        assert_eq!(ended, expected);
        assert!(srv.sessions.is_empty(), "{CLOSE_TAKES_SESSION}");
    }

    fn fixture_settings() -> Settings {
        settings(StateDir::from_path(FIXTURE_CLAIMS.path().to_path_buf()))
    }

    /// A reload ends the old run of that session and waits for it to be down
    /// before it reads, while every other open session keeps running.
    #[tokio::test]
    async fn reloading_one_of_several_open_sessions_replaces_only_that_one() {
        let (mut srv, _out_rx) = empty_server();
        let mut ended_rx = record_session_ends(&mut srv);
        let kept = fixture_session();
        let reloaded = fixture_session();
        let reloaded_id = reloaded.state.claim.id();
        let kept_sid = install(&mut srv, kept.state);
        install(&mut srv, reloaded.state);

        let claim = release_session(&mut srv, reloaded_id).await;

        assert_eq!(claim.map(|claim| claim.id()), Some(reloaded_id));
        assert_eq!(
            ended_rx.try_recv().ok(),
            Some((reloaded_id, SessionEndReason::Replaced))
        );
        assert!(srv.closing.is_empty());
        assert_eq!(
            srv.sessions.keys().collect::<Vec<_>>(),
            [&kept_sid],
            "{OTHER_SESSIONS_UNTOUCHED}"
        );
        assert!(
            !kept.cancel_rx.has_changed().unwrap(),
            "{OTHER_SESSIONS_UNTOUCHED}"
        );
    }

    #[tokio::test]
    async fn a_prompt_reaches_only_the_session_it_names() {
        let (mut srv, out_rx) = empty_server();
        let other = fixture_session();
        let mut named = fixture_session();
        let other_sid = install(&mut srv, other.state);
        let named_sid = install(&mut srv, named.state);

        handle_request(
            &mut srv,
            "session/prompt",
            RequestId::Number(PROMPT_ID),
            &prompt_request(&named_sid),
            &fixture_settings(),
        )
        .await;

        assert!(out_rx.is_empty(), "{PROMPT_ANSWERED_ON_TURN_END}");
        assert!(named.input_rx.try_recv().is_ok(), "{NAMED_SESSION_REACHED}");
        assert_eq!(
            srv.sessions[&named_sid].pending.lock().unwrap().prompt,
            Some(RequestId::Number(PROMPT_ID))
        );
        assert!(other.input_rx.is_empty(), "{OTHER_SESSIONS_UNTOUCHED}");
        assert!(
            srv.sessions[&other_sid]
                .pending
                .lock()
                .unwrap()
                .prompt
                .is_none(),
            "{OTHER_SESSIONS_UNTOUCHED}"
        );
    }

    #[tokio::test]
    async fn a_cancel_reaches_only_the_session_it_names() {
        let (mut srv, _out_rx) = empty_server();
        let other = fixture_session();
        let named = fixture_session();
        install(&mut srv, other.state);
        let named_sid = install(&mut srv, named.state);

        handle_notification(&srv, "session/cancel", &cancel_notification(&named_sid));

        assert!(
            named.cancel_rx.has_changed().unwrap(),
            "{NAMED_SESSION_REACHED}"
        );
        assert!(
            !other.cancel_rx.has_changed().unwrap(),
            "{OTHER_SESSIONS_UNTOUCHED}"
        );
    }

    fn switched(
        model_rx: &mut UnboundedReceiver<Model>,
        mode_rx: &mut UnboundedReceiver<AgentMode>,
    ) -> bool {
        model_rx.try_recv().is_ok() || mode_rx.try_recv().is_ok()
    }

    #[test_case("session/set_mode", set_mode_request ; "a_mode_switch")]
    #[test_case("session/set_config_option", set_model_request ; "a_model_switch")]
    #[tokio::test]
    async fn a_switch_reaches_only_the_session_it_names(
        method: &str,
        request: fn(&SessionId) -> Value,
    ) {
        let (mut srv, mut out_rx) = empty_server();
        let mut other = fixture_session();
        let mut named = fixture_session();
        install(&mut srv, other.state);
        let named_sid = install(&mut srv, named.state);

        handle_request(
            &mut srv,
            method,
            RequestId::Number(REQUEST_ID),
            &request(&named_sid),
            &fixture_settings(),
        )
        .await;

        let answer = drain(&mut out_rx).pop().expect(REQUEST_ANSWERED);
        assert!(answer["result"].is_object(), "{answer}");
        assert!(
            switched(&mut named.model_rx, &mut named.mode_rx),
            "{NAMED_SESSION_REACHED}"
        );
        assert!(
            !switched(&mut other.model_rx, &mut other.mode_rx),
            "{OTHER_SESSIONS_UNTOUCHED}"
        );
    }

    /// Ask ids are minted for the whole process, so the answer has to find
    /// the session that asked, not whichever one is open.
    #[tokio::test]
    async fn an_answer_reaches_the_session_that_asked() {
        let (mut srv, _out_rx) = empty_server();
        let other = fixture_session();
        let mut asker = fixture_session();
        asker
            .state
            .pending
            .lock()
            .unwrap()
            .asks
            .insert(ANSWERED_ID, permission_ask(None));
        install(&mut srv, other.state);
        install(&mut srv, asker.state);

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));

        assert_eq!(
            asker.answer_rx.try_recv().ok(),
            Some(TaggedAnswer::new(PARENT_TOOL_USE_ID, PermissionAnswer::AllowOnce).encode())
        );
        assert!(other.answer_rx.is_empty(), "{OTHER_SESSIONS_UNTOUCHED}");
    }

    #[tokio::test]
    async fn an_answer_to_an_ask_of_a_closed_session_reaches_nobody() {
        let (mut srv, _out_rx) = empty_server();
        let other = fixture_session();
        let mut closed = fixture_session();
        closed
            .state
            .pending
            .lock()
            .unwrap()
            .asks
            .insert(ANSWERED_ID, permission_ask(None));
        install(&mut srv, other.state);
        let closed_sid = install(&mut srv, closed.state);

        close_session(&mut srv, &closed_sid, SessionEndReason::Delete).await;
        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        finish_teardowns(&mut srv).await;

        assert!(closed.answer_rx.try_recv().is_err(), "{CLOSED_ASK_DROPPED}");
        assert!(other.answer_rx.is_empty(), "{OTHER_SESSIONS_UNTOUCHED}");
    }

    #[test_case("session/prompt", prompt_request ; "a_prompt")]
    #[test_case("session/set_mode", set_mode_request ; "a_mode_switch")]
    #[test_case("session/set_config_option", set_model_request ; "a_model_switch")]
    #[tokio::test]
    async fn a_request_for_an_unknown_session_is_not_found(
        method: &str,
        request: fn(&SessionId) -> Value,
    ) {
        let (mut srv, _answer_rx, mut out_rx) = test_server();
        let unknown = SessionId::from(UNKNOWN_SESSION_ID);

        handle_request(
            &mut srv,
            method,
            RequestId::Number(REQUEST_ID),
            &request(&unknown),
            &fixture_settings(),
        )
        .await;

        let answer = out_rx.try_recv().expect(REQUEST_ANSWERED);
        let expected = AcpError::session_not_found(unknown);
        assert_eq!(answer["id"], REQUEST_ID);
        assert_eq!(answer["error"]["code"], i32::from(expected.code));
        assert_eq!(answer["error"]["data"]["sessionId"], UNKNOWN_SESSION_ID);
    }

    async fn close_by_request(srv: &mut Server, sid: &SessionId) {
        handle_request(
            srv,
            "session/close",
            RequestId::Number(REQUEST_ID),
            &close_request(sid),
            &fixture_settings(),
        )
        .await;
    }

    /// `session/close` counts as a cancel: the prompt still running there is
    /// answered before the close is, and only that session ends.
    #[tokio::test]
    async fn session_close_ends_only_the_session_it_names() {
        let (mut srv, mut out_rx) = empty_server();
        let mut ended_rx = record_session_ends(&mut srv);
        let kept = fixture_session();
        let closed = fixture_session();
        let closed_id = closed.state.handle.session_id.id();
        let kept_sid = install(&mut srv, kept.state);
        let closed_sid = install(&mut srv, closed.state);
        srv.sessions[&closed_sid].pending.lock().unwrap().prompt =
            Some(RequestId::Number(PROMPT_ID));

        close_by_request(&mut srv, &closed_sid).await;
        finish_teardowns(&mut srv).await;

        let wire = drain(&mut out_rx);
        assert_eq!(wire.len(), 2, "{wire:?}");
        assert_eq!(wire[0]["id"], PROMPT_ID);
        assert_eq!(wire[0]["result"]["stopReason"], STOP_CANCELLED);
        assert_eq!(wire[1]["id"], REQUEST_ID);
        assert!(wire[1]["result"].is_object(), "{wire:?}");
        assert_eq!(
            ended_rx.try_recv().ok(),
            Some((closed_id, SessionEndReason::Delete))
        );
        assert_eq!(
            srv.sessions.keys().collect::<Vec<_>>(),
            [&kept_sid],
            "{OTHER_SESSIONS_UNTOUCHED}"
        );
    }

    /// The hook can take seconds, so a prompt left for it to answer would
    /// leave the client's turn spinning that long after the user closed it.
    #[tokio::test]
    async fn close_answers_the_running_prompt_before_the_hook_runs() {
        let (mut srv, mut out_rx) = empty_server();
        let sid = install(&mut srv, fixture_session().state);
        srv.sessions[&sid].pending.lock().unwrap().prompt = Some(RequestId::Number(PROMPT_ID));
        let out_tx = srv.out_tx.clone();
        srv.stack.on_session_end = Some(Arc::new(move |_, _| {
            let _ = out_tx.send(Value::from(HOOK_MARKER));
            Box::pin(future::ready(()))
        }));

        close_by_request(&mut srv, &sid).await;
        finish_teardowns(&mut srv).await;

        let wire = drain(&mut out_rx);
        let answered_at = wire.iter().position(|msg| {
            msg["id"] == PROMPT_ID && msg["result"]["stopReason"] == STOP_CANCELLED
        });
        let hook_at = wire.iter().position(|msg| *msg == HOOK_MARKER);
        assert!(
            matches!((answered_at, hook_at), (Some(answered), Some(hook)) if answered < hook),
            "{wire:?}"
        );
    }

    /// The pump is the only sender of a session's updates, and a close stops
    /// it before answering, so an event the run emits later goes nowhere.
    #[tokio::test]
    async fn no_update_for_a_session_goes_out_after_its_close_response() {
        let (mut srv, mut out_rx) = empty_server();
        let sid = install(&mut srv, fixture_session().state);
        let (guard, events) = maki_agent::event_stream();
        let pump = start_event_pump(
            events,
            sid.clone(),
            srv.out_tx.clone(),
            Arc::clone(&srv.sessions[&sid].pending),
            PathBuf::from(PUMP_CWD),
            None,
            PUMP_TRUSTED,
            None,
        );
        srv.sessions.get_mut(&sid).expect(SESSION_INSTALLED).pump = AbortOnDropHandle::new(pump);
        let sender = guard.sender(0);
        let delta = || AgentEvent::TextDelta {
            text: QUEUED_TEXT.to_owned(),
        };
        sender.send(delta()).unwrap();

        close_by_request(&mut srv, &sid).await;
        let _ = sender.send(delta());
        drop(guard);
        finish_teardowns(&mut srv).await;

        let wire = drain(&mut out_rx);
        let closed_at = wire
            .iter()
            .position(|msg| msg["id"] == REQUEST_ID)
            .expect(REQUEST_ANSWERED);
        assert!(
            wire[closed_at + 1..]
                .iter()
                .all(|msg| msg["method"] != "session/update"),
            "{CLOSED_SESSION_SILENT}: {wire:?}"
        );
    }

    /// A session we do not run is as closed as it gets, so the client is
    /// told it worked.
    #[tokio::test]
    async fn session_close_of_an_unknown_session_succeeds() {
        let (mut srv, _answer_rx, mut out_rx) = test_server();
        let running = only_sid(&srv);

        handle_request(
            &mut srv,
            "session/close",
            RequestId::Number(REQUEST_ID),
            &close_request(&SessionId::from(UNKNOWN_SESSION_ID)),
            &fixture_settings(),
        )
        .await;

        let answer = out_rx.try_recv().expect(REQUEST_ANSWERED);
        assert_eq!(answer["id"], REQUEST_ID);
        assert!(answer["result"].is_object(), "{answer}");
        assert_eq!(
            srv.sessions.keys().collect::<Vec<_>>(),
            [&running],
            "{OTHER_SESSIONS_UNTOUCHED}"
        );
    }

    const PROJECT_REVIEW: &str = "project:review";
    const REVIEW_NAME: &str = "review";
    const REVIEW_CONTENT: &str = "Review $ARGUMENTS";
    const REVIEW_LINE: &str = "/project:review src/main.rs";
    const REVIEW_RENDERED: &str = "Review src/main.rs";
    const ATTACHED_TEXT: &str = "and keep the tests green";
    const IMAGE_DATA: &str = "aGk=";
    const IMAGE_MIME: &str = "image/png";
    const GREET: &str = "srv:greet";
    const GREET_QUALIFIED: &str = "srv.greet";
    const GREET_LINE: &str = "/srv:greet bob";
    const NAME_ARG: &str = "name";
    const NAME_VALUE: &str = "bob";
    const COMPACT_LINE: &str = "/compact keep the test plan";
    const COMPACT_GUIDANCE: &str = "keep the test plan";
    const PATH_PROMPT: &str = "/usr/bin is broken";
    const COMMANDS_UPDATE: &str = "available_commands_update";
    const MESSAGE_CHUNK: &str = "agent_message_chunk";
    const STOP_END_TURN: &str = "end_turn";
    const EXPECTED_PROMPT: &str = "the agent gets a prompt";
    const REJECTED_STAYS_LOCAL: &str = "a rejected command reaches no agent";

    /// A session with one custom command and one MCP prompt that needs an
    /// argument.
    fn command_session() -> FixtureSession {
        let mut fixture = fixture_session();
        fixture.state.commands = Arc::from(vec![CustomCommand {
            name: REVIEW_NAME.into(),
            description: String::new(),
            content: REVIEW_CONTENT.into(),
            scope: CommandScope::Project,
            accepts_args: true,
            argument_hint: None,
        }]);
        fixture.state.mcp_prompts = McpSnapshotReader::from_snapshot(McpSnapshot {
            prompts: vec![McpPromptInfo::new(
                GREET,
                GREET_QUALIFIED,
                &[(NAME_ARG, true)],
            )],
            ..McpSnapshot::default()
        });
        fixture
    }

    /// What a prompt of `blocks` sent to [`command_session`] hands its agent,
    /// with the server so a test can read the wire and the pending prompt.
    async fn prompt_command_session(
        blocks: Vec<ContentBlock>,
    ) -> (Server, UnboundedReceiver<Value>, Option<SessionInput>) {
        let (mut srv, out_rx) = empty_server();
        let mut fixture = command_session();
        let sid = install(&mut srv, fixture.state);
        handle_request(
            &mut srv,
            "session/prompt",
            RequestId::Number(PROMPT_ID),
            &with_params(PromptRequest::new(sid, blocks)),
            &fixture_settings(),
        )
        .await;
        (srv, out_rx, fixture.input_rx.try_recv().ok())
    }

    fn agent_input(input: Option<SessionInput>) -> AgentInput {
        match input {
            Some(SessionInput::Prompt(input)) => input,
            _ => panic!("{EXPECTED_PROMPT}"),
        }
    }

    fn pending_prompt(srv: &Server) -> Option<RequestId> {
        pending(srv).lock().unwrap().prompt.clone()
    }

    #[tokio::test]
    async fn advertised_commands_follow_the_open_response() {
        let (mut srv, mut out_rx) = empty_server();
        let sid = install(&mut srv, command_session().state);
        let resp = AgentResponse::NewSessionResponse(methods::new_session_response(&sid.0));

        let opened = Opened {
            sid,
            response: resp,
        };
        answer_open(&mut srv, RequestId::Number(REQUEST_ID), Ok(opened));

        let wire = drain(&mut out_rx);
        assert_eq!(wire.len(), 2, "{wire:?}");
        assert_eq!(wire[0]["id"], REQUEST_ID);
        let update = &wire[1]["params"]["update"];
        assert_eq!(update["sessionUpdate"], COMMANDS_UPDATE);
        let names: Vec<_> = update["availableCommands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command["name"].clone())
            .collect();
        assert_eq!(names, [COMPACT_NAME, PROJECT_REVIEW, GREET]);
    }

    #[tokio::test]
    async fn compact_with_guidance_holds_the_prompt_until_it_ends() {
        let blocks = vec![
            ContentBlock::from(COMPACT_LINE),
            ContentBlock::from(ATTACHED_TEXT),
        ];
        let (srv, out_rx, input) = prompt_command_session(blocks).await;

        assert!(
            matches!(input, Some(SessionInput::Compact(Some(ref guidance))) if guidance == COMPACT_GUIDANCE)
        );
        assert_eq!(pending_prompt(&srv), Some(RequestId::Number(PROMPT_ID)));
        assert!(out_rx.is_empty(), "{PROMPT_ANSWERED_ON_TURN_END}");
    }

    #[tokio::test]
    async fn custom_command_renders_with_later_blocks_attached() {
        let blocks = vec![
            ContentBlock::from(REVIEW_LINE),
            ContentBlock::from(ATTACHED_TEXT),
            ContentBlock::Image(ImageContent::new(IMAGE_DATA, IMAGE_MIME)),
        ];
        let (srv, _out_rx, input) = prompt_command_session(blocks).await;

        let input = agent_input(input);
        assert_eq!(input.message, format!("{REVIEW_RENDERED}\n{ATTACHED_TEXT}"));
        assert_eq!(input.images.len(), 1);
        assert!(input.prompt.is_none());
        assert_eq!(pending_prompt(&srv), Some(RequestId::Number(PROMPT_ID)));
    }

    #[tokio::test]
    async fn mcp_prompt_rides_the_input() {
        let (_srv, _out_rx, input) =
            prompt_command_session(vec![ContentBlock::from(GREET_LINE)]).await;

        let expected = McpPromptRef {
            qualified_name: GREET_QUALIFIED.into(),
            arguments: HashMap::from([(NAME_ARG.into(), NAME_VALUE.into())]),
        };
        let input = agent_input(input);
        assert_eq!(input.prompt.as_deref(), Some(&expected));
        assert_eq!(input.message, GREET_LINE);
    }

    #[tokio::test]
    async fn unknown_slash_text_passes_through() {
        let (_srv, _out_rx, input) =
            prompt_command_session(vec![ContentBlock::from(PATH_PROMPT)]).await;

        let input = agent_input(input);
        assert_eq!(input.message, PATH_PROMPT);
        assert!(input.prompt.is_none());
    }

    #[tokio::test]
    async fn rejected_command_answers_with_its_usage() {
        let (srv, mut out_rx, input) =
            prompt_command_session(vec![ContentBlock::from(format!("/{GREET}"))]).await;

        let wire = drain(&mut out_rx);
        assert_eq!(wire.len(), 2, "{wire:?}");
        let update = &wire[0]["params"]["update"];
        assert_eq!(update["sessionUpdate"], MESSAGE_CHUNK);
        assert_eq!(
            update["content"]["text"],
            format!("{USAGE_PREFIX} /{GREET} <{NAME_ARG}>")
        );
        assert_eq!(wire[1]["id"], PROMPT_ID);
        assert_eq!(wire[1]["result"]["stopReason"], STOP_END_TURN);
        assert!(input.is_none(), "{REJECTED_STAYS_LOCAL}");
        assert_eq!(pending_prompt(&srv), None);
    }

    #[tokio::test]
    async fn prompts_that_land_later_are_advertised_again() {
        let (srv, mut out_rx) = empty_server();
        let mcp = McpHandle::clone(&test_support::stub_session(&[]));
        let unseen = mcp.reader().load().generation.wrapping_add(1);
        let sid = SessionId::from(UNKNOWN_SESSION_ID);

        watch_prompt_commands(mcp, unseen, Arc::default(), sid, srv.out_tx.downgrade())
            .await
            .unwrap();

        let wire = drain(&mut out_rx);
        assert_eq!(wire.len(), 1, "{wire:?}");
        assert_eq!(
            wire[0]["params"]["update"]["sessionUpdate"],
            COMMANDS_UPDATE
        );
    }

    /// The watch is due the moment it runs, and the teardown yields after the
    /// close is answered, so only stopping it with the pump keeps it quiet.
    #[tokio::test]
    async fn no_command_list_goes_out_after_the_close_response() {
        let (mut srv, mut out_rx) = empty_server();
        let sid = install(&mut srv, fixture_session().state);
        // Lets the fixture's finished pump settle, so the close awaits nothing.
        tokio::task::yield_now().await;
        let mcp = McpHandle::clone(&test_support::stub_session(&[]));
        let unseen = mcp.reader().load().generation.wrapping_add(1);
        let watch = watch_prompt_commands(
            mcp,
            unseen,
            Arc::default(),
            sid.clone(),
            srv.out_tx.downgrade(),
        );
        srv.sessions
            .get_mut(&sid)
            .expect(SESSION_INSTALLED)
            .commands_watch = Some(AbortOnDropHandle::new(watch));

        close_by_request(&mut srv, &sid).await;
        finish_teardowns(&mut srv).await;
        // Queues this test behind the watch, had the close left it running.
        tokio::task::yield_now().await;

        let wire = drain(&mut out_rx);
        let closed_at = wire
            .iter()
            .position(|msg| msg["id"] == REQUEST_ID)
            .expect(REQUEST_ANSWERED);
        assert!(
            wire[closed_at + 1..]
                .iter()
                .all(|msg| msg["params"]["update"]["sessionUpdate"] != COMMANDS_UPDATE),
            "{CLOSED_SESSION_SILENT}: {wire:?}"
        );
    }
    /// JSON-RPC ids are `string | number`, and a client is free to echo ours
    /// back in either shape. An answer dropped for its shape leaves the tool
    /// parked forever.
    #[test_case(Value::from(ANSWERED_ID), true ; "a_numeric_id_answers_the_ask")]
    #[test_case(Value::from(ANSWERED_ID.to_string()), true ; "a_string_id_answers_the_same_ask")]
    #[test_case(Value::from(UNANSWERABLE_ID), false ; "an_id_matching_no_ask_answers_nothing")]
    #[tokio::test]
    async fn a_response_is_delivered_whatever_shape_its_id_has(id: Value, delivered: bool) {
        let (srv, mut answer_rx, ..) = server_with_ask(permission_ask(None));

        handle_incoming_response(
            &srv,
            &serde_json::json!({
                "id": id,
                "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } },
            }),
        );

        let answered = TaggedAnswer::new(PARENT_TOOL_USE_ID, PermissionAnswer::AllowOnce).encode();
        assert_eq!(answer_rx.try_recv().ok(), delivered.then_some(answered));
        assert_eq!(
            pending(&srv)
                .lock()
                .unwrap()
                .asks
                .contains_key(&ANSWERED_ID),
            !delivered,
            "only the ask that was answered is retired"
        );
    }

    #[tokio::test]
    async fn only_the_outstanding_request_id_is_answered() {
        let (srv, mut answer_rx, ..) = server_with_ask(permission_ask(None));

        handle_incoming_response(&srv, &allow_once(UNKNOWN_ID));
        assert!(answer_rx.is_empty(), "an unknown id is dropped");

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert_eq!(
            answer_rx.try_recv().ok(),
            Some(TaggedAnswer::new(PARENT_TOOL_USE_ID, PermissionAnswer::AllowOnce).encode())
        );

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert!(
            answer_rx.is_empty(),
            "a replayed answer cannot land on the next request"
        );
    }

    #[tokio::test]
    async fn elicitation_response_forwards_the_raw_result() {
        let (srv, mut answer_rx, ..) = server_with_ask(Ask::Elicitation);
        let raw = serde_json::json!({
            "id": ANSWERED_ID,
            "result": { "action": "accept", "content": { "q1": "axum" } },
        });

        handle_incoming_response(&srv, &raw);
        let forwarded = answer_rx.try_recv().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&forwarded).unwrap(),
            raw["result"]
        );
    }

    #[tokio::test]
    async fn discovered_models_are_pushed_to_the_client() {
        let (mut srv, .., mut out_rx) = test_server();
        srv.model_specs = vec![OFFLINE_SPEC.to_owned()];
        let batch = vec![DISCOVERED_SPEC.to_owned()];

        refresh_models(&mut srv, batch.clone());
        let update = out_rx.try_recv().expect("the fuller list is announced");
        let option = &update["params"]["update"]["configOptions"][0];
        assert_eq!(option["id"], methods::MODEL_CONFIG_ID);
        let selectable: Vec<&str> = option["options"]
            .as_array()
            .expect("the option is a select")
            .iter()
            .filter_map(|o| o["value"].as_str())
            .collect();
        assert!(
            selectable.contains(&OFFLINE_SPEC) && selectable.contains(&DISCOVERED_SPEC),
            "a batch is merged into the offline list, not swapped for it: {selectable:?}"
        );

        refresh_models(&mut srv, batch);
        assert!(out_rx.is_empty(), "a batch adding nothing is not announced");
    }

    /// Editor clients call `session/list` to offer resuming a session. The
    /// response must contain only the requested project's stored sessions,
    /// with the fields the picker renders (title, ISO 8601 updated_at).
    #[tokio::test]
    async fn list_sessions_returns_stored_sessions_for_their_cwd() {
        use std::str::FromStr as _;
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut here: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new(SELECTED_SPEC, "/project");
        here.replace_messages(vec![Message::user("keep".into())]);
        here.save(&SessionClaim::acquire(here.id, &dir).unwrap(), &dir)
            .unwrap();
        let mut elsewhere: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new(SELECTED_SPEC, "/other");
        elsewhere.replace_messages(vec![Message::user("away".into())]);
        elsewhere
            .save(&SessionClaim::acquire(elsewhere.id, &dir).unwrap(), &dir)
            .unwrap();

        let raw = serde_json::json!({ "params": { "cwd": "/project" } });
        let AgentResponse::ListSessionsResponse(list) = list_sessions(&raw, &dir).await.unwrap()
        else {
            panic!("session/list must answer ListSessionsResponse");
        };
        let json = serde_json::to_string(&list).unwrap();
        assert!(json.contains(&here.id.to_string()), "{json}");
        assert!(!json.contains(&elsewhere.id.to_string()), "{json}");
        assert_eq!(list.sessions.len(), 1);
        assert_eq!(list.sessions[0].title.as_deref(), Some("New session"));
        // Any u64 formats to some string; parsing is the real check.
        let ts = list.sessions[0]
            .updated_at
            .clone()
            .expect("a saved session has an updated_at");
        assert!(jiff::Timestamp::from_str(&ts).is_ok(), "{ts}");
    }

    /// `session/list` must work with no live session. An omitted cwd
    /// applies no filter: sessions of every project are listed.
    #[tokio::test]
    async fn session_list_dispatches_with_no_session_and_no_cwd() {
        let tmp = TempDir::new().unwrap();
        let storage = StateDir::from_path(tmp.path().to_path_buf());
        let cwd = std::env::current_dir().unwrap();
        let mut session: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new(SELECTED_SPEC, &cwd.to_string_lossy());
        session.replace_messages(vec![Message::user("here".into())]);
        session
            .save(
                &SessionClaim::acquire(session.id, &storage).unwrap(),
                &storage,
            )
            .unwrap();
        let mut foreign: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new(SELECTED_SPEC, "/somewhere-else");
        foreign.replace_messages(vec![Message::user("there".into())]);
        foreign
            .save(
                &SessionClaim::acquire(foreign.id, &storage).unwrap(),
                &storage,
            )
            .unwrap();

        let (mut srv, _answer_rx, mut out_rx) = test_server();
        srv.sessions.clear();
        let settings = settings(storage);
        let raw = serde_json::json!({ "params": {} });
        handle_request(
            &mut srv,
            "session/list",
            RequestId::Number(7),
            &raw,
            &settings,
        )
        .await;
        let resp = out_rx.try_recv().unwrap();
        let resp = resp.to_string();
        assert!(
            resp.contains(&session.id.to_string()),
            "session/list with no cwd must include the process cwd's session: {resp}"
        );
        assert!(
            resp.contains(&foreign.id.to_string()),
            "an omitted cwd applies no filter, so other projects are listed: {resp}"
        );
    }

    #[tokio::test]
    async fn load_history_round_trips_stored_messages() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let messages = vec![
            Message::user("rename foo to bar".into()),
            Message {
                role: Role::Assistant,
                content: vec![MsgBlock::Text {
                    text: "done".into(),
                }],
                display_text: None,
                ..Default::default()
            },
        ];
        let mut session: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new("anthropic/test-model", "/project");
        session.replace_messages(messages.clone());
        session.token_usage = TokenUsage {
            input: 1_000,
            output: 200,
            ..Default::default()
        };
        session
            .save(&SessionClaim::acquire(session.id, &dir).unwrap(), &dir)
            .unwrap();

        let id: MakiId = session.id;
        let restored = claim_history(&dir, id).await.unwrap().0;
        assert_eq!(restored.session.model, "anthropic/test-model");
        assert_eq!(
            serde_json::to_value(restored.session.messages()).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
        assert_eq!(restored.cwd, Some(PathBuf::from("/project")));
        assert_eq!(restored.session.token_usage, session.token_usage);
    }

    /// Resuming must bill what the session actually paid. If the per-model
    /// breakdown came back empty or lost its recorded costs, ACP would re-price
    /// the restored total against today's table and disagree with the TUI.
    #[tokio::test]
    async fn load_history_prices_a_resumed_session_at_what_it_paid() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new(RETIRED_SPEC, "/project");
        session.token_usage = TokenUsage {
            input: 1_000_000,
            output: 200_000,
            ..Default::default()
        };
        session.add_model_usage(
            RETIRED_MODEL_ID,
            StoredTokenUsage {
                input: 1_000_000,
                output: 200_000,
                cost: Some(RECORDED_COST),
                ..Default::default()
            },
        );
        session
            .save(&SessionClaim::acquire(session.id, &dir).unwrap(), &dir)
            .unwrap();

        let restored = claim_history(&dir, session.id).await.unwrap().0;
        let mut by_model = restored.session.usage_by_model().clone();
        assert_eq!(
            by_model[RETIRED_MODEL_ID].cost,
            Some(RECORDED_COST),
            "the per-model breakdown survives the file"
        );

        // Mirrors `load_session`: the recorded spec no longer parses, so the
        // selected model stands in, and that must not change the bill.
        let recorded_model = Model::from_spec(&restored.session.model)
            .unwrap_or_else(|_| Model::from_spec(SELECTED_SPEC).expect("a shipped model"));
        assert_eq!(
            settle_session(
                &restored.session.token_usage,
                &mut by_model,
                &recorded_model,
                false
            ),
            Some(RECORDED_COST)
        );
    }

    #[tokio::test]
    async fn load_history_records_absolute_cwd_only() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new("anthropic/test-model", "relative/project");
        session
            .save(&SessionClaim::acquire(session.id, &dir).unwrap(), &dir)
            .unwrap();
        assert_eq!(claim_history(&dir, session.id).await.unwrap().0.cwd, None);
    }

    #[tokio::test]
    async fn load_missing_session_is_session_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let missing = MakiId::generate();
        let err = claim_history(&dir, missing).await.unwrap_err();
        let expected = AcpError::session_not_found(SessionId::from(missing.to_string()));
        assert_eq!((err.code, err.data), (expected.code, expected.data));
    }

    #[test]
    fn converts_injected_mcp_servers() {
        let raw = serde_json::json!({
            "params": {
                "sessionId": MakiId::generate().to_string(),
                "cwd": "/project",
                "mcpServers": [
                    {
                        "type": "http",
                        "name": "kan.dev/mcp",
                        "url": "http://127.0.0.1:41012",
                        "headers": [{ "name": "Authorization", "value": "Bearer abc" }]
                    },
                    {
                        "name": "local",
                        "command": "/usr/bin/mcp",
                        "args": ["--stdio"],
                        "env": [{ "name": "TOKEN", "value": "t" }]
                    },
                    {
                        "type": "sse",
                        "name": "legacy",
                        "url": "http://127.0.0.1:41013",
                        "headers": []
                    }
                ]
            }
        });

        let req: LoadSessionRequest = parse_params(&raw).unwrap();
        let servers = injected_servers(&req.mcp_servers);
        assert_eq!(servers.len(), 2, "sse is dropped, not converted");

        let (name, RawTransport::Http(http)) = &servers[0] else {
            panic!("expected http transport");
        };
        assert_eq!(name, "kan-dev-mcp", "wire names are coerced to valid ones");
        assert_eq!(http.url, "http://127.0.0.1:41012");
        assert_eq!(
            http.headers.get("Authorization").map(String::as_str),
            Some("Bearer abc")
        );

        let (name, RawTransport::Stdio(stdio)) = &servers[1] else {
            panic!("expected stdio transport");
        };
        assert_eq!(name, "local");
        assert_eq!(stdio.command, ["/usr/bin/mcp", "--stdio"]);
        assert_eq!(
            stdio.environment.get("TOKEN").map(String::as_str),
            Some("t")
        );
    }

    const INIT_ID: i64 = 1;
    const OPEN_ID: i64 = 2;
    const LOAD_ID: i64 = 3;
    const RELAYED_SESSION: &str = "sess-in-child";
    const MISSING_FOLDER: &str = "gone";
    const CHILD_STARTED: &str = "a child starts for the other folder";
    const RELAYED_NOT_ANSWERED: &str =
        "the child answers a relayed request, the server stays quiet";

    enum Folder {
        Launch,
        Other,
    }

    fn rpc_request(id: i64, method: &str, params: Value) -> Value {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn open_in(cwd: &Path) -> Value {
        rpc_request(
            OPEN_ID,
            "session/new",
            serde_json::json!({ "cwd": cwd, "mcpServers": [] }),
        )
    }

    fn launched_in(launch: &Path, relay: bool) -> Settings {
        Settings {
            launch_dir: launch.canonicalize().unwrap(),
            relay,
            ..fixture_settings()
        }
    }

    #[test_case(Folder::Launch, true, false ; "the_launch_folder_runs_here")]
    #[test_case(Folder::Other, true, true ; "another_folder_goes_to_its_child")]
    #[test_case(Folder::Other, false, false ; "a_relayed_process_never_relays")]
    fn route_open_follows_the_canonical_cwd(cwd: Folder, relay: bool, to_child: bool) {
        let launch = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let cwd = match cwd {
            Folder::Launch => launch.path(),
            Folder::Other => other.path(),
        };
        let fixture = RelayFixture::new();

        let route = route_open(
            &fixture.srv,
            &open_in(cwd),
            &launched_in(launch.path(), relay),
        )
        .unwrap();

        let child_folder = match route {
            Route::Here => None,
            Route::Child(folder) => Some(folder),
        };
        assert_eq!(
            child_folder,
            to_child.then(|| other.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn an_open_in_a_missing_folder_names_it() {
        let launch = TempDir::new().unwrap();
        let missing = launch.path().join(MISSING_FOLDER);
        let fixture = RelayFixture::new();

        let Err(error) = route_open(
            &fixture.srv,
            &open_in(&missing),
            &launched_in(launch.path(), true),
        ) else {
            panic!("{CWD_UNUSABLE}");
        };

        assert_eq!(error.code, ErrorCode::InvalidParams);
        let detail = error.data.unwrap();
        assert!(
            detail
                .as_str()
                .unwrap()
                .contains(&*missing.to_string_lossy()),
            "{detail}"
        );
    }

    /// The child answers every request of its session under the client's own
    /// id, and the server never answers one itself.
    #[tokio::test]
    async fn a_session_in_another_folder_runs_in_its_child() {
        let launch = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let settings = launched_in(launch.path(), true);
        let mut fx = RelayFixture::new();
        let initialize = rpc_request(
            INIT_ID,
            "initialize",
            serde_json::json!({ "protocolVersion": 1 }),
        );
        handle_line(&mut fx.srv, &initialize.to_string(), &settings).await;
        assert_eq!(fx.next_to_client()["id"], INIT_ID);

        let open = open_in(other.path());
        handle_line(&mut fx.srv, &open.to_string(), &settings).await;
        assert!(fx.out_rx.try_recv().is_err(), "{RELAYED_NOT_ANSWERED}");
        let mut child = fx.spawned_rx.try_recv().expect(CHILD_STARTED);
        assert_eq!(child.folder, other.path().canonicalize().unwrap());
        let child_init = child.recv().await;
        assert_eq!(child_init["params"], initialize["params"]);
        child.answer(&child_init, serde_json::json!({})).await;
        fx.pump().await;
        let relayed_open = child.recv().await;
        assert_eq!(relayed_open["params"], open["params"]);
        child
            .answer(
                &relayed_open,
                serde_json::json!({ "sessionId": RELAYED_SESSION }),
            )
            .await;
        fx.pump().await;
        let opened = fx.next_to_client();
        assert_eq!(opened["id"], OPEN_ID);
        assert_eq!(opened["result"]["sessionId"], RELAYED_SESSION);
        assert!(fx.srv.sessions.is_empty());

        let prompt = rpc_request(
            PROMPT_ID,
            "session/prompt",
            serde_json::json!({ "sessionId": RELAYED_SESSION, "prompt": [] }),
        );
        handle_line(&mut fx.srv, &prompt.to_string(), &settings).await;
        assert!(fx.out_rx.try_recv().is_err(), "{RELAYED_NOT_ANSWERED}");
        let relayed_prompt = child.recv().await;
        assert_eq!(relayed_prompt["params"], prompt["params"]);
        let stop = serde_json::json!({ "stopReason": "end_turn" });
        child.answer(&relayed_prompt, stop.clone()).await;
        fx.pump().await;
        let answered = fx.next_to_client();
        assert_eq!(answered["id"], PROMPT_ID);
        assert_eq!(answered["result"], stop);

        let reload = rpc_request(
            LOAD_ID,
            "session/load",
            serde_json::json!({
                "sessionId": RELAYED_SESSION,
                "cwd": launch.path(),
                "mcpServers": [],
            }),
        );
        handle_line(&mut fx.srv, &reload.to_string(), &settings).await;
        assert!(fx.out_rx.try_recv().is_err(), "{RELAYED_NOT_ANSWERED}");
        assert_eq!(
            child.recv().await["params"],
            reload["params"],
            "a load of a child's session goes to that child, whatever its cwd"
        );
        assert!(fx.spawned_rx.try_recv().is_err(), "one child per folder");
    }
}

#[cfg(test)]
mod iso_tests {
    use super::iso_from_epoch;
    use test_case::test_case;

    #[test_case(0, None ; "unknown_sentinel_is_no_timestamp")]
    #[test_case(1_700_000_000, Some("2023-11-14T22:13:20Z") ; "epoch_seconds_to_utc")]
    #[test_case(u64::MAX, None ; "out_of_range_drops_the_timestamp")]
    fn iso_from_epoch_converts(secs: u64, expected: Option<&str>) {
        assert_eq!(iso_from_epoch(secs).as_deref(), expected);
    }
}
