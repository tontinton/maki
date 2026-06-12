//! SDK streaming mode: `maki --print --input-format stream-json`.
//!
//! Wire protocol matches Claude Code's SDK interface so tools like Conductor, Windsurf, and custom
//! orchestrators work without adaptation.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::mem;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use flume::{Receiver, Sender};
use maki_agent::headless::{self, InteractiveHandle, InteractiveParams};
use maki_agent::mcp;
use maki_agent::permissions::PermissionAnswer;
use maki_agent::prompt::ResolvedSlots;
use maki_agent::tools::QUESTION_TOOL_NAME;
use maki_agent::{
    AgentConfig, AgentEvent, AgentInput, AgentMode, Envelope, PermissionsConfig, ToolOutput,
};
use maki_providers::model::Model;
use maki_providers::{Message, StopReason, Timeouts, TokenUsage};
use maki_storage::StateDir;
use maki_storage::sessions::Session;
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

use crate::cli::Cli;

const TOOL_NAME_MAP: &[(&str, &str)] = &[
    ("bash", "Bash"),
    ("read", "Read"),
    ("edit", "Edit"),
    ("write", "Write"),
    ("grep", "Grep"),
    ("glob", "Glob"),
    ("todo_write", "TodoWrite"),
    ("webfetch", "WebFetch"),
    ("websearch", "WebSearch"),
    ("task", "Task"),
    ("multiedit", "MultiEdit"),
    ("code_execution", "CodeExecution"),
    ("index", "Index"),
    ("memory", "Memory"),
    ("question", "Question"),
    ("skill", "Skill"),
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
}

impl PermissionMode {
    fn resolve(flag: Option<&str>, yolo: bool) -> Self {
        match flag {
            Some(s) => Self::parse(s).unwrap_or_else(|| {
                eprintln!("warning: unknown permission mode '{s}', using default");
                Self::Default
            }),
            None if yolo => Self::BypassPermissions,
            None => Self::Default,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "acceptEdits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    fn agent_mode(self, cwd: &Path) -> AgentMode {
        match self {
            Self::Plan => AgentMode::Plan(cwd.join("plan.md")),
            _ => AgentMode::Build,
        }
    }
}

#[derive(Serialize)]
struct WireMessage {
    #[serde(flatten)]
    inner: WireInner,
    session_id: String,
    uuid: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireInner {
    System(SystemPayload),
    Assistant(AssistantPayload),
    User(UserPayload),
    Result(ResultPayload),
    StreamEvent(StreamEventPayload),
    ControlResponse(ControlResponsePayload),
    ControlRequest(ControlRequestPayload),
}

#[derive(Serialize)]
struct SystemPayload {
    subtype: &'static str,
    #[serde(flatten)]
    extra: Value,
}

#[derive(Serialize)]
struct AssistantPayload {
    message: AssistantMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<String>,
}

#[derive(Serialize)]
struct AssistantMessage {
    id: String,
    model: String,
    role: &'static str,
    content: Value,
    stop_reason: Option<StopReason>,
    usage: TokenUsage,
}

#[derive(Serialize)]
struct UserPayload {
    message: UserMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<String>,
}

#[derive(Serialize)]
struct UserMessage {
    role: &'static str,
    content: Value,
}

#[derive(Serialize)]
struct ResultPayload {
    subtype: &'static str,
    is_error: bool,
    duration_ms: u128,
    duration_api_ms: u128,
    num_turns: u32,
    result: String,
    total_cost_usd: f64,
    usage: TokenUsage,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    permission_denials: Vec<Value>,
}

#[derive(Serialize)]
struct StreamEventPayload {
    event: Value,
}

#[derive(Serialize)]
struct ControlResponsePayload {
    response: ControlResponseInner,
}

#[derive(Serialize)]
struct ControlResponseInner {
    subtype: &'static str,
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct ControlRequestPayload {
    request_id: String,
    request: ControlRequestInner,
}

#[derive(Serialize)]
struct ControlRequestInner {
    subtype: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct InboundMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(flatten)]
    payload: Value,
}

#[derive(serde::Deserialize)]
struct InboundUser {
    message: InboundUserMessage,
}

#[derive(serde::Deserialize)]
struct InboundUserMessage {
    content: Value,
}

#[derive(serde::Deserialize)]
struct InboundControlRequest {
    request_id: String,
    request: InboundControlRequestInner,
}

#[derive(serde::Deserialize)]
struct InboundControlRequestInner {
    subtype: String,
    #[serde(flatten)]
    extra: Value,
}

#[derive(serde::Deserialize)]
struct InboundControlResponse {
    response: Value,
}

#[derive(serde::Deserialize)]
struct InboundControlCancelRequest {
    request_id: String,
}

// StreamSynth owns all Anthropic stream state. Each method returns every wire
// event the transition needs (closing the old block, opening a new message, ...)
// so callers never have to track block lifecycle themselves.

#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
}

struct StreamSynth {
    block_index: i32,
    started: bool,
    current_block: Option<BlockKind>,
}

impl StreamSynth {
    fn new() -> Self {
        Self {
            block_index: -1,
            started: false,
            current_block: None,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn text_delta(&mut self, model: &str, text: &str) -> Vec<Value> {
        let mut events = self.ensure_block(model, BlockKind::Text);
        events.push(serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "text_delta", "text": text}
        }));
        events
    }

