use std::collections::HashMap;
use std::io::Write;
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::{
    AgentNotification, AgentRequest, AgentResponse, ContentBlock, CurrentModeUpdate,
    EmbeddedResourceResource, Error as AcpError, ImageContent, InitializeRequest, JsonRpcMessage,
    LoadSessionRequest, McpServer, NewSessionRequest, Notification, PromptRequest, PromptResponse,
    Request, RequestId, RequestPermissionRequest, RequestPermissionResponse, Response, SessionId,
    SessionModeId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
    TextContent, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};
use color_eyre::eyre::Context;
use flume::{Receiver, Sender};
use maki_agent::headless::{self, InteractiveHandle, InteractiveParams};
use maki_agent::mcp::config::{RawHttpFields, RawStdioFields, RawTransport};
use maki_agent::mcp::{self, McpHandle};
use maki_agent::permissions::PermissionAnswer;
use maki_agent::tools::{LocalToolFn, LocalTools, QUESTION_TOOL_NAME, local_tool};
use maki_agent::types::AgentEvent;
use maki_agent::{AgentInput, AgentMode, Envelope, ImageMediaType, ImageSource};
use maki_config::MAX_SERVER_NAME_LEN;
use maki_providers::Message;
use maki_providers::model::Model;
use maki_providers::provider::available_model_specs;
use maki_storage::id::{MakiId, SessionRef};
use serde::Serialize;
use serde_json::Value;
use smol::io::AsyncBufReadExt;
use tracing::{debug, warn};

use crate::{AcpParams, elicitation, methods, permissions, translate};

const FIRST_OUTGOING_REQUEST_ID: i64 = 1000;

/// Ids come from here and are never reused, so a late answer for a closed
/// session cannot match a request of the session that replaced it.
static NEXT_OUTGOING_REQUEST_ID: AtomicI64 = AtomicI64::new(FIRST_OUTGOING_REQUEST_ID);

/// What the client still owes us. `ask` is the one outstanding request that
/// blocks a tool (permission or elicitation): there can only be one, because
/// both wait on the agent's single answer channel.
#[derive(Default)]
struct Pending {
    prompt: Option<RequestId>,
    ask: Option<(i64, AskKind)>,
}

enum AskKind {
    Permission,
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
    model_policy: Arc<maki_config::ModelPolicy>,
    client_elicits_form: bool,
    session: Option<SessionState>,
}

impl Server {
    fn respond(&self, id: RequestId, result: Result<AgentResponse, AcpError>) {
        send(&self.out_tx, Response::new(id, result));
    }
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
        session: None,
    };

    let stdin = smol::Unblock::new(std::io::stdin());
    let mut reader = smol::io::BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await.context("read stdin")? == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let raw: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "invalid JSON on stdin");
                server.respond(RequestId::Null, Err(AcpError::parse_error()));
                continue;
            }
        };

        let id = raw.get("id").map(request_id);

        if raw.get("result").is_some() || raw.get("error").is_some() {
            handle_incoming_response(&server, &raw);
        } else if let Some(method) = raw.get("method").and_then(Value::as_str) {
            match id {
                Some(id) => handle_request(&mut server, method, id, &raw, &params).await,
                None => handle_notification(&server, method),
            }
        } else if let Some(id) = id {
            server.respond(id, Err(AcpError::invalid_request()));
        }
    }

    drop(server);
    writer_task.await;

    Ok(())
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
    close_session(srv).await;
    let mcp = start_mcp(&req.cwd, &req.mcp_servers).await;
    let cwd = req.cwd.clone();
    let (handle, pending) = spawn_session(srv, params, req.cwd, None, Vec::new(), mcp.clone());
    let spec = params.model.spec();
    let resp = methods::new_session_response(handle.session_id.as_str())
        .config_options(vec![methods::model_config_option(&spec, &srv.model_specs)]);
    install_session(srv, handle, mcp, spec, pending, cwd);
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
    let (history, recorded_cwd) = load_history(session_ref.id())?;
    close_session(srv).await;
    let mcp = start_mcp(&req.cwd, &req.mcp_servers).await;
    let sid = SessionId::from(session_ref.to_string());
    let home = maki_storage::paths::home();
    let replay_cwd = recorded_cwd.as_deref().unwrap_or(&req.cwd);
    for update in translate::replay_history(&history, replay_cwd, home.as_deref()) {
        session_update(&srv.out_tx, &sid, update);
    }
    let cwd = req.cwd.clone();
    let (handle, pending) = spawn_session(
        srv,
        params,
        req.cwd,
        Some(session_ref),
        history,
        mcp.clone(),
    );
    let spec = params.model.spec();
    let resp = methods::load_session_response()
        .config_options(vec![methods::model_config_option(&spec, &srv.model_specs)]);
    install_session(srv, handle, mcp, spec, pending, cwd);
    Ok(AgentResponse::LoadSessionResponse(resp))
}

