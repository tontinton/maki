use std::collections::HashMap;
use std::io::Write;
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::{
    AgentNotification, AgentRequest, AgentResponse, ConfigOptionUpdate, ContentBlock,
    CurrentModeUpdate, EmbeddedResourceResource, Error as AcpError, ImageContent,
    InitializeRequest, JsonRpcMessage, LoadSessionRequest, McpServer, NewSessionRequest,
    Notification, PromptRequest, PromptResponse, Request, RequestId, RequestPermissionRequest,
    RequestPermissionResponse, Response, SessionId, SessionModeId, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, TextContent,
};
use color_eyre::eyre::Context;
use flume::{Sender, WeakSender};
use maki_agent::headless::{self, InteractiveHandle, InteractiveParams};
use maki_agent::mcp::config::{RawHttpFields, RawStdioFields, RawTransport};
use maki_agent::mcp::{self, McpHandle};
use maki_agent::permissions::PermissionAnswer;
use maki_agent::tools::{LocalTool, LocalTools, QUESTION_TOOL_NAME, ToolAudience, local_tool};
use maki_agent::types::AgentEvent;
use maki_agent::{
    AgentInput, AgentMode, Envelope, ImageMediaType, ImageSource, SessionEndReason, SessionEvents,
};
use maki_config::project::{self, TrustAnswer, TrustMode, policy_grant};
use maki_config::{MAX_SERVER_NAME_LEN, ModelPolicy, ProjectConfig, SessionDefaults, TrustConfig};
use maki_providers::model::Model;
use maki_providers::provider::{available_model_specs, fetch_all_models};
use maki_providers::{Message, TokenUsage, add_cost, settle_session};
use maki_storage::StateDir;
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::StoredTokenUsage;
use serde::Serialize;
use serde_json::Value;
use smol::Task;
use smol::io::AsyncBufReadExt;
use tracing::{debug, info, warn};

use crate::{AcpParams, SessionEndHook, elicitation, methods, permissions, translate};

const FIRST_OUTGOING_REQUEST_ID: i64 = 1000;
/// Turns already recorded were priced when they ran. `always_fast` is a live
/// config value, so reading it here would reprice history the user paid for at
/// standard rates. New turns still honour it.
const RESTORED_FAST: bool = false;

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
    Permission(Option<Sender<String>>),
    Elicitation,
}

type PendingState = Arc<Mutex<Pending>>;

struct SessionState {
    handle: InteractiveHandle,
    mcp: Option<McpHandle>,
    current_mode: AgentMode,
    current_model: String,
    pending: PendingState,
}

struct Server {
    out_tx: Sender<Value>,
    model_specs: Vec<String>,
    model_policy: Arc<ModelPolicy>,
    client_elicits_form: bool,
    defaults: SessionDefaults,
    session: Option<SessionState>,
    on_session_end: Option<SessionEndHook>,
}

impl Server {
    fn respond(&self, id: RequestId, result: Result<AgentResponse, AcpError>) {
        send(&self.out_tx, Response::new(id, result));
    }
}

enum Incoming {
    Line(String),
    Models(Vec<String>),
}

pub async fn serve(params: AcpParams) -> color_eyre::Result<()> {
    let (out_tx, out_rx) = flume::unbounded::<Value>();

    let writer_task = smol::spawn(async move {
        let stdout = std::io::stdout();
        while let Ok(msg) = out_rx.recv_async().await {
            let mut handle = stdout.lock();
            if serde_json::to_writer(&mut handle, &msg).is_ok() {
                let _ = handle.write_all(b"\n");
                let _ = handle.flush();
            }
        }
    });

    let mut server = Server {
        out_tx,
        model_specs: available_model_specs(&params.model_policy),
        model_policy: Arc::clone(&params.model_policy),
        client_elicits_form: false,
        defaults: params.defaults,
        session: None,
        on_session_end: params.on_session_end.clone(),
    };

    let (in_tx, in_rx) = flume::unbounded::<Incoming>();
    // Weak, so a discovery still in flight cannot keep the loop alive once stdin closes.
    discover_models(Arc::clone(&params.model_policy), in_tx.downgrade());
    let reader_task = smol::spawn(read_stdin(in_tx));

    while let Ok(incoming) = in_rx.recv_async().await {
        match incoming {
            Incoming::Line(line) => handle_line(&mut server, &line, &params).await,
            Incoming::Models(specs) => refresh_models(&mut server, specs),
        }
    }

    close_session(&mut server, SessionEndReason::Shutdown).await;
    drop(server);
    writer_task.await;
    reader_task.await.context("read stdin")?;

    Ok(())
}