    fn thinking_delta(&mut self, model: &str, text: &str) -> Vec<Value> {
        let mut events = self.ensure_block(model, BlockKind::Thinking);
        events.push(serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "thinking_delta", "thinking": text}
        }));
        events
    }

    fn tool_use(&mut self, model: &str, id: &str, name: &str, input_json: &str) -> Vec<Value> {
        let mut events = self.ensure_started(model);
        events.extend(self.close_block());
        self.block_index += 1;
        events.push(serde_json::json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
        }));
        events.push(serde_json::json!({
            "type": "content_block_delta",
            "index": self.block_index,
            "delta": {"type": "input_json_delta", "partial_json": input_json}
        }));
        events.push(self.block_stop());
        events
    }

    fn finish_message(&mut self, usage: &TokenUsage) -> Vec<Value> {
        if !self.started {
            return Vec::new();
        }
        let mut events: Vec<Value> = self.close_block().into_iter().collect();
        events.push(serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": null},
            "usage": {"output_tokens": usage.output}
        }));
        events.push(serde_json::json!({"type": "message_stop"}));
        self.reset();
        events
    }

    fn ensure_started(&mut self, model: &str) -> Vec<Value> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![serde_json::json!({
            "type": "message_start",
            "message": {
                "id": uuid::Uuid::new_v4().to_string(),
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        })]
    }

    fn ensure_block(&mut self, model: &str, kind: BlockKind) -> Vec<Value> {
        let mut events = self.ensure_started(model);
        if self.current_block == Some(kind) {
            return events;
        }
        events.extend(self.close_block());
        self.block_index += 1;
        self.current_block = Some(kind);
        let content_block = match kind {
            BlockKind::Text => serde_json::json!({"type": "text", "text": ""}),
            BlockKind::Thinking => serde_json::json!({"type": "thinking", "thinking": ""}),
        };
        events.push(serde_json::json!({
            "type": "content_block_start",
            "index": self.block_index,
            "content_block": content_block
        }));
        events
    }

    fn close_block(&mut self) -> Option<Value> {
        self.current_block.take().map(|_| self.block_stop())
    }

    fn block_stop(&self) -> Value {
        serde_json::json!({
            "type": "content_block_stop",
            "index": self.block_index,
        })
    }
}

fn maki_to_claude_tool_name(name: &str) -> &str {
    TOOL_NAME_MAP
        .iter()
        .find(|(m, _)| *m == name)
        .map(|(_, c)| *c)
        .unwrap_or(name)
}

#[derive(Clone)]
struct SdkWriter {
    session_id: String,
    out_tx: Sender<String>,
}

impl SdkWriter {
    fn emit(&self, inner: WireInner) -> Result<()> {
        let msg = WireMessage {
            inner,
            session_id: self.session_id.clone(),
            uuid: uuid::Uuid::new_v4().to_string(),
        };
        self.out_tx
            .send(serde_json::to_string(&msg)?)
            .map_err(|_| eyre!("stdout writer closed"))
    }

    fn emit_system(&self, subtype: &'static str, extra: Value) -> Result<()> {
        self.emit(WireInner::System(SystemPayload { subtype, extra }))
    }

    fn emit_control_response(
        &self,
        request_id: &str,
        response: Option<Value>,
        error: Option<String>,
    ) -> Result<()> {
        self.emit(WireInner::ControlResponse(ControlResponsePayload {
            response: ControlResponseInner {
                subtype: if error.is_some() { "error" } else { "success" },
                request_id: request_id.into(),
                response,
                error,
            },
        }))
    }
}

pub struct SdkParams {
    pub cli: Cli,
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt_slots: ResolvedSlots,
    pub fast: bool,
}