fn spawn_session(
    srv: &Server,
    params: &AcpParams,
    cwd: PathBuf,
    session_id: Option<SessionRef>,
    history: Vec<Message>,
    mcp_handle: Option<McpHandle>,
) -> (InteractiveHandle, PendingState) {
    let pending = PendingState::default();
    // Without form elicitation the question tool would spin forever waiting
    // for a TUI that does not exist, so it is dropped and the model asks in
    // plain text instead.
    let (excluded_tools, local_tools) = if srv.client_elicits_form {
        let tool = question_tool(srv.out_tx.clone(), Arc::clone(&pending));
        let map: LocalTools = Arc::new(HashMap::from([(QUESTION_TOOL_NAME.to_owned(), tool)]));
        (Vec::new(), map)
    } else {
        (vec![QUESTION_TOOL_NAME], LocalTools::default())
    };
    let handle = headless::spawn_interactive(InteractiveParams {
        model: params.model.clone(),
        config: params.config.clone(),
        permissions_config: params.permissions_config.clone(),
        timeouts: params.timeouts,
        prompt_slots: Arc::clone(&params.prompt_slots),
        excluded_tools,
        mcp_handle,
        initial_wd: cwd,
        session_id,
        initial_history: history,
        yolo: params.yolo,
        system_prompt_override: None,
        append_system_prompt: None,
        workflow: false,
        model_policy: Arc::clone(&params.model_policy),
        plugin_rules: Arc::clone(&params.plugin_rules),
        local_tools,
    });
    (handle, pending)
}