/// Lives in its own task because `read_line` is not cancel safe: the main loop
/// waits on discovery too, and a dropped read would eat half a line.
async fn read_stdin(tx: Sender<Incoming>) -> std::io::Result<()> {
    let mut reader = smol::io::BufReader::new(smol::Unblock::new(std::io::stdin()));
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        if tx.send_async(Incoming::Line(line)).await.is_err() {
            return Ok(());
        }
    }
}

/// Static manifests miss providers that only list their models over the wire
/// (OpenRouter and friends), so the same discovery the TUI runs happens here,
/// in the background. Each batch leaves the moment it lands: the slowest source
/// is a cold catalog download, and a provider the client could already pick from
/// should not wait behind it.
fn discover_models(policy: Arc<ModelPolicy>, tx: WeakSender<Incoming>) {
    smol::spawn(async move {
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
    })
    .detach();
}

/// Discovery lands in batches after the client built its selector from the
/// offline list, so every batch that adds something announces the fuller list.
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
    let Some(session) = &srv.session else { return };
    let option = methods::model_config_option(&session.current_model, &srv.model_specs);
    session_update(
        &srv.out_tx,
        &SessionId::from(session.handle.session_id.to_string()),
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![option])),
    );
}

async fn handle_line(server: &mut Server, line: &str, params: &AcpParams) {
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
        handle_incoming_response(server, &raw);
    } else if let Some(method) = raw.get("method").and_then(Value::as_str) {
        match id {
            Some(id) => handle_request(server, method, id, &raw, params).await,
            None => handle_notification(server, method),
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
    params: &AcpParams,
) {
    let result = match method {
        "initialize" => {
            srv.client_elicits_form = parse_params::<InitializeRequest>(raw)
                .is_ok_and(|req| elicitation::supports_form(&req.client_capabilities));
            Ok(AgentResponse::InitializeResponse(
                methods::initialize_response(),
            ))
        }
        "session/new" => new_session(srv, raw, params).await,
        "session/load" => load_session(srv, raw, params).await,
        "session/prompt" => match handle_prompt(srv, raw, &id) {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        "session/set_mode" => handle_set_mode(srv, raw),
        "session/set_config_option" => handle_set_config(srv, raw),
        _ => Err(AcpError::method_not_found()),
    };
    srv.respond(id, result);
}

async fn new_session(
    srv: &mut Server,
    raw: &Value,
    params: &AcpParams,
) -> Result<AgentResponse, AcpError> {
    let req: NewSessionRequest = parse_params(raw)?;
    close_session(srv, SessionEndReason::Replaced).await;
    let project_config = trusted_project_config(
        &req.cwd,
        &params.storage,
        params.trust_mode,
        &params.trust_policy,
    );
    let mcp = start_mcp(&req.cwd, &req.mcp_servers, project_config.clone()).await;
    let session_ref = start_session(
        srv,
        params,
        req.cwd,
        None,
        Vec::new(),
        mcp,
        project_config,
        None,
    );
    maki_otel::emit::session_started(maki_otel::emit::START_FRESH, Some(session_ref.as_str()));
    let spec = params.model.spec();
    let resp = methods::new_session_response(session_ref.as_str())
        .config_options(vec![methods::model_config_option(&spec, &srv.model_specs)]);
    Ok(AgentResponse::NewSessionResponse(resp))
}

async fn load_session(
    srv: &mut Server,
    raw: &Value,
    params: &AcpParams,
) -> Result<AgentResponse, AcpError> {
    let req: LoadSessionRequest = parse_params(raw)?;
    let session_ref: SessionRef = req
        .session_id
        .0
        .parse()
        .map_err(|_| AcpError::resource_not_found(Some(req.session_id.0.to_string())))?;
    let mut restored = load_history(session_ref.id())?;
    close_session(srv, SessionEndReason::Replaced).await;
    let project_config = trusted_project_config(
        &req.cwd,
        &params.storage,
        params.trust_mode,
        &params.trust_policy,
    );
    let mcp = start_mcp(&req.cwd, &req.mcp_servers, project_config.clone()).await;
    let sid = SessionId::from(session_ref.to_string());
    let home = maki_storage::paths::home();
    let replay_cwd = restored.cwd.as_deref().unwrap_or(&req.cwd);
    for update in translate::replay_history(&restored.history, replay_cwd, home.as_deref()) {
        session_update(&srv.out_tx, &sid, update);
    }
    // Priced against the model the session recorded, not the one selected now
    // (which may cost 10x more or less). Later turns add their own exact cost.
    let recorded_model = Model::from_spec(&restored.model).unwrap_or_else(|_| params.model.clone());
    let restored_cost = settle_session(
        &restored.usage,
        &mut restored.by_model,
        &recorded_model,
        RESTORED_FAST,
    );
    let started = start_session(
        srv,
        params,
        req.cwd,
        Some(session_ref),
        restored.history,
        mcp,
        project_config,
        restored_cost,
    );
    maki_otel::emit::session_started(maki_otel::emit::START_RESUME, Some(started.as_str()));
    let spec = params.model.spec();
    let resp = methods::load_session_response()
        .config_options(vec![methods::model_config_option(&spec, &srv.model_specs)]);
    Ok(AgentResponse::LoadSessionResponse(resp))
}

/// Spawns a session and installs it as the server's current one. Spawning
/// alone is not a useful state: the event stream has exactly one reader, so it
/// must be handed to the pump here rather than travel any further.
#[allow(clippy::too_many_arguments)]
fn start_session(
    srv: &mut Server,
    params: &AcpParams,
    cwd: PathBuf,
    session_id: Option<SessionRef>,
    history: Vec<Message>,
    mcp: Option<McpHandle>,
    project_config: ProjectConfig,
    initial_cost: Option<f64>,
) -> SessionRef {
    let pending = PendingState::default();
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
    // The ACP process cwd owns env, Lua, and application config, but the client
    // picks the session cwd. So permissions, where a saved answer lands, and
    // MCP config all follow the session's project, not ours.
    let project_trusted = project_config.is_trusted();
    let permissions_config = maki_config::load_permissions(&project_config);
    let (handle, events) = headless::spawn_interactive(InteractiveParams {
        model: params.model.clone(),
        config: params.config.clone(),
        permissions_config,
        timeouts: params.timeouts,
        prompt_slots: Arc::clone(&params.prompt_slots),
        excluded_tools,
        mcp_handle: mcp.clone(),
        initial_wd: cwd.clone(),
        session_id,
        initial_history: history,
        yolo: params.yolo,
        system_prompt_override: None,
        append_system_prompt: None,
        defaults: params.defaults,
        model_policy: Arc::clone(&params.model_policy),
        plugin_rules: Arc::clone(&params.plugin_rules),
        project_config,
        local_tools,
    });
    let session_ref = handle.session_id.clone();
    start_event_pump(
        events,
        session_ref.clone(),
        srv.out_tx.clone(),
        Arc::clone(&pending),
        cwd,
        maki_storage::paths::home(),
        project_trusted,
        initial_cost,
    )
    .detach();
    srv.session = Some(SessionState {
        handle,
        mcp,
        current_mode: AgentMode::Build,
        current_model: params.model.spec(),
        pending,
    });
    session_ref
}

/// Registers each ask before sending so a response cannot race past it.
fn ask_client(
    out_tx: &Sender<Value>,
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
fn question_tool(out_tx: WeakSender<Value>, pending: PendingState) -> LocalTool {
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

            let guard = rx.lock().await;
            let request = AgentRequest::CreateElicitationRequest(request);
            let id = ask_client(&out_tx, &pending, Ask::Elicitation, request);
            let response = ctx.cancel.race(guard.recv_async()).await;
            // Cleared while still holding the channel, so a stale id cannot
            // clobber whatever ask comes next.
            pending.lock().unwrap().asks.remove(&id);
            drop(guard);

            Ok(match response {
                Ok(Ok(raw)) => elicitation::format_response(&input, &raw),
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
    let matched = decision
        .state
        .unanswered()
        .and_then(|question| policy_grant(question, policy));
    if let Some(pattern) = matched {
        // ACP has no card, so policy is the only yes a cwd with no stored
        // decision can get. Recorded like any other yes so `maki trust list`
        // shows what this server trusted on the client's behalf. `unanswered`
        // and not `question`: a recorded `Never` is a stored decision, and
        // granting over it would wipe the rejection out of the store.
        info!(%pattern, cwd = %cwd.display(), "ACP folder trusted by trust.paths policy");
        decision = project::apply_answer(storage, decision, TrustAnswer::Trust);
    }
    // ACP never asks, so the restriction notice is part of what it reports.
    for warning in decision.notices() {
        warn!(%warning, "ACP project configuration trust warning");
    }
    decision.project_config
}

/// Stop the old session before the next one starts, so two generations of the
/// same MCP servers never fight over a port or a lock file.
async fn close_session(srv: &mut Server, reason: SessionEndReason) {
    let Some(state) = srv.session.take() else {
        return;
    };
    if let Some(cb) = &srv.on_session_end {
        cb(state.handle.session_id.id(), reason).await;
    }
    // The event pump dies with the session, so the prompt it owed an answer to
    // has to be answered here or the client waits on it forever.
    if let Some(id) = state.pending.lock().unwrap().prompt.take() {
        let resp = PromptResponse::new(StopReason::Cancelled);
        send(
            &srv.out_tx,
            Response::new(id, Ok(AgentResponse::PromptResponse(resp))),
        );
    }
    state.handle.task.cancel().await;
    if let Some(mcp) = state.mcp {
        mcp.shutdown().await;
    }
}

#[derive(Debug)]
struct Restored {
    history: Vec<Message>,
    /// Only set when the session recorded an absolute cwd.
    cwd: Option<PathBuf>,
    usage: TokenUsage,
    by_model: HashMap<String, StoredTokenUsage>,
    model: String,
}

fn load_history(session_id: MakiId) -> Result<Restored, AcpError> {
    let storage = maki_storage::StateDir::resolve()
        .map_err(|e| AcpError::internal_error().data(json_str(&e)))?;
    load_history_from(&storage, session_id)
}

/// History plus the absolute cwd the session recorded in its header. Tool
/// inputs from a past run resolve against that cwd, not the client's current
/// one; a non-absolute recording falls back to the caller's cwd.
fn load_history_from(
    storage: &maki_storage::StateDir,
    session_id: MakiId,
) -> Result<Restored, AcpError> {
    let session: maki_storage::sessions::Session<
        Message,
        maki_providers::TokenUsage,
        maki_agent::ToolOutput,
    > = maki_storage::sessions::Session::load(session_id, storage).map_err(|e| {
        AcpError::resource_not_found(Some(format!("session/{session_id}"))).data(json_str(&e))
    })?;
    let recorded = if Path::new(&session.cwd).is_absolute() {
        Some(PathBuf::from(&session.cwd))
    } else {
        None
    };
    Ok(Restored {
        cwd: recorded,
        usage: session.token_usage,
        by_model: session.usage_by_model().clone(),
        model: session.model.clone(),
        history: session.take_messages(),
    })
}

fn handle_prompt(srv: &mut Server, raw: &Value, id: &RequestId) -> Result<(), AcpError> {
    let req: PromptRequest = parse_params(raw)?;
    let session = srv.session.as_ref().ok_or_else(no_session)?;

    let (message, images) = extract_prompt_content(&req.prompt);
    let input =
        AgentInput::from_defaults(message, session.current_mode.clone(), images, srv.defaults);

    // One outstanding id per session, checked and set under the same guard:
    // `input_tx` is unbounded, so a second prompt would queue happily and
    // overwrite the first id, leaving that request unanswered forever.
    let mut pending = session.pending.lock().unwrap();
    if pending.prompt.is_some() {
        return Err(AcpError::new(-32603, "a prompt is already running"));
    }
    session
        .handle
        .input_tx
        .send(input)
        .map_err(|_| AcpError::new(-32603, "session ended"))?;
    pending.prompt = Some(id.clone());
    Ok(())
}

fn handle_set_mode(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionModeRequest = parse_params(raw)?;
    let mode_str = req.mode_id.0.to_string();
    let new_mode = methods::mode_id_to_agent_mode(&mode_str)
        .ok_or_else(|| AcpError::new(-32602, format!("unknown mode: {mode_str}")))?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session.current_mode = new_mode;

    let sid = SessionId::from(session.handle.session_id.to_string());
    session_update(
        &srv.out_tx,
        &sid,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(mode_str))),
    );
    Ok(AgentResponse::SetSessionModeResponse(
        SetSessionModeResponse::new(),
    ))
}

fn handle_set_config(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionConfigOptionRequest = parse_params(raw)?;
    if req.config_id.0.as_ref() != methods::MODEL_CONFIG_ID {
        let detail = format!("unknown config option: {}", req.config_id);
        return Err(AcpError::invalid_params().data(json_str(&detail)));
    }

    let spec = req.value.0.to_string();
    if !srv.model_policy.allows(&spec) {
        return Err(AcpError::invalid_params().data(json_str(&"model is not allowed by policy")));
    }
    let model =
        Model::from_spec(&spec).map_err(|e| AcpError::invalid_params().data(json_str(&e)))?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
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

fn handle_notification(srv: &Server, method: &str) {
    match method {
        "session/cancel" => {
            if let Some(session) = &srv.session {
                // Every answer still in flight belongs to the cancelled turn,
                // so forget the ids and let them be dropped on arrival.
                session.pending.lock().unwrap().asks.clear();
                let _ = session.handle.cancel_tx.try_send(());
            }
        }
        _ => debug!(method, "unknown notification"),
    }
}

fn handle_incoming_response(srv: &Server, raw: &Value) {
    let Some(session) = &srv.session else { return };
    let Some(id) = raw.get("id").and_then(Value::as_i64) else {
        return;
    };
    let ask = session.pending.lock().unwrap().asks.remove(&id);
    let Some(ask) = ask else {
        warn!(id, "response for an unknown request id");
        return;
    };
    let (answer, answer_tx) = match &ask {
        Ask::Permission(answer_tx) => (permission_answer(raw).encode(), answer_tx.as_ref()),
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
            }) => images.push(ImageSource {
                media_type: image_media_type(mime_type),
                data: Arc::from(data.as_str()),
            }),
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
    session_id: SessionRef,
    out_tx: Sender<Value>,
    pending: PendingState,
    cwd: PathBuf,
    home: Option<PathBuf>,
    project_trusted: bool,
    initial_cost: Option<f64>,
) -> Task<()> {
    smol::spawn(async move {
        let sid = SessionId::from(session_id.to_string());
        let mut cost_total = initial_cost;
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
                add_cost(&mut cost_total, tc.cost);
            }

            let update = match event {
                AgentEvent::PermissionRequest { id, tool, scopes } => {
                    let tool = tool.to_string();
                    let scope = format!("{tool}: {}", scopes.join(", "));
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
                            ),
                            permissions::permission_options(project_trusted),
                        ));
                    ask_client(&out_tx, &pending, Ask::Permission(answer_tx), request);
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
                AgentEvent::TurnComplete(event) => translate::usage_update(&event, cost_total),
                AgentEvent::Done { reason, .. } => {
                    tool_inputs.clear();
                    if let Some(id) = pending.lock().unwrap().prompt.take() {
                        let resp = PromptResponse::new(translate::map_done_reason(reason));
                        send(
                            &out_tx,
                            Response::new(id, Ok(AgentResponse::PromptResponse(resp))),
                        );
                    }
                    continue;
                }
                AgentEvent::Error { message } => {
                    // A turn that dies on a provider 500 never reaches `Done`,
                    // and the pump outlives the session, so without this the
                    // whole file a `write` was carrying stays pinned forever.
                    tool_inputs.clear();
                    if let Some(id) = pending.lock().unwrap().prompt.take() {
                        let error = AcpError::internal_error().data(Value::String(message));
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

fn send(out_tx: &Sender<Value>, msg: impl Serialize) {
    if let Ok(json) = serde_json::to_value(JsonRpcMessage::wrap(msg)) {
        let _ = out_tx.send(json);
    }
}

fn session_update(out_tx: &Sender<Value>, sid: &SessionId, update: SessionUpdate) {
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

fn no_session() -> AcpError {
    AcpError::new(-32600, "no active session")
}

fn parse_params<T: serde::de::DeserializeOwned>(raw: &Value) -> Result<T, AcpError> {
    serde_json::from_value(raw.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|e| AcpError::invalid_params().data(json_str(&e)))
}

fn json_str(e: &impl std::fmt::Display) -> Value {
    Value::String(e.to_string())
}

#[cfg(test)]
mod tests {
    use maki_agent::permissions::PermissionManager;
    use maki_agent::{DoneReason, EventSender, SubagentInfo, ToolStartEvent, TurnCompleteEvent};
    use maki_config::project::TrustQuestion;
    use maki_config::{Effect, ToolKey, TrustFileConfig};
    use maki_providers::{ContentBlock as MsgBlock, Role, TokenUsage};
    use maki_storage::StateDir;
    use maki_storage::sessions::Session;
    use maki_storage::trusted_folders::{CanonicalFolder, TrustStatus, TrustedFolders};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const ANSWERED_ID: i64 = 1001;
    const UNKNOWN_ID: i64 = 1002;
    const DISCOVERED_SPEC: &str = "openrouter/discovered-model";
    const OFFLINE_SPEC: &str = "openai/gpt-5";
    const SELECTED_SPEC: &str = "openai/gpt-5.6-sol";
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
            ProjectConfig::for_project(project.path()).config_root()
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
        let (done_tx, done_rx) = flume::bounded(1);
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

    fn test_server() -> (Server, flume::Receiver<String>, flume::Receiver<Value>) {
        let (answer_tx, answer_rx) = flume::unbounded();
        let (out_tx, out_rx) = flume::unbounded();
        let handle = InteractiveHandle {
            tool_names: Vec::new(),
            input_tx: flume::unbounded().0,
            answer_tx,
            cancel_tx: flume::unbounded().0,
            model_tx: flume::unbounded().0,
            session_id: SessionRef::from(MakiId::generate()),
            permissions: Arc::new(PermissionManager::new(
                maki_config::PermissionsConfig::default(),
                PathBuf::from("/project"),
                ProjectConfig::for_project(Path::new("/project")),
                Arc::default(),
            )),
            task: smol::spawn(async {}),
        };
        let server = Server {
            out_tx,
            model_specs: Vec::new(),
            model_policy: Arc::new(ModelPolicy::default()),
            client_elicits_form: false,
            defaults: SessionDefaults::default(),
            on_session_end: None,
            session: Some(SessionState {
                handle,
                mcp: None,
                current_mode: AgentMode::Build,
                current_model: String::new(),
                pending: Arc::default(),
            }),
        };
        (server, answer_rx, out_rx)
    }

    fn pending(srv: &Server) -> &PendingState {
        &srv.session
            .as_ref()
            .expect("a session is installed")
            .pending
    }

    fn server_with_ask(ask: Ask) -> (Server, flume::Receiver<String>, flume::Receiver<Value>) {
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
    const PROMPT_ID: i64 = 7;
    const SUBAGENT_COST: f64 = 0.25;
    const TURN_COST: f64 = 0.5;
    const CONTEXT_WINDOW: u32 = 200_000;
    const PARENT_TOOL_USE_ID: &str = "toolu_1";
    const SUBAGENT_NAME: &str = "task";
    const CHILD_TOOL_USE_IDS: [&str; 2] = ["child_write_1", "child_write_2"];
    const PERMISSION_TOOL: &str = "write";
    const MAIN_PERMISSION_TITLE: &str = "write: /project";
    const CHILD_PERMISSION_TITLE: &str = "task: write: /project";
    const TURN_ERROR: &str = "provider returned 500";
    /// One tool id arriving from two runs of the same session. `openai_compat`
    /// mints unique ids now, but a provider can still repeat one, and the pump
    /// has to keep the runs apart either way.
    const COLLIDING_TOOL_USE_ID: &str = "maki_unnamed_0";
    /// These cover cost and transcript plumbing, not the trust-scoped wording
    /// of permission options.
    const PUMP_TRUSTED: bool = true;
    fn run_pump(srv: &Server, initial_cost: Option<f64>, feed: impl FnOnce(&EventSender)) {
        let session = srv.session.as_ref().expect("a session is installed");
        let (guard, events) = maki_agent::event_stream();
        let pump = start_event_pump(
            events,
            session.handle.session_id.clone(),
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
        smol::block_on(pump);
    }

    fn turn_complete(cost: f64) -> Box<TurnCompleteEvent> {
        Box::new(TurnCompleteEvent {
            message: Message::user(String::new()),
            usage: TokenUsage::default(),
            model: SELECTED_SPEC.to_owned(),
            cost: Some(cost),
            context_size: None,
            context_window: CONTEXT_WINDOW,
        })
    }

    fn subagent(answer_tx: Option<Sender<String>>) -> SubagentInfo {
        SubagentInfo {
            parent_tool_use_id: PARENT_TOOL_USE_ID.to_owned(),
            name: SUBAGENT_NAME.to_owned(),
            prompt: None,
            model: None,
            opts: None,
            answer_tx,
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
        }
    }

    #[test_case(false ; "answers_reach_the_agent_that_asked_even_out_of_order")]
    #[test_case(true ; "cancel_drops_every_outstanding_answer")]
    fn concurrent_subagent_permissions(cancel: bool) {
        let (srv, main_rx, out_rx) = test_server();
        let children = [flume::unbounded(), flume::unbounded()];
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
        });

        let requests: Vec<_> = out_rx.try_iter().collect();
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
            handle_notification(&srv, "session/cancel");
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
        let child_rx = children.map(|(_, rx)| rx);
        for (rx, expected) in child_rx.iter().chain([&main_rx]).zip([
            PermissionAnswer::Deny,
            PermissionAnswer::AllowOnce,
            PermissionAnswer::AllowSession,
        ]) {
            assert_eq!(rx.try_recv().ok(), (!cancel).then(|| expected.encode()));
            assert!(rx.is_empty(), "each answer is delivered only once");
        }
    }

    /// Without a channel of its own a child's answer would land in the main
    /// agent's, and the main agent's next wait would eat it as its own.
    #[test]
    fn a_subagent_permission_without_an_answer_channel_is_never_asked() {
        let (srv, main_rx, out_rx) = test_server();
        run_pump(&srv, None, |sender| {
            sender
                .send_envelope(Envelope {
                    event: permission_request(CHILD_TOOL_USE_IDS[0]),
                    subagent: Some(subagent(None)),
                    run_id: 0,
                })
                .unwrap();
        });

        assert!(out_rx.is_empty(), "the client is never asked");
        assert!(main_rx.is_empty(), "the main agent keeps its own turn");
        assert!(
            pending(&srv).lock().unwrap().asks.is_empty(),
            "nothing is left waiting for an answer"
        );
    }

    /// The close marker rides the same FIFO as the events, so a turn still
    /// streaming when the session got replaced is reported in full and the
    /// client's outstanding `session/prompt` is answered instead of hanging.
    #[test]
    fn event_pump_delivers_everything_queued_before_the_close() {
        let (srv, .., out_rx) = test_server();
        pending(&srv).lock().unwrap().prompt = Some(RequestId::Number(PROMPT_ID));
        run_pump(&srv, None, |sender| {
            sender
                .send(AgentEvent::TextDelta {
                    text: QUEUED_TEXT.to_owned(),
                })
                .unwrap();
            sender.send(done_event()).unwrap();
        });

        let chunk = out_rx.try_recv().expect("the queued text reaches the wire");
        let update = &chunk["params"]["update"];
        assert_eq!(update["sessionUpdate"], "agent_message_chunk");
        assert_eq!(update["content"]["text"], QUEUED_TEXT);

        let answer = out_rx.try_recv().expect("the pending prompt is answered");
        assert_eq!(answer["id"], PROMPT_ID);
        assert_eq!(answer["result"]["stopReason"], "end_turn");
        assert!(pending(&srv).lock().unwrap().prompt.is_none());
    }

    /// A cached input holds the whole file a `write` is about to lay down, and
    /// this pump lives as long as the session does, so a turn that died on a
    /// provider error has to let go of it just like a turn that finished.
    #[test_case(None, true ; "a_live_turn_shows_the_file_its_tool_call_named")]
    #[test_case(Some(done_event()), false ; "a_finished_turn_releases_its_tool_inputs")]
    #[test_case(Some(AgentEvent::Error { message: TURN_ERROR.to_owned() }), false ; "a_failed_turn_releases_its_tool_inputs")]
    fn a_terminal_event_releases_the_turns_tool_inputs(
        terminal: Option<AgentEvent>,
        keeps_input: bool,
    ) {
        let (srv, .., out_rx) = test_server();
        run_pump(&srv, None, |sender| {
            sender.send(write_tool_start(PARENT_TOOL_USE_ID)).unwrap();
            if let Some(event) = terminal {
                sender.send(event).unwrap();
            }
            sender.send(permission_request(PARENT_TOOL_USE_ID)).unwrap();
        });

        let request = out_rx
            .try_iter()
            .last()
            .expect("the permission request reaches the client");
        let call = &request["params"]["toolCall"];
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(call["title"], MAIN_PERMISSION_TITLE);
        assert_eq!(!call["rawInput"].is_null(), keeps_input, "{request}");
    }

    /// Ids are only unique inside one run, so a child can ask about a call whose
    /// id the parent already cached. The dialog has to stay title only, or it
    /// would name a file the subagent is not touching.
    #[test]
    fn a_subagent_permission_ignores_a_colliding_cached_input() {
        let (srv, .., out_rx) = test_server();
        let (answer_tx, _answer_rx) = flume::unbounded();
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
        });

        let request = out_rx
            .try_iter()
            .last()
            .expect("the permission request reaches the client");
        let call = &request["params"]["toolCall"];
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(call["toolCallId"], PARENT_TOOL_USE_ID);
        assert_eq!(call["title"], CHILD_PERMISSION_TITLE);
        for field in ["kind", "locations", "rawInput", "content"] {
            assert!(call[field].is_null(), "{field} must stay unset: {request}");
        }
    }

    /// A resumed session opens with a bill, and subagent turns spend against it
    /// even though their events never enter the transcript.
    #[test]
    fn event_pump_folds_restored_and_subagent_cost_into_the_usage_update() {
        let (srv, .., out_rx) = test_server();
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
        });

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

    #[test]
    fn close_session_awaits_the_session_end_hook() {
        let (mut srv, ..) = test_server();
        let ended = srv.session.as_ref().unwrap().handle.session_id.id();
        let (ended_tx, ended_rx) = flume::bounded(1);
        srv.on_session_end = Some(Arc::new(move |id, reason| {
            let ended_tx = ended_tx.clone();
            Box::pin(async move {
                let _ = ended_tx.send((id, reason));
            })
        }));

        smol::block_on(close_session(&mut srv, SessionEndReason::Replaced));

        assert_eq!(
            ended_rx.try_recv().ok(),
            Some((ended, SessionEndReason::Replaced))
        );
        assert!(srv.session.is_none(), "close must take the session");
    }

    #[test]
    fn only_the_outstanding_request_id_is_answered() {
        let (srv, answer_rx, ..) = server_with_ask(Ask::Permission(None));

        handle_incoming_response(&srv, &allow_once(UNKNOWN_ID));
        assert!(answer_rx.is_empty(), "an unknown id is dropped");

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert_eq!(
            answer_rx.try_recv().ok(),
            Some(PermissionAnswer::AllowOnce.encode())
        );

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert!(
            answer_rx.is_empty(),
            "a replayed answer cannot land on the next request"
        );
    }

    #[test]
    fn elicitation_response_forwards_the_raw_result() {
        let (srv, answer_rx, ..) = server_with_ask(Ask::Elicitation);
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

    #[test]
    fn discovered_models_are_pushed_to_the_client() {
        let (mut srv, .., out_rx) = test_server();
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

    #[test]
    fn load_history_round_trips_stored_messages() {
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
        session.save(&dir).unwrap();

        let id: MakiId = session.id;
        let restored = load_history_from(&dir, id).unwrap();
        assert_eq!(restored.model, "anthropic/test-model");
        assert_eq!(
            serde_json::to_value(&restored.history).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
        assert_eq!(restored.cwd, Some(PathBuf::from("/project")));
        assert_eq!(restored.usage, session.token_usage);
    }

    /// Resuming must bill what the session actually paid. If `by_model` came
    /// back empty or lost its recorded costs, ACP would re-price the restored
    /// total against today's table and disagree with the TUI.
    #[test]
    fn load_history_prices_a_resumed_session_at_what_it_paid() {
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
        session.save(&dir).unwrap();

        let mut restored = load_history_from(&dir, session.id).unwrap();
        assert_eq!(
            restored.by_model[RETIRED_MODEL_ID].cost,
            Some(RECORDED_COST),
            "the per-model breakdown survives the file"
        );

        // Mirrors `load_session`: the recorded spec no longer parses, so the
        // selected model stands in, and that must not change the bill.
        let recorded_model = Model::from_spec(&restored.model)
            .unwrap_or_else(|_| Model::from_spec(SELECTED_SPEC).expect("a shipped model"));
        assert_eq!(
            settle_session(
                &restored.usage,
                &mut restored.by_model,
                &recorded_model,
                false
            ),
            Some(RECORDED_COST)
        );
    }

    #[test]
    fn load_history_records_absolute_cwd_only() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new("anthropic/test-model", "relative/project");
        session.save(&dir).unwrap();
        assert_eq!(load_history_from(&dir, session.id).unwrap().cwd, None);
    }

    #[test]
    fn load_missing_session_is_resource_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let err = load_history_from(&dir, MakiId::generate()).unwrap_err();
        assert_eq!(err.code, AcpError::resource_not_found(None).code);
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
}