struct Shared {
    model: Model,
    permission_mode: PermissionMode,
    turn_start: Instant,
    pending: HashSet<String>,
}

pub fn run(params: SdkParams) -> Result<()> {
    let SdkParams {
        cli,
        model,
        mut config,
        permissions_config,
        timeouts,
        prompt_slots,
        fast,
    } = params;
    cli.warn_ignored_flags();
    if let Some(max) = cli.max_turns {
        config.max_turns = Some(max);
    }
    let permission_mode = PermissionMode::resolve(cli.permission_mode.as_deref(), cli.yolo);

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let working_dir = cwd.to_string_lossy().into_owned();
    let (session_id, initial_history) = resolve_session(&cli, &working_dir)?;

    let (mcp_handle, mcp_config_errors) = smol::block_on(mcp::start(&cwd));
    if !mcp_config_errors.is_empty() {
        eprintln!("MCP config error: {mcp_config_errors}");
    }

    let startup_model = model.clone();
    let handle = headless::spawn_interactive(InteractiveParams {
        model,
        config,
        permissions_config,
        timeouts,
        prompt_slots: Arc::new(prompt_slots),
        excluded_tools: vec![QUESTION_TOOL_NAME],
        mcp_handle,
        initial_wd: cwd.clone(),
        session_id,
        initial_history,
        yolo: permission_mode == PermissionMode::BypassPermissions,
        system_prompt_override: cli.system_prompt.clone().filter(|s| !s.is_empty()),
        append_system_prompt: cli.append_system_prompt.clone().filter(|s| !s.is_empty()),
    });

    let (out_tx, out_rx) = flume::unbounded::<String>();
    let writer_thread = std::thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        while let Ok(line) = out_rx.recv() {
            if writeln!(stdout, "{line}").and(stdout.flush()).is_err() {
                break;
            }
        }
    });

    let writer = SdkWriter {
        session_id: handle.session_id.clone(),
        out_tx,
    };
    let tools: Vec<&str> = handle
        .tool_names
        .iter()
        .map(|t| maki_to_claude_tool_name(t))
        .collect();
    writer.emit_system(
        "init",
        serde_json::json!({
            "cwd": working_dir,
            "tools": tools,
            "model": startup_model.id,
            "permissionMode": permission_mode.as_str(),
            "apiKeySource": "none",
            "mcp_servers": [],
            "slash_commands": [],
            "output_style": "default",
        }),
    )?;

    let shared = Arc::new(Mutex::new(Shared {
        model: startup_model.clone(),
        permission_mode,
        turn_start: Instant::now(),
        pending: HashSet::new(),
    }));

    let pump = EventPump {
        writer: writer.clone(),
        shared: Arc::clone(&shared),
        answer_tx: handle.answer_tx.clone(),
        include_partial_messages: cli.include_partial_messages,
        fast,
        synth: StreamSynth::new(),
        tool_inputs: HashMap::new(),
        result_text: String::new(),
        request_counter: 0,
    }
    .spawn(handle.event_rx.clone());

    for line in io::stdin().lock().lines() {
        let line = line.context("read stdin")?;
        if line.is_empty() {
            continue;
        }

        let msg: InboundMessage = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("warning: ignoring malformed input line: {e}");
                continue;
            }
        };

        match msg.msg_type.as_str() {
            "user" => {
                let Some(user) = parse_or_warn::<InboundUser>(msg.payload, "user message") else {
                    continue;
                };
                let content = user.message.content;
                let prompt = content_text(&content).unwrap_or_else(|| content.to_string());
                let mode = {
                    let mut shared = shared.lock().unwrap();
                    shared.turn_start = Instant::now();
                    shared.permission_mode
                };
                let input = AgentInput {
                    message: prompt,
                    mode: mode.agent_mode(&cwd),
                    fast,
                    ..Default::default()
                };
                if handle.input_tx.send(input).is_err() {
                    break;
                }
            }
            "control_request" => {
                let Some(cr) =
                    parse_or_warn::<InboundControlRequest>(msg.payload, "control_request")
                else {
                    continue;
                };
                handle_control_request(&cr, &writer, &handle, &shared, &startup_model)?;
            }
            "control_response" => {
                let Some(cr) =
                    parse_or_warn::<InboundControlResponse>(msg.payload, "control_response")
                else {
                    continue;
                };
                let data = cr.response;
                if let Some(req_id) = data.get("request_id").and_then(Value::as_str)
                    && shared.lock().unwrap().pending.remove(req_id)
                {
                    let _ = handle
                        .answer_tx
                        .send(decode_permission_response(&data).encode());
                }
            }
            "control_cancel_request" => {
                let Some(ccr) = parse_or_warn::<InboundControlCancelRequest>(
                    msg.payload,
                    "control_cancel_request",
                ) else {
                    continue;
                };
                if shared.lock().unwrap().pending.remove(&ccr.request_id) {
                    let _ = handle.answer_tx.send(PermissionAnswer::Deny.encode());
                }
            }
            other => warn!("unknown inbound message type: {other}"),
        }
    }

    let InteractiveHandle { input_tx, task, .. } = handle;
    drop(input_tx);
    smol::block_on(async {
        task.await;
        pump.await;
    });
    drop(writer);
    let _ = writer_thread.join();
    Ok(())
}