/// Sends a request the client must answer and records it as the outstanding
/// ask, registered before sending so the response can never race past us.
fn ask_client(
    out_tx: &Sender<Value>,
    pending: &PendingState,
    kind: AskKind,
    request: AgentRequest,
) -> i64 {
    let id = NEXT_OUTGOING_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    pending.lock().unwrap().ask = Some((id, kind));
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
/// answer channel as permissions, so at most one ask is in flight.
fn question_tool(out_tx: Sender<Value>, pending: PendingState) -> LocalToolFn {
    local_tool(move |input, ctx| {
        let out_tx = out_tx.clone();
        let pending = Arc::clone(&pending);
        Box::pin(async move {
            let session_id = ctx
                .session_id
                .as_ref()
                .map(ToString::to_string)
                .ok_or("no session")?;
            let request = elicitation::form_request(&session_id, ctx.tool_use_id, &input)?;
            let rx = ctx.user_response_rx.as_ref().ok_or("no answer channel")?;

            let guard = rx.lock().await;
            let request = AgentRequest::CreateElicitationRequest(request);
            let id = ask_client(&out_tx, &pending, AskKind::Elicitation, request);
            let response = ctx.cancel.race(guard.recv_async()).await;
            // Cleared while still holding the channel, so a stale id cannot
            // clobber whatever ask comes next.
            let _ = pending
                .lock()
                .unwrap()
                .ask
                .take_if(|(ask_id, _)| *ask_id == id);
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
async fn start_mcp(cwd: &Path, servers: &[McpServer]) -> Option<McpHandle> {
    let (handle, errors) = mcp::start_with_extra(cwd, injected_servers(servers)).await;
    if !errors.is_empty() {
        warn!(%errors, "MCP config errors");
    }
    handle
}

/// Stop the old session before the next one starts, so two generations of the
/// same MCP servers never fight over a port or a lock file.
async fn close_session(srv: &mut Server) {
    let Some(state) = srv.session.take() else {
        return;
    };
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

fn install_session(
    srv: &mut Server,
    handle: InteractiveHandle,
    mcp: Option<McpHandle>,
    current_model: String,
    pending: PendingState,
    cwd: PathBuf,
) {
    start_event_pump(
        handle.event_rx.clone(),
        handle.session_id.clone(),
        srv.out_tx.clone(),
        Arc::clone(&pending),
        cwd,
        maki_storage::paths::home(),
    );
    srv.session = Some(SessionState {
        handle,
        mcp,
        current_mode: AgentMode::Build,
        current_model,
        pending,
    });
}

fn load_history(session_id: MakiId) -> Result<(Vec<Message>, Option<PathBuf>), AcpError> {
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
) -> Result<(Vec<Message>, Option<PathBuf>), AcpError> {
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
    Ok((session.take_messages(), recorded))
}

fn handle_prompt(srv: &mut Server, raw: &Value, id: &RequestId) -> Result<(), AcpError> {
    let req: PromptRequest = parse_params(raw)?;
    let session = srv.session.as_ref().ok_or_else(no_session)?;

    let (message, images) = extract_prompt_content(&req.prompt);
    let input = AgentInput {
        message,
        mode: session.current_mode.clone(),
        images,
        preamble: Vec::new(),
        thinking: Default::default(),
        fast: false,
        workflow: false,
        prompt: None,
    };

    session
        .handle
        .input_tx
        .send(input)
        .map_err(|_| AcpError::new(-32603, "session ended"))?;
    session.pending.lock().unwrap().prompt = Some(id.clone());
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
                // Any answer still in flight belongs to the cancelled turn, so
                // forget its id and let it be dropped on arrival.
                session.pending.lock().unwrap().ask = None;
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
    let ask = session
        .pending
        .lock()
        .unwrap()
        .ask
        .take_if(|(ask_id, _)| *ask_id == id);
    let Some((_, kind)) = ask else {
        warn!(id, "response for an unknown request id");
        return;
    };
    let answer = match kind {
        AskKind::Permission => permission_answer(raw).encode(),
        // The waiting question tool parses this; an error response decodes to
        // nothing and counts as a dismissal.
        AskKind::Elicitation => raw
            .get("result")
            .cloned()
            .unwrap_or(Value::Null)
            .to_string(),
    };
    let _ = session.handle.answer_tx.send(answer);
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

fn start_event_pump(
    event_rx: Receiver<Envelope>,
    session_id: SessionRef,
    out_tx: Sender<Value>,
    pending: PendingState,
    cwd: PathBuf,
    home: Option<PathBuf>,
) {
    smol::spawn(async move {
        let sid = SessionId::from(session_id.to_string());

        while let Ok(Envelope {
            event, subagent, ..
        }) = event_rx.recv_async().await
        {
            if subagent.is_some() {
                continue;
            }

            let update = match event {
                AgentEvent::TextDelta { text } => translate::text_delta(&text),
                AgentEvent::ThinkingDelta { text } => translate::thinking_delta(&text),
                AgentEvent::ToolPending { id, name } => translate::tool_pending(&id, &name),
                AgentEvent::ToolStart(event) => {
                    translate::tool_start(&event, &cwd, home.as_deref())
                }
                AgentEvent::ToolOutput { id, content } => translate::tool_output(&id, &content),
                AgentEvent::ToolDone(event) => translate::tool_done(&event, &cwd, home.as_deref()),
                AgentEvent::PermissionRequest { id, tool, scopes } => {
                    let fields =
                        ToolCallUpdateFields::new().title(format!("{tool}: {}", scopes.join(", ")));
                    let request =
                        AgentRequest::RequestPermissionRequest(RequestPermissionRequest::new(
                            sid.clone(),
                            ToolCallUpdate::new(ToolCallId::from(id), fields),
                            permissions::permission_options(),
                        ));
                    ask_client(&out_tx, &pending, AskKind::Permission, request);
                    continue;
                }
                AgentEvent::Done { reason, .. } => {
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
    .detach();
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
    use maki_providers::{ContentBlock as MsgBlock, Role, TokenUsage};
    use maki_storage::StateDir;
    use maki_storage::sessions::Session;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const ANSWERED_ID: i64 = 1001;
    const UNKNOWN_ID: i64 = 1002;

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

    fn server_awaiting_answer() -> (Server, Receiver<String>) {
        server_with_ask(AskKind::Permission)
    }

    fn server_with_ask(kind: AskKind) -> (Server, Receiver<String>) {
        let (answer_tx, answer_rx) = flume::unbounded();
        let handle = InteractiveHandle {
            event_rx: flume::unbounded().1,
            tool_names: Vec::new(),
            input_tx: flume::unbounded().0,
            answer_tx,
            cancel_tx: flume::unbounded().0,
            model_tx: flume::unbounded().0,
            session_id: SessionRef::from(MakiId::generate()),
            permissions: Arc::new(PermissionManager::new(
                maki_config::PermissionsConfig::default(),
                PathBuf::from("/project"),
                Arc::default(),
            )),
            task: smol::spawn(async {}),
        };
        let server = Server {
            out_tx: flume::unbounded().0,
            model_specs: Vec::new(),
            model_policy: Arc::new(maki_config::ModelPolicy::default()),
            client_elicits_form: false,
            session: Some(SessionState {
                handle,
                mcp: None,
                current_mode: AgentMode::Build,
                current_model: String::new(),
                pending: Arc::new(Mutex::new(Pending {
                    prompt: None,
                    ask: Some((ANSWERED_ID, kind)),
                })),
            }),
        };
        (server, answer_rx)
    }

    #[test]
    fn only_the_outstanding_request_id_is_answered() {
        let (srv, answer_rx) = server_awaiting_answer();

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
    fn cancel_drops_the_outstanding_permission_request() {
        let (srv, answer_rx) = server_awaiting_answer();
        handle_notification(&srv, "session/cancel");

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert!(answer_rx.is_empty(), "the cancelled turn owns that answer");
    }

    #[test]
    fn elicitation_response_forwards_the_raw_result() {
        let (srv, answer_rx) = server_with_ask(AskKind::Elicitation);
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
        session.save(&dir).unwrap();

        let id: MakiId = session.id;
        let (history, recorded) = load_history_from(&dir, id).unwrap();
        assert_eq!(
            serde_json::to_value(&history).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
        assert_eq!(recorded, Some(PathBuf::from("/project")));
    }

    #[test]
    fn load_history_records_absolute_cwd_only() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<Message, TokenUsage, maki_agent::ToolOutput> =
            Session::new("anthropic/test-model", "relative/project");
        session.save(&dir).unwrap();
        let (_, recorded) = load_history_from(&dir, session.id).unwrap();
        assert_eq!(recorded, None);
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