type StoredSession = Session<Message, TokenUsage, ToolOutput>;

fn resolve_session(cli: &Cli, cwd: &str) -> Result<(Option<String>, Vec<Message>)> {
    let (resumed_id, history) = if let Some(id) = &cli.session {
        let storage = StateDir::resolve().context("resolve state dir")?;
        let session =
            StoredSession::load(id, &storage).map_err(|e| eyre!("load session {id}: {e}"))?;
        let resumed = (!cli.fork_session).then_some(session.id);
        (resumed, session.messages)
    } else if cli.continue_session {
        let storage = StateDir::resolve().context("resolve state dir")?;
        match StoredSession::latest(cwd, &storage) {
            Ok(Some(session)) => (Some(session.id), session.messages),
            _ => (None, Vec::new()),
        }
    } else {
        (None, Vec::new())
    };

    Ok((cli.session_id.clone().or(resumed_id), history))
}

fn parse_or_warn<T: serde::de::DeserializeOwned>(payload: Value, what: &str) -> Option<T> {
    match serde_json::from_value(payload) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("warning: ignoring malformed {what}: {e}");
            None
        }
    }
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|b| {
                    (b.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| b.get("text").and_then(Value::as_str))
                        .flatten()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

fn handle_control_request(
    cr: &InboundControlRequest,
    writer: &SdkWriter,
    handle: &InteractiveHandle,
    shared: &Mutex<Shared>,
    startup_model: &Model,
) -> Result<()> {
    let ok = Some(Value::Object(Default::default()));
    match cr.request.subtype.as_str() {
        "initialize" => {
            if let Some(extra) = cr.request.extra.as_object()
                && (extra.contains_key("hooks") || extra.contains_key("agents"))
            {
                eprintln!("note: hooks/agents payloads are ignored");
            }
            writer.emit_control_response(
                &cr.request_id,
                Some(serde_json::json!({"commands": []})),
                None,
            )
        }
        "interrupt" => {
            let _ = handle.cancel_tx.try_send(());
            writer.emit_control_response(&cr.request_id, ok, None)
        }
        "set_permission_mode" => {
            let mode_str = cr.request.extra.get("mode").and_then(Value::as_str);
            match mode_str.and_then(PermissionMode::parse) {
                Some(mode) => {
                    shared.lock().unwrap().permission_mode = mode;
                    writer.emit_control_response(&cr.request_id, ok, None)
                }
                None => writer.emit_control_response(
                    &cr.request_id,
                    None,
                    Some(format!(
                        "invalid permission mode: {}",
                        mode_str.unwrap_or("<missing>")
                    )),
                ),
            }
        }
        "set_model" => {
            if let Some(model) = resolve_set_model(cr.request.extra.get("model"), startup_model) {
                let _ = handle.model_tx.send(model.clone());
                shared.lock().unwrap().model = model;
            }
            writer.emit_control_response(&cr.request_id, ok, None)
        }
        other => writer.emit_control_response(
            &cr.request_id,
            None,
            Some(format!("unsupported: {other}")),
        ),
    }
}

fn resolve_set_model(model_val: Option<&Value>, startup_model: &Model) -> Option<Model> {
    match model_val? {
        Value::Null => Some(startup_model.clone()),
        Value::String(model_str) => match Model::from_spec(&resolve_model_spec(model_str)) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!(
                    "warning: failed to resolve model '{model_str}': {e}, keeping current model"
                );
                None
            }
        },
        _ => None,
    }
}

fn resolve_model_spec(model_id: &str) -> String {
    if model_id.contains('/') {
        return model_id.to_string();
    }
    if model_id.starts_with("claude-") {
        return format!("anthropic/{model_id}");
    }
    model_id.to_string()
}

fn decode_permission_response(data: &Value) -> PermissionAnswer {
    match data.get("behavior").and_then(Value::as_str) {
        Some("allow") if data.get("updatedPermissions").is_some() => PermissionAnswer::AllowSession,
        Some("allow") => PermissionAnswer::AllowOnce,
        Some("deny") => match data.get("message").and_then(Value::as_str) {
            Some(msg) if !msg.is_empty() => PermissionAnswer::DenyWithGuidance(msg.to_string()),
            _ => PermissionAnswer::Deny,
        },
        _ => PermissionAnswer::Deny,
    }
}

struct EventPump {
    writer: SdkWriter,
    shared: Arc<Mutex<Shared>>,
    answer_tx: Sender<String>,
    include_partial_messages: bool,
    fast: bool,
    synth: StreamSynth,
    tool_inputs: HashMap<String, (String, Value)>,
    result_text: String,
    request_counter: u64,
}

impl EventPump {
    fn spawn(mut self, event_rx: Receiver<Envelope>) -> smol::Task<()> {
        smol::spawn(async move {
            while let Ok(envelope) = event_rx.recv_async().await {
                if let Err(e) = self.handle(envelope) {
                    warn!(error = %e, "sdk event pump stopped");
                    break;
                }
            }
        })
    }

    fn model_id(&self) -> String {
        self.shared.lock().unwrap().model.id.clone()
    }

    fn emit_stream(&self, events: Vec<Value>) -> Result<()> {
        events.into_iter().try_for_each(|event| {
            self.writer
                .emit(WireInner::StreamEvent(StreamEventPayload { event }))
        })
    }

    fn reset_turn(&mut self) {
        self.synth.reset();
        self.tool_inputs.clear();
        self.result_text.clear();
        self.shared.lock().unwrap().pending.clear();
    }

    fn emit_turn_result(
        &mut self,
        is_error: bool,
        result: String,
        num_turns: u32,
        usage: TokenUsage,
    ) -> Result<()> {
        let (duration_ms, total_cost_usd) = {
            let shared = self.shared.lock().unwrap();
            (
                shared.turn_start.elapsed().as_millis(),
                usage.cost(&shared.model.pricing, self.fast),
            )
        };
        self.writer.emit(WireInner::Result(ResultPayload {
            subtype: if is_error {
                "error_during_execution"
            } else {
                "success"
            },
            is_error,
            duration_ms,
            duration_api_ms: duration_ms,
            num_turns,
            result,
            total_cost_usd,
            usage,
            permission_denials: Vec::new(),
        }))?;
        self.reset_turn();
        Ok(())
    }

    fn handle(&mut self, envelope: Envelope) -> Result<()> {
        let parent_tool_use_id = envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.clone());

        match &envelope.event {
            AgentEvent::TextDelta { text } => {
                if self.include_partial_messages {
                    let model = self.model_id();
                    let events = self.synth.text_delta(&model, text);
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ThinkingDelta { text } => {
                if self.include_partial_messages {
                    let model = self.model_id();
                    let events = self.synth.thinking_delta(&model, text);
                    self.emit_stream(events)?;
                }
            }
            AgentEvent::ToolStart(ts) => {
                let name = ts.tool.to_string();
                let input = ts.raw_input.clone().unwrap_or(Value::Null);

                if self.include_partial_messages {
                    let model = self.model_id();
                    let events = self.synth.tool_use(
                        &model,
                        &ts.id,
                        maki_to_claude_tool_name(&name),
                        &serde_json::to_string(&input)?,
                    );
                    self.emit_stream(events)?;
                }
                self.tool_inputs.insert(ts.id.clone(), (name, input));
            }
            AgentEvent::ToolPending { .. }
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolDone(_)
            | AgentEvent::BatchProgress(_)
            | AgentEvent::QueueItemConsumed { .. }
            | AgentEvent::AutoCompacting
            | AgentEvent::AuthRequired
            | AgentEvent::SubagentHistory { .. }
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. } => {}
            AgentEvent::Retry {
                attempt,
                message,
                delay_ms,
            } => {
                self.writer.emit_system(
                    "api_retry",
                    serde_json::json!({
                        "attempt": attempt,
                        "retry_delay_ms": delay_ms,
                        "error": message,
                    }),
                )?;
            }
            AgentEvent::TurnComplete(tc) => {
                if self.include_partial_messages {
                    let events = self.synth.finish_message(&tc.usage);
                    self.emit_stream(events)?;
                }

                let content_value = serde_json::to_value(&tc.message.content)?;
                if parent_tool_use_id.is_none() {
                    self.result_text = content_text(&content_value).unwrap_or_default();
                }
                self.writer.emit(WireInner::Assistant(AssistantPayload {
                    message: AssistantMessage {
                        id: uuid::Uuid::new_v4().to_string(),
                        model: tc.model.clone(),
                        role: "assistant",
                        content: map_tool_names_in_content(&content_value),
                        stop_reason: None,
                        usage: tc.usage,
                    },
                    parent_tool_use_id,
                }))?;
            }
            AgentEvent::ToolResultsSubmitted { message } => {
                self.writer.emit(WireInner::User(UserPayload {
                    message: UserMessage {
                        role: "user",
                        content: serde_json::to_value(&message.content)?,
                    },
                    parent_tool_use_id,
                }))?;
            }
            AgentEvent::PermissionRequest { id, tool, .. } => {
                if self.shared.lock().unwrap().permission_mode == PermissionMode::BypassPermissions
                {
                    let _ = self.answer_tx.send(PermissionAnswer::AllowSession.encode());
                    return Ok(());
                }

                let (tool_name, input) = self
                    .tool_inputs
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| (tool.clone(), Value::Null));

                self.request_counter += 1;
                let req_id = format!("req_{}", self.request_counter);
                self.shared.lock().unwrap().pending.insert(req_id.clone());

                self.writer
                    .emit(WireInner::ControlRequest(ControlRequestPayload {
                        request_id: req_id,
                        request: ControlRequestInner {
                            subtype: "can_use_tool",
                            tool_name: Some(maki_to_claude_tool_name(&tool_name).into()),
                            input: Some(input),
                            tool_use_id: Some(id.clone()),
                        },
                    }))?;
            }
            AgentEvent::Done {
                usage,
                num_turns,
                stop_reason: _,
            } => {
                let result = mem::take(&mut self.result_text);
                self.emit_turn_result(false, result, *num_turns, *usage)?;
            }
            AgentEvent::Error { message } => {
                self.emit_turn_result(true, message.clone(), 0, TokenUsage::default())?;
            }
        }
        Ok(())
    }
}

fn map_tool_names_in_content(content: &Value) -> Value {
    match content {
        Value::Array(blocks) => {
            let mapped: Vec<Value> = blocks
                .iter()
                .map(|block| {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && let Some(name) = block.get("name").and_then(Value::as_str)
                    {
                        let mut b = block.clone();
                        b["name"] = Value::String(maki_to_claude_tool_name(name).to_string());
                        return b;
                    }
                    block.clone()
                })
                .collect();
            Value::Array(mapped)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn claude_to_maki_tool_name(name: &str) -> &str {
        TOOL_NAME_MAP
            .iter()
            .find(|(_, c)| *c == name)
            .map(|(m, _)| *m)
            .unwrap_or(name)
    }

    #[test_case("bash", "Bash")]
    #[test_case("read", "Read")]
    #[test_case("edit", "Edit")]
    #[test_case("write", "Write")]
    #[test_case("grep", "Grep")]
    #[test_case("glob", "Glob")]
    #[test_case("todo_write", "TodoWrite")]
    #[test_case("webfetch", "WebFetch")]
    #[test_case("websearch", "WebSearch")]
    #[test_case("task", "Task")]
    #[test_case("multiedit", "MultiEdit")]
    #[test_case("code_execution", "CodeExecution")]
    #[test_case("index", "Index")]
    #[test_case("memory", "Memory")]
    #[test_case("question", "Question")]
    fn maki_to_claude_roundtrip(maki: &str, claude: &str) {
        assert_eq!(maki_to_claude_tool_name(maki), claude);
        assert_eq!(claude_to_maki_tool_name(claude), maki);
    }

    #[test]
    fn unknown_tool_name_passthrough() {
        assert_eq!(maki_to_claude_tool_name("unknown_tool"), "unknown_tool");
        assert_eq!(claude_to_maki_tool_name("UnknownTool"), "UnknownTool");
    }

    const MODEL: &str = "test-model";

    fn types(events: &[Value]) -> Vec<&str> {
        events.iter().map(|e| e["type"].as_str().unwrap()).collect()
    }

    #[test]
    fn text_delta_starts_message_and_subsequent_is_delta_only() {
        let mut synth = StreamSynth::new();
        let events = synth.text_delta(MODEL, "hi");
        assert_eq!(
            types(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert_eq!(events[0]["message"]["model"], MODEL);
        assert_eq!(events[1]["index"], 0);
        assert_eq!(events[1]["content_block"]["type"], "text");
        assert_eq!(events[2]["delta"]["text"], "hi");

        let more = synth.text_delta(MODEL, "again");
        assert_eq!(types(&more), ["content_block_delta"]);
    }

    #[test]
    fn block_transition_closes_previous_and_increments_index() {
        let mut synth = StreamSynth::new();
        synth.text_delta(MODEL, "a");
        let events = synth.thinking_delta(MODEL, "b");
        assert_eq!(
            types(&events),
            [
                "content_block_stop",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert_eq!(events[0]["index"], 0);
        assert_eq!(events[1]["index"], 1);
        assert_eq!(events[1]["content_block"]["type"], "thinking");
    }

    #[test]
    fn tool_use_emits_complete_block() {
        let mut synth = StreamSynth::new();
        synth.text_delta(MODEL, "a");
        let events = synth.tool_use(MODEL, "tool_1", "Read", r#"{"path":"t"}"#);
        assert_eq!(
            types(&events),
            [
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop"
            ]
        );
        assert_eq!(events[1]["content_block"]["type"], "tool_use");
        assert_eq!(events[1]["content_block"]["name"], "Read");
        assert_eq!(events[2]["delta"]["type"], "input_json_delta");
    }

    #[test]
    fn multiple_tool_uses_increment_block_index() {
        let mut synth = StreamSynth::new();
        synth.text_delta(MODEL, "x");
        let t1 = synth.tool_use(MODEL, "t1", "Read", "{}");
        let t2 = synth.tool_use(MODEL, "t2", "Write", "{}");
        let idx = |events: &[Value]| {
            events
                .iter()
                .find(|e| e["type"] == "content_block_start")
                .unwrap()["index"]
                .as_i64()
        };
        assert_eq!(idx(&t1), Some(1));
        assert_eq!(idx(&t2), Some(2));
    }

    #[test]
    fn finish_message_closes_block_and_resets() {
        let mut synth = StreamSynth::new();
        synth.text_delta(MODEL, "a");
        let usage = TokenUsage {
            output: 5,
            ..Default::default()
        };
        let events = synth.finish_message(&usage);
        assert_eq!(
            types(&events),
            ["content_block_stop", "message_delta", "message_stop"]
        );
        assert_eq!(events[1]["usage"]["output_tokens"], 5);

        assert!(synth.finish_message(&usage).is_empty());

        let next = synth.text_delta(MODEL, "new");
        assert_eq!(next[1]["index"], 0);
    }

    #[test]
    fn finish_message_before_start_is_empty() {
        let mut synth = StreamSynth::new();
        assert!(synth.finish_message(&TokenUsage::default()).is_empty());
    }

    #[test]
    fn tool_use_on_fresh_synth_has_no_spurious_stop() {
        let mut synth = StreamSynth::new();
        let events = synth.tool_use(MODEL, "t1", "Read", r#"{"path":"x"}"#);
        assert_eq!(events[0]["type"], "message_start");
        let start_pos = events
            .iter()
            .position(|e| e["type"] == "content_block_start")
            .unwrap();
        let stop_pos = events
            .iter()
            .position(|e| e["type"] == "content_block_stop")
            .unwrap();
        assert!(stop_pos > start_pos);
    }

    #[test_case("default", PermissionMode::Default)]
    #[test_case("acceptEdits", PermissionMode::AcceptEdits)]
    #[test_case("plan", PermissionMode::Plan)]
    #[test_case("bypassPermissions", PermissionMode::BypassPermissions)]
    fn permission_mode_roundtrip(s: &str, mode: PermissionMode) {
        assert_eq!(PermissionMode::parse(s), Some(mode));
        assert_eq!(mode.as_str(), s);
    }

    #[test]
    fn permission_mode_resolve() {
        assert_eq!(
            PermissionMode::resolve(None, false),
            PermissionMode::Default
        );
        assert_eq!(
            PermissionMode::resolve(None, true),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            PermissionMode::resolve(Some("plan"), true),
            PermissionMode::Plan
        );
        assert_eq!(
            PermissionMode::resolve(Some("bogus"), false),
            PermissionMode::Default
        );
    }

    #[test]
    fn content_text_extracts_from_all_shapes() {
        assert_eq!(content_text(&serde_json::json!("hi")), Some("hi".into()));
        let blocks = serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "image", "source": {}},
            {"type": "text", "text": "b"},
        ]);
        assert_eq!(content_text(&blocks), Some("a\nb".into()));
        assert_eq!(content_text(&serde_json::json!(42)), None);
    }

    #[test]
    fn wire_result_serializes_correctly() {
        let msg = WireMessage {
            inner: WireInner::Result(ResultPayload {
                subtype: "success",
                is_error: false,
                duration_ms: 1000,
                duration_api_ms: 1000,
                num_turns: 1,
                result: "done".into(),
                total_cost_usd: 0.01,
                usage: TokenUsage::default(),
                permission_denials: Vec::new(),
            }),
            session_id: "s".into(),
            uuid: "u".into(),
        };
        let json: Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "result");
        assert_eq!(json["subtype"], "success");
        assert_eq!(json["num_turns"], 1);
        assert!(json.get("session_id").is_some());
    }

    #[test]
    fn wire_init_serializes_correctly() {
        let msg = WireMessage {
            inner: WireInner::System(SystemPayload {
                subtype: "init",
                extra: serde_json::json!({
                    "cwd": "/tmp",
                    "tools": ["Read"],
                    "model": "test",
                    "permissionMode": "default",
                }),
            }),
            session_id: "s".into(),
            uuid: "u".into(),
        };
        let json: Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "system");
        assert_eq!(json["subtype"], "init");
        assert_eq!(json["cwd"], "/tmp");
    }

    #[test]
    fn wire_control_response_serializes() {
        let msg = WireMessage {
            inner: WireInner::ControlResponse(ControlResponsePayload {
                response: ControlResponseInner {
                    subtype: "success",
                    request_id: "req_1".into(),
                    response: Some(serde_json::json!({"commands": []})),
                    error: None,
                },
            }),
            session_id: "s".into(),
            uuid: "u".into(),
        };
        let json: Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "control_response");
        assert_eq!(json["response"]["request_id"], "req_1");
    }

    #[test]
    fn wire_control_request_serializes() {
        let msg = WireMessage {
            inner: WireInner::ControlRequest(ControlRequestPayload {
                request_id: "req_5".into(),
                request: ControlRequestInner {
                    subtype: "can_use_tool",
                    tool_name: Some("Read".into()),
                    input: Some(serde_json::json!({"path": "/tmp"})),
                    tool_use_id: Some("tool_123".into()),
                },
            }),
            session_id: "s".into(),
            uuid: "u".into(),
        };
        let json: Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "control_request");
        assert_eq!(json["request"]["subtype"], "can_use_tool");
        assert_eq!(json["request"]["tool_name"], "Read");
    }

    #[test_case("claude-opus-4-6", "anthropic/claude-opus-4-6"; "claude_prefix")]
    #[test_case("openai/gpt-4", "openai/gpt-4"; "explicit_provider")]
    #[test_case("gpt-4o", "gpt-4o"; "unknown_passthrough")]
    fn resolve_model_spec_cases(input: &str, expected: &str) {
        assert_eq!(resolve_model_spec(input), expected);
    }

    #[test]
    fn decode_permission_response_variants() {
        assert!(matches!(
            decode_permission_response(&serde_json::json!({"behavior": "allow"})),
            PermissionAnswer::AllowOnce
        ));
        assert!(matches!(
            decode_permission_response(
                &serde_json::json!({"behavior": "allow", "updatedPermissions": []})
            ),
            PermissionAnswer::AllowSession
        ));
        assert!(matches!(
            decode_permission_response(&serde_json::json!({})),
            PermissionAnswer::Deny
        ));
        assert!(matches!(
            decode_permission_response(&serde_json::json!({"behavior": "something_else"})),
            PermissionAnswer::Deny
        ));
        match decode_permission_response(
            &serde_json::json!({"behavior": "deny", "message": "not now"}),
        ) {
            PermissionAnswer::DenyWithGuidance(msg) => assert_eq!(msg, "not now"),
            other => panic!("expected guidance, got {other:?}"),
        }
    }

    #[test]
    fn resolve_set_model_null_returns_startup() {
        let startup = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let result = resolve_set_model(Some(&Value::Null), &startup).unwrap();
        assert_eq!(result.id, startup.id);
    }

    #[test]
    fn map_tool_names_in_content_maps_known_and_preserves_rest() {
        let content = serde_json::json!([
            {"type": "text", "text": "hello"},
            {"type": "tool_use", "name": "read", "id": "1", "input": {}},
            {"type": "tool_use", "name": "unknown_native", "id": "2", "input": {}},
        ]);
        let mapped = map_tool_names_in_content(&content);
        assert_eq!(mapped[0]["type"], "text");
        assert_eq!(mapped[1]["name"], "Read");
        assert_eq!(mapped[2]["name"], "unknown_native");
    }
}
