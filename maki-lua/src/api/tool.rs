use std::borrow::Cow;
use std::cell::RefCell;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flume::Sender;
use maki_agent::prompt::{PromptId, Slot, SlotKind, ValidNames};
use maki_agent::tools::Tool;
use maki_agent::tools::registry::{RegisteredTool, ToolRegistry};
use maki_agent::tools::schema::{ParamSchema, to_json_schema, try_from_json, validate};
use maki_agent::tools::{
    BoxFuture, Deadline, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError,
    PermissionScopes, ToolAudience, ToolContext, ToolExecResult, ToolFilter, ToolInvocation,
    is_tool_enabled, timeout_annotation,
};
use maki_agent::{
    AgentEvent, BufferSnapshot, ImageMediaType, ImageSource, InstructionBlock, SharedBuf,
    TextOutput, ToolOutput,
};
use maki_config::ToolOutputLines;
use mlua::{
    Function, Lua, LuaSerdeExt, MultiValue, RegistryKey, Result as LuaResult, Table,
    Value as LuaValue,
};
use serde_json::{Value, json};

use crate::api::ui::buf::{BufHandle, line_to_lua};
use crate::api::util::command::{
    CommandEntry, CommandHandlerMap, LuaCommandWriter, publish_command_snapshot,
};
use crate::api::util::convert::{json_to_lua, lua_to_json};
use crate::api::util::ctx::LuaCtx;
use crate::runtime::{HintContent, LiveCtx, PromptHintCallbacks, PromptHintRegistration, Request};

const TOOL_NAME_MAX: usize = 64;
const TOOL_HANDLER_RETURN_ERR: &str =
    "tool handler must return string or {output=string, is_error?=bool}";
const TIMEOUT_PARSE_ERR: &str = "register_tool: 'timeout' must be a positive number, 0, or false";
const MAX_HINT_CONTENT_SIZE: usize = 1024 * 1024;
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(3);
const PLAIN_HEADER_STYLE: &str = "tool";

type DescribeFn = Box<dyn Fn(&str, &str, &Value) -> Option<String>>;

thread_local! {
    /// Lives on the Lua runtime thread only. Calling `Request::Describe` from
    /// that same thread would self-deadlock, so we resolve in-thread instead.
    static LOCAL_DESCRIBE: RefCell<Option<DescribeFn>> = const { RefCell::new(None) };
}

pub(crate) fn set_local_describe(f: impl Fn(&str, &str, &Value) -> Option<String> + 'static) {
    LOCAL_DESCRIBE.with(|c| *c.borrow_mut() = Some(Box::new(f)));
}

fn local_describe(plugin: &str, tool: &str, dctx: &Value) -> Option<Option<String>> {
    LOCAL_DESCRIBE.with(|c| c.borrow().as_ref().map(|f| f(plugin, tool, dctx)))
}

type ToolHandles = (Option<Function>, Option<Function>);
type ToolHandlesFn = Box<dyn Fn(&str) -> Option<ToolHandles>>;

thread_local! {
    static LOCAL_TOOL_HANDLES: RefCell<Option<ToolHandlesFn>> = const { RefCell::new(None) };
}

pub(crate) fn set_local_tool_handles(f: impl Fn(&str) -> Option<ToolHandles> + 'static) {
    LOCAL_TOOL_HANDLES.with(|c| *c.borrow_mut() = Some(Box::new(f)));
}

fn local_tool_handles(tool: &str) -> Option<ToolHandles> {
    LOCAL_TOOL_HANDLES.with(|c| c.borrow().as_ref().and_then(|f| f(tool)))
}

fn dctx_json(ctx: &DescriptionContext) -> Value {
    let mut obj = json!({
        "audience": ctx.audience.name().unwrap_or("main"),
        "workflow": ctx.workflow,
    });
    match ctx.filter {
        ToolFilter::All => {}
        ToolFilter::Only(names) => obj["only"] = json!(names),
        ToolFilter::AllExcept(names) => obj["except"] = json!(names),
    }
    obj
}

#[derive(Clone)]
pub(crate) enum StartAnnotation {
    Count(Arc<str>),
    Timeout(Arc<str>),
}

#[derive(Clone)]
pub(crate) enum PermissionScopeKind {
    Field(Arc<str>),
    Callback,
}

pub(crate) enum PermissionScopeSpec {
    Field(Arc<str>),
    Callback(RegistryKey),
}

impl PermissionScopeSpec {
    pub(crate) fn kind(&self) -> PermissionScopeKind {
        match self {
            Self::Field(f) => PermissionScopeKind::Field(Arc::clone(f)),
            Self::Callback(_) => PermissionScopeKind::Callback,
        }
    }
}

pub(crate) struct PendingTool {
    pub(crate) name: Arc<str>,
    pub(crate) description: String,
    pub(crate) schema: &'static ParamSchema,
    pub(crate) audience: ToolAudience,
    pub(crate) kind: Option<Arc<str>>,
    pub(crate) handler_key: RegistryKey,
    pub(crate) header_key: Option<RegistryKey>,
    pub(crate) restore_key: Option<RegistryKey>,
    pub(crate) start_key: Option<RegistryKey>,
    pub(crate) permission_scopes: Option<PermissionScopeSpec>,
    pub(crate) mutable_path_field: Option<Arc<str>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) start_annotation: Option<StartAnnotation>,
    pub(crate) examples: Option<Value>,
    pub(crate) describe_key: Option<RegistryKey>,
}

pub(crate) type PendingTools = Arc<Mutex<Vec<PendingTool>>>;

pub(crate) struct LuaTool {
    pub(crate) name: Arc<str>,
    pub(crate) description: String,
    pub(crate) schema: &'static ParamSchema,
    pub(crate) audience: ToolAudience,
    pub(crate) kind: Option<Arc<str>>,
    pub(crate) tx: Sender<Request>,
    pub(crate) plugin: Arc<str>,
    pub(crate) has_header_fn: bool,
    pub(crate) has_start_fn: bool,
    pub(crate) permission_scope_kind: Option<PermissionScopeKind>,
    pub(crate) mutable_path_field: Option<Arc<str>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) start_annotation: Option<StartAnnotation>,
    pub(crate) examples: Option<Value>,
    pub(crate) has_describe_fn: bool,
}

impl Tool for LuaTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self, ctx: &DescriptionContext) -> Cow<'_, str> {
        if !self.has_describe_fn {
            return Cow::Borrowed(&self.description);
        }
        let dctx = dctx_json(ctx);
        if let Some(result) = local_describe(&self.plugin, &self.name, &dctx) {
            return match result {
                Some(s) => Cow::Owned(s),
                None => Cow::Borrowed(&self.description),
            };
        }
        let (reply_tx, reply_rx) = flume::bounded(1);
        let sent = self
            .tx
            .send(Request::Describe {
                plugin: Arc::clone(&self.plugin),
                tool: Arc::clone(&self.name),
                dctx,
                reply: reply_tx,
            })
            .is_ok();
        if sent {
            match reply_rx.recv_timeout(DESCRIBE_TIMEOUT) {
                Ok(Some(s)) => return Cow::Owned(s),
                Ok(None) => {}
                Err(e) => tracing::warn!(
                    tool = %self.name,
                    error = %e,
                    "describe round trip failed; falling back to static description"
                ),
            }
        }
        Cow::Borrowed(&self.description)
    }

    fn schema(&self) -> Value {
        to_json_schema(self.schema)
    }

    fn audience(&self) -> ToolAudience {
        self.audience
    }

    fn tool_kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }

    fn examples(&self) -> Option<Value> {
        self.examples.clone()
    }

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let validated = validate(self.schema, input.clone())?;
        let permission_state = match &self.permission_scope_kind {
            Some(PermissionScopeKind::Field(field)) => {
                let scope = validated.get(field.as_ref()).and_then(|v| v.as_str());
                PermissionState::Ready(Some(match scope {
                    Some(s) => PermissionScopes::single(s.to_owned()),
                    None => PermissionScopes::force_prompt(validated.to_string()),
                }))
            }
            Some(PermissionScopeKind::Callback) => PermissionState::NeedsCompute,
            None => PermissionState::Ready(None),
        };
        Ok(Box::new(LuaToolInvocation {
            tool: Arc::clone(&self.name),
            plugin: Arc::clone(&self.plugin),
            has_header_fn: self.has_header_fn,
            has_start_fn: self.has_start_fn,
            input: validated,
            tx: self.tx.clone(),
            permission_state,
            mutable_path_field: self.mutable_path_field.clone(),
            timeout: self.timeout,
            start_annotation: self.start_annotation.clone(),
        }))
    }
}

enum PermissionState {
    Ready(Option<PermissionScopes>),
    NeedsCompute,
}

struct LuaToolInvocation {
    tool: Arc<str>,
    plugin: Arc<str>,
    has_header_fn: bool,
    has_start_fn: bool,
    input: Value,
    tx: Sender<Request>,
    permission_state: PermissionState,
    mutable_path_field: Option<Arc<str>>,
    timeout: Option<Duration>,
    start_annotation: Option<StartAnnotation>,
}

impl ToolInvocation for LuaToolInvocation {
    fn start_header(&self) -> HeaderFuture {
        if !self.has_header_fn {
            return HeaderFuture::Ready(HeaderResult::plain(self.tool.to_string()));
        }
        let (reply_tx, reply_rx) = flume::bounded::<HeaderResult>(1);
        let tool = Arc::clone(&self.tool);
        let plugin = Arc::clone(&self.plugin);
        let input = self.input.clone();
        let tx = self.tx.clone();
        let fallback = tool.to_string();
        HeaderFuture::Pending {
            fallback: fallback.clone(),
            fut: Box::pin(async move {
                let sent = tx
                    .send_async(Request::ComputeHeader {
                        plugin: Arc::clone(&plugin),
                        tool: Arc::clone(&tool),
                        input,
                        reply: reply_tx,
                    })
                    .await;
                if sent.is_err() {
                    return HeaderResult::plain(fallback);
                }
                reply_rx
                    .recv_async()
                    .await
                    .unwrap_or_else(|_| HeaderResult::plain(fallback))
            }),
        }
    }

    fn start_annotation(&self) -> Option<String> {
        match self.start_annotation.as_ref()? {
            StartAnnotation::Timeout(field) => {
                let secs = self.input.get(field.as_ref())?.as_u64()?;
                Some(timeout_annotation(secs))
            }
            StartAnnotation::Count(field) => {
                let field = field.as_ref();
                let n = self.input.get(field)?.as_array()?.len();
                let (singular, plural) = if let Some(stem) = field.strip_suffix('s') {
                    (stem, field)
                } else {
                    (field, &*format!("{field}s"))
                };
                let label = if n == 1 { singular } else { plural };
                Some(format!("{n} {label}"))
            }
        }
    }

    fn start<'a>(&'a self, ctx: &'a ToolContext) -> BoxFuture<'a, ()> {
        let id = ctx.tool_use_id.as_ref().filter(|_| self.has_start_fn);
        let Some(id) = id else {
            return Box::pin(std::future::ready(()));
        };
        let (reply_tx, reply_rx) = flume::bounded::<()>(1);
        let req = Request::StartTool {
            plugin: Arc::clone(&self.plugin),
            tool: Arc::clone(&self.tool),
            input: self.input.clone(),
            live: LiveCtx {
                event_tx: ctx.event_tx.clone(),
                tool_use_id: id.clone(),
            },
            ctx: Box::new(LuaCtx::start(ctx)),
            reply: reply_tx,
        };
        let tx = self.tx.clone();
        Box::pin(async move {
            if tx.send_async(req).await.is_ok() {
                let _ = reply_rx.recv_async().await;
            }
        })
    }

    fn permission_scopes(&self) -> BoxFuture<'_, Option<PermissionScopes>> {
        match &self.permission_state {
            PermissionState::Ready(v) => Box::pin(std::future::ready(v.clone())),
            PermissionState::NeedsCompute => {
                let (reply_tx, reply_rx) = flume::bounded(1);
                let tx = self.tx.clone();
                let plugin = Arc::clone(&self.plugin);
                let tool = Arc::clone(&self.tool);
                let input = self.input.clone();
                let fallback = input.to_string();
                Box::pin(async move {
                    if tx
                        .send_async(Request::ComputePermissionScopes {
                            plugin,
                            tool,
                            input,
                            reply: reply_tx,
                        })
                        .await
                        .is_err()
                    {
                        return Some(PermissionScopes::force_prompt(fallback));
                    }
                    match reply_rx.recv_async().await {
                        Ok(Some(scopes)) => Some(scopes),
                        _ => Some(PermissionScopes::force_prompt(fallback)),
                    }
                })
            }
        }
    }

    fn mutable_path(&self) -> Option<&Path> {
        let field = self.mutable_path_field.as_deref()?;
        let val = self.input.get(field)?.as_str()?;
        Some(Path::new(val))
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        let deadline = ctx.deadline;
        let plugin = self.plugin;
        let tool = self.tool;
        let input = self.input;
        let tx = self.tx;
        let tool_timeout = self.timeout;

        Box::pin(async move {
            let effective_secs: Option<u64> = match tool_timeout {
                Some(d) => match deadline.cap_timeout(d.as_secs()) {
                    Ok(s) => Some(s),
                    Err(e) => return Err(e).into(),
                },
                None => match deadline {
                    Deadline::At(_) => match deadline.cap_timeout(u64::MAX) {
                        Ok(s) => Some(s),
                        Err(e) => return Err(e).into(),
                    },
                    Deadline::None => None,
                },
            };

            let (reply_tx, reply_rx) = flume::bounded::<ToolCallReply>(1);
            let live = ctx.tool_use_id.clone().map(|id| LiveCtx {
                event_tx: ctx.event_tx.clone(),
                tool_use_id: id,
            });
            let lua_ctx = LuaCtx::handler(ctx);

            if tx
                .send_async(Request::CallTool {
                    plugin: Arc::clone(&plugin),
                    tool: Arc::clone(&tool),
                    input,
                    ctx: Box::new(lua_ctx),
                    deadline: match deadline {
                        Deadline::At(t) => Some(t),
                        Deadline::None => None,
                    },
                    reply: reply_tx,
                    live,
                })
                .await
                .is_err()
            {
                return Err("lua thread disconnected".to_string()).into();
            }

            let recv = async { Some(reply_rx.recv_async().await) };
            let result = match effective_secs {
                Some(secs) => {
                    futures_lite::future::race(recv, async move {
                        smol::Timer::after(Duration::from_secs(secs)).await;
                        None
                    })
                    .await
                }
                None => recv.await,
            };

            match result {
                None => Err(format!(
                    "plugin {} tool {} exceeded timeout ({}s)",
                    plugin,
                    tool,
                    effective_secs.unwrap_or(0)
                ))
                .into(),
                Some(Err(_)) => Err("lua thread disconnected".to_string()).into(),
                Some(Ok(reply)) => {
                    if let Some(ref id) = ctx.tool_use_id {
                        if let Some(live_buf) = reply.live_buf {
                            let _ = ctx.event_tx.send(AgentEvent::LiveToolBuf {
                                id: id.clone(),
                                body: live_buf,
                            });
                        }
                        crate::runtime::RestoreReply {
                            body: reply.snapshot,
                            header: reply.header,
                        }
                        .emit(id, None, &ctx.event_tx);
                    }
                    let format = reply.format;
                    let instructions = reply.instructions;
                    let image = reply.image;
                    let state = reply.state;
                    ToolExecResult {
                        output: reply.result.map(|s| {
                            if let Some(source) = image {
                                ToolOutput::Image { source, text: s }
                            } else if let Some(diff) = reply.diff {
                                ToolOutput::Diff {
                                    summary: s,
                                    path: diff.path,
                                    before: diff.before,
                                    after: diff.after,
                                }
                            } else {
                                let inner = TextOutput {
                                    text: s,
                                    instructions: instructions.filter(|b| !b.is_empty()),
                                    state,
                                };
                                match format {
                                    LuaOutputFormat::Markdown => ToolOutput::Markdown(inner),
                                    LuaOutputFormat::Plain => ToolOutput::Plain(inner),
                                }
                            }
                        }),
                        annotation: reply.annotation,
                        written_path: reply.written_path,
                    }
                }
            }
        })
    }
}

fn parse_slot(spec: &Table) -> LuaResult<Slot> {
    spec.get::<String>("slot")
        .map_err(|_| mlua::Error::runtime("'slot' is required"))?
        .parse()
        .map_err(|_| {
            mlua::Error::runtime(format!("unknown 'slot'. Valid: {}", Slot::valid_names()))
        })
}

fn parse_prompt_field(spec: &Table) -> LuaResult<Option<Vec<PromptId>>> {
    let parse_one = |s: &str| -> mlua::Result<PromptId> {
        s.parse().map_err(|_| {
            mlua::Error::runtime(format!(
                "unknown 'prompt'. Valid: {}",
                PromptId::valid_names()
            ))
        })
    };
    match spec.get::<LuaValue>("prompt") {
        Ok(LuaValue::String(s)) => Ok(Some(vec![parse_one(&s.to_str()?)?])),
        Ok(LuaValue::Table(t)) => {
            let mut ids = Vec::new();
            for pair in t.sequence_values::<mlua::String>() {
                ids.push(parse_one(&pair?.to_str()?)?);
            }
            if ids.is_empty() {
                return Err(mlua::Error::runtime(
                    "'prompt' table is empty or has no sequence entries; expected a list like {\"system\", \"general\"}",
                ));
            }
            Ok(Some(ids))
        }
        Ok(LuaValue::Nil) | Err(_) => Ok(None),
        Ok(_) => Err(mlua::Error::runtime(
            "'prompt' must be a string or list of strings",
        )),
    }
}

fn validate_slot_prompt_compatibility(
    slot: Slot,
    prompts: &Option<Vec<PromptId>>,
) -> LuaResult<()> {
    if let Some(prompts) = prompts {
        for &pid in prompts {
            if !pid.has_slot(slot) {
                return Err(mlua::Error::runtime(format!(
                    "slot '{}' is not available for prompt '{}'",
                    slot, pid
                )));
            }
        }
    }
    Ok(())
}

fn parse_hint_content(lua: &Lua, spec: &Table) -> LuaResult<HintContent> {
    let has_content = spec.contains_key("content")?;
    if !has_content {
        return Err(mlua::Error::runtime("'content' is required"));
    }

    match spec.get("content")? {
        LuaValue::String(s) => {
            let text = s.to_string_lossy();
            if text.is_empty() {
                return Err(mlua::Error::runtime("'content' must not be empty"));
            }
            if text.len() > MAX_HINT_CONTENT_SIZE {
                return Err(mlua::Error::runtime(format!(
                    "content exceeds the {} byte limit",
                    MAX_HINT_CONTENT_SIZE
                )));
            }
            Ok(HintContent::Static(text))
        }
        LuaValue::Function(f) => Ok(HintContent::Callback(lua.create_registry_value(f)?)),
        _ => Err(mlua::Error::runtime(
            "'content' must be a string or function",
        )),
    }
}

pub(crate) fn create_api_table(
    lua: &Lua,
    pending: PendingTools,
    plugin: Arc<str>,
) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "register_tool",
        lua.create_function(move |lua, spec: Table| {
            register_tool_from_lua(lua, &spec, pending.clone())
        })?,
    )?;

    {
        let plugin = Arc::clone(&plugin);
        t.set(
            "register_prompt_hint",
            lua.create_function(move |lua, spec: Table| {
                let slot: Slot = parse_slot(&spec)?;
                if slot.kind() == SlotKind::Singleton {
                    return Err(mlua::Error::runtime(format!(
                        "register_prompt_hint is for aggregate slots ({}); \
                         use set_prompt for singleton slots ({})",
                        Slot::names_for_kind(SlotKind::Aggregate),
                        Slot::names_for_kind(SlotKind::Singleton),
                    )));
                }
                let prompts = parse_prompt_field(&spec)?;
                validate_slot_prompt_compatibility(slot, &prompts)?;

                let content = parse_hint_content(lua, &spec)?;
                let reg = PromptHintRegistration {
                    prompts,
                    slot,
                    content,
                };
                let mut map = lua
                    .app_data_mut::<PromptHintCallbacks>()
                    .ok_or_else(|| mlua::Error::runtime("not initialized"))?;
                map.entry(Arc::clone(&plugin)).or_default().push(reg);
                Ok(())
            })?,
        )?;
    }

    {
        let plugin = Arc::clone(&plugin);
        t.set(
            "set_prompt",
            lua.create_function(move |lua, spec: Table| {
                let slot: Slot = parse_slot(&spec)?;
                if slot.kind() == SlotKind::Aggregate {
                    return Err(mlua::Error::runtime(format!(
                        "set_prompt is for singleton slots ({}); \
                         use register_prompt_hint for aggregate slots ({})",
                        Slot::names_for_kind(SlotKind::Singleton),
                        Slot::names_for_kind(SlotKind::Aggregate),
                    )));
                }

                let prompts = parse_prompt_field(&spec)?;
                validate_slot_prompt_compatibility(slot, &prompts)?;

                let content = parse_hint_content(lua, &spec)?;
                let reg = PromptHintRegistration {
                    prompts,
                    slot,
                    content,
                };
                let mut map = lua
                    .app_data_mut::<PromptHintCallbacks>()
                    .ok_or_else(|| mlua::Error::runtime("not initialized"))?;
                map.entry(Arc::clone(&plugin)).or_default().push(reg);
                Ok(())
            })?,
        )?
    }

    t.set(
        "register_command",
        lua.create_function(move |lua, spec: Table| {
            register_command_from_lua(lua, &spec, Arc::clone(&plugin))
        })?,
    )?;

    t.set("get_tools", lua.create_function(get_tools_from_lua)?)?;
    t.set("get_tool", lua.create_function(get_tool_from_lua)?)?;

    Ok(t)
}

/// Registry snapshot without descriptions (avoids recursion from describe callbacks).
fn get_tools_from_lua(lua: &Lua, opts: Option<Table>) -> LuaResult<Table> {
    let registry = lua
        .app_data_ref::<Arc<ToolRegistry>>()
        .map(|r| Arc::clone(&r))
        .ok_or_else(|| mlua::Error::runtime("get_tools: tool registry not available"))?;
    let mut disabled: Vec<String> = Vec::new();
    if let Some(o) = opts
        && let Some(config) = o.get::<Option<Table>>("config")?
    {
        disabled = config
            .get::<Option<Vec<String>>>("disabled_tools")?
            .unwrap_or_default();
    }

    let out = lua.create_table()?;
    for (i, entry) in registry.iter().iter().enumerate() {
        let t = tool_entry_to_lua(lua, entry)?;
        t.set("enabled", is_tool_enabled(&disabled, entry.name()))?;
        out.set(i + 1, t)?;
    }
    Ok(out)
}

fn tool_entry_to_lua(lua: &Lua, entry: &RegisteredTool) -> LuaResult<Table> {
    let audience = entry.tool.audience();
    let audiences = lua.create_table()?;
    for (flag, name) in maki_agent::tools::registry::AUDIENCE_NAMES {
        if audience.contains(*flag) {
            audiences.push(*name)?;
        }
    }
    let t = lua.create_table()?;
    t.set("name", entry.name())?;
    t.set("schema", json_to_lua(lua, &entry.tool.schema())?)?;
    t.set("audiences", audiences)?;
    if let Some(kind) = entry.tool.tool_kind() {
        t.set("kind", kind)?;
    }
    Ok(t)
}

/// Single-tool lookup that also hands out `header`/`restore` handles for
/// Lua tools (nil for MCP or missing tools), wrapped so one broken tool
/// cannot break a composing caller. Kept apart from `get_tools` so the
/// listing stays a cheap data-only snapshot.
fn get_tool_from_lua(lua: &Lua, name: String) -> LuaResult<LuaValue> {
    let registry = lua
        .app_data_ref::<Arc<ToolRegistry>>()
        .map(|r| Arc::clone(&r))
        .ok_or_else(|| mlua::Error::runtime("get_tool: tool registry not available"))?;
    let Some(entry) = registry.get(&name) else {
        return Ok(LuaValue::Nil);
    };
    let t = tool_entry_to_lua(lua, &entry)?;
    if let Some((header, restore)) = local_tool_handles(&name) {
        if let Some(f) = header {
            t.set("header", wrap_header(lua, name.clone(), f)?)?;
        }
        if let Some(f) = restore {
            t.set("restore", wrap_restore(lua, name, f)?)?;
        }
    }
    Ok(LuaValue::Table(t))
}

/// Async so wrapped fns can yield (e.g. bash restore highlights inline).
fn wrap_nothrow(
    lua: &Lua,
    tool: String,
    handle: &'static str,
    f: Function,
    norm: fn(&Lua, LuaValue) -> LuaResult<LuaValue>,
) -> LuaResult<Function> {
    lua.create_async_function(move |lua, args: MultiValue| {
        let (f, tool) = (f.clone(), tool.clone());
        async move {
            match f.call_async::<LuaValue>(args).await {
                Ok(v) => norm(&lua, v),
                Err(e) => {
                    tracing::warn!(tool, handle, error = %e, "get_tool handle failed");
                    Ok(LuaValue::Nil)
                }
            }
        }
    })
}

/// Normalizes a header fn to one spans line or nil: a plain string gets
/// the standalone header style, a buf return contributes its first line.
fn wrap_header(lua: &Lua, tool: String, f: Function) -> LuaResult<Function> {
    wrap_nothrow(lua, tool, "header", f, |lua, v| match v {
        LuaValue::String(s) => {
            let span = lua.create_table()?;
            span.raw_set(1, s)?;
            span.raw_set(2, PLAIN_HEADER_STYLE)?;
            let line = lua.create_table()?;
            line.raw_set(1, span)?;
            Ok(LuaValue::Table(line))
        }
        LuaValue::UserData(ud) => {
            let Ok(h) = ud.borrow::<BufHandle>() else {
                return Ok(LuaValue::Nil);
            };
            match h.buf.read().first() {
                Some(line) => Ok(LuaValue::Table(line_to_lua(lua, line)?)),
                None => Ok(LuaValue::Nil),
            }
        }
        _ => Ok(LuaValue::Nil),
    })
}

/// Normalizes a restore fn to its body buf or nil (whether it returned
/// the buf directly or a `{ body = buf }` reply), so callers composing
/// another tool's rendering need no pcall of their own. The ctx arg may be
/// a real `LuaCtx` or a plain `{ tool_output_lines =, state = }` table (how
/// batch drives child restores); either way the fn sees a restore `LuaCtx`.
fn wrap_restore(lua: &Lua, tool: String, f: Function) -> LuaResult<Function> {
    let prepped = lua.create_async_function(move |lua, mut args: MultiValue| {
        let f = f.clone();
        async move {
            let ctx = normalize_restore_ctx(&lua, args.get(3))?;
            while args.len() < 4 {
                args.push_back(LuaValue::Nil);
            }
            args[3] = ctx;
            f.call_async::<MultiValue>(args).await
        }
    })?;
    wrap_nothrow(lua, tool, "restore", prepped, |_lua, v| {
        Ok(match &v {
            LuaValue::UserData(ud) if ud.is::<BufHandle>() => v,
            LuaValue::Table(t) => t
                .get::<mlua::AnyUserData>("body")
                .ok()
                .filter(|ud| ud.is::<BufHandle>())
                .map(LuaValue::UserData)
                .unwrap_or(LuaValue::Nil),
            _ => LuaValue::Nil,
        })
    })
}

fn normalize_restore_ctx(lua: &Lua, v: Option<&LuaValue>) -> LuaResult<LuaValue> {
    if let Some(LuaValue::UserData(ud)) = v
        && ud.is::<LuaCtx>()
    {
        return Ok(LuaValue::UserData(ud.clone()));
    }
    let (tol, state) = match v {
        Some(LuaValue::Table(t)) => (
            t.get::<LuaValue>("tool_output_lines")
                .ok()
                .and_then(|v| lua.from_value::<ToolOutputLines>(v).ok())
                .unwrap_or_default(),
            t.get::<LuaValue>("state")
                .ok()
                .and_then(|v| lua_to_json(lua, &v).ok())
                .filter(|v| !v.is_null()),
        ),
        _ => (ToolOutputLines::default(), None),
    };
    let ud = lua.create_userdata(LuaCtx::restore(tol, state))?;
    Ok(LuaValue::UserData(ud))
}

fn is_valid_tool_name(name: &str) -> bool {
    if name.is_empty() || name.len() > TOOL_NAME_MAX {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_audience(audiences: Option<mlua::Table>) -> LuaResult<ToolAudience> {
    let Some(arr) = audiences else {
        return Ok(ToolAudience::default());
    };
    let mut flags = ToolAudience::empty();
    let mut count = 0;
    for item in arr.sequence_values::<String>() {
        let s = item?;
        count += 1;
        flags |= match s.as_str() {
            "all" => ToolAudience::all(),
            other => ToolAudience::parse_name(other)
                .ok_or_else(|| mlua::Error::runtime(format!("unknown audience: {other}")))?,
        };
    }
    if count == 0 {
        return Err(mlua::Error::runtime(
            "register_tool: 'audiences' must be omitted or non-empty",
        ));
    }
    Ok(flags)
}

fn parse_timeout(spec: &Table) -> LuaResult<Option<Duration>> {
    let value: LuaValue = spec.get("timeout").unwrap_or(LuaValue::Nil);
    match value {
        LuaValue::Nil | LuaValue::Boolean(false) => Ok(None),
        LuaValue::Integer(0) => Ok(None),
        LuaValue::Integer(n) if n > 0 => Ok(Some(Duration::from_secs(n as u64))),
        LuaValue::Number(n) if n > 0.0 && n.is_finite() => Ok(Some(Duration::from_secs(n as u64))),
        LuaValue::Number(0.0) => Ok(None),
        _ => Err(mlua::Error::runtime(TIMEOUT_PARSE_ERR)),
    }
}

fn spec_opt<T: mlua::FromLua>(spec: &Table, key: &str, expected: &str) -> LuaResult<Option<T>> {
    spec.get::<Option<T>>(key)
        .map_err(|_| mlua::Error::runtime(format!("register_tool: '{key}' must be {expected}")))
}

fn check_schema_field(schema: &Value, key: &str, field: &str, expected: &str) -> LuaResult<()> {
    let matches = schema
        .get("properties")
        .and_then(|p| p.get(field))
        .and_then(|s| s.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t == expected);
    if matches {
        Ok(())
    } else {
        Err(mlua::Error::runtime(format!(
            "register_tool: {key} field '{field}' not in schema properties or not type '{expected}'"
        )))
    }
}

fn require_schema_field(spec: &Table, key: &str, schema: &Value) -> LuaResult<Option<Arc<str>>> {
    let Some(field) = spec_opt::<String>(spec, key, "a string")? else {
        return Ok(None);
    };
    check_schema_field(schema, key, &field, "string")?;
    Ok(Some(Arc::from(field.as_str())))
}

fn parse_start_annotation(spec: &Table, schema: &Value) -> LuaResult<Option<StartAnnotation>> {
    match spec.get::<Option<LuaValue>>("start_annotation")? {
        None => Ok(None),
        Some(LuaValue::String(s)) => {
            let field = s.to_str()?.to_owned();
            check_schema_field(schema, "start_annotation", &field, "array")?;
            Ok(Some(StartAnnotation::Count(Arc::from(field.as_str()))))
        }
        Some(LuaValue::Table(t)) => {
            let field: String = t.get("field").map_err(|_| {
                mlua::Error::runtime("register_tool: start_annotation.field required")
            })?;
            if t.get::<Option<String>>("kind")?.as_deref() != Some("timeout") {
                return Err(mlua::Error::runtime(
                    "register_tool: start_annotation.kind must be 'timeout'",
                ));
            }
            check_schema_field(schema, "start_annotation", &field, "integer")?;
            Ok(Some(StartAnnotation::Timeout(Arc::from(field.as_str()))))
        }
        Some(_) => Err(mlua::Error::runtime(
            "register_tool: 'start_annotation' must be a string field name or a table",
        )),
    }
}

fn register_tool_from_lua(lua: &Lua, spec: &Table, pending: PendingTools) -> LuaResult<()> {
    let name: String = spec
        .get("name")
        .map_err(|_| mlua::Error::runtime("register_tool: missing 'name'"))?;
    if !is_valid_tool_name(&name) {
        return Err(mlua::Error::runtime(format!(
            "register_tool: invalid name '{name}'"
        )));
    }
    let description: String = spec.get("description").unwrap_or_default();
    if description.trim().is_empty() {
        return Err(mlua::Error::runtime(
            "register_tool: description must be non-empty",
        ));
    }
    let handler: Function = spec
        .get("handler")
        .map_err(|_| mlua::Error::runtime("register_tool: missing 'handler'"))?;
    let schema_table: LuaValue = spec
        .get("schema")
        .map_err(|_| mlua::Error::runtime("register_tool: missing 'schema'"))?;
    let audiences: Option<mlua::Table> = spec.get("audiences").ok();

    let schema_val: Value = lua.from_value(schema_table)?;
    let param_schema = try_from_json(&schema_val).map_err(mlua::Error::runtime)?;

    if !spec.get::<LuaValue>("permission_scope")?.is_nil() {
        return Err(mlua::Error::runtime(
            "register_tool: 'permission_scope' was removed; use permission_scopes = \"<field>\" or permission_scopes = function(input) ... end",
        ));
    }
    let mutable_path_field = require_schema_field(spec, "mutable_path", &schema_val)?;

    let permission_scopes = match spec.get::<LuaValue>("permission_scopes")? {
        LuaValue::Nil => None,
        LuaValue::String(s) => {
            let field = s.to_str()?.to_owned();
            check_schema_field(&schema_val, "permission_scopes", &field, "string")?;
            Some(PermissionScopeSpec::Field(Arc::from(field.as_str())))
        }
        LuaValue::Function(f) => Some(PermissionScopeSpec::Callback(lua.create_registry_value(f)?)),
        _ => {
            return Err(mlua::Error::runtime(
                "register_tool: 'permission_scopes' must be a string field name or a function",
            ));
        }
    };

    let header_fn: Option<Function> = spec.get("header").ok();
    let restore_fn: Option<Function> = spec.get("restore").ok();
    let start_fn: Option<Function> = spec.get("start").ok();
    let kind: Option<Arc<str>> = spec
        .get::<String>("kind")
        .ok()
        .map(|s| Arc::from(s.as_str()));
    let audience = parse_audience(audiences)?;
    let timeout = parse_timeout(spec)?;
    let start_annotation = parse_start_annotation(spec, &schema_val)?;
    let handler_key: RegistryKey = lua.create_registry_value(handler)?;
    let header_key = header_fn
        .map(|f| lua.create_registry_value(f))
        .transpose()?;
    let restore_key = restore_fn
        .map(|f| lua.create_registry_value(f))
        .transpose()?;
    let start_key = start_fn.map(|f| lua.create_registry_value(f)).transpose()?;

    let describe_fn: Option<Function> = spec.get("describe").ok();
    let describe_key = describe_fn
        .map(|f| lua.create_registry_value(f))
        .transpose()?;

    let examples: Option<Value> =
        spec_opt::<Table>(spec, "examples", "a table (array of example inputs)")?
            .map(|t| lua.from_value(LuaValue::Table(t)))
            .transpose()
            .map_err(|e| mlua::Error::runtime(format!("register_tool: invalid examples: {e}")))?;

    let name: Arc<str> = Arc::from(name.as_str());

    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(PendingTool {
            name,
            description,
            schema: param_schema,
            audience,
            kind,
            handler_key,
            header_key,
            restore_key,
            start_key,
            permission_scopes,
            mutable_path_field,
            timeout,
            start_annotation,
            examples,
            describe_key,
        });

    Ok(())
}

fn register_command_from_lua(lua: &Lua, spec: &Table, plugin: Arc<str>) -> LuaResult<()> {
    let name: String = spec
        .get("name")
        .map_err(|_| mlua::Error::runtime("register_command: missing 'name'"))?;
    if name.is_empty() {
        return Err(mlua::Error::runtime(
            "register_command: name must be non-empty",
        ));
    }
    let description: String = spec.get("description").unwrap_or_default();
    let handler: Function = spec
        .get("handler")
        .map_err(|_| mlua::Error::runtime("register_command: missing 'handler'"))?;

    let handler_key = lua.create_registry_value(handler)?;
    let name: Arc<str> = Arc::from(name.as_str());
    let description: Arc<str> = Arc::from(description.as_str());

    {
        let mut map = lua
            .app_data_mut::<CommandHandlerMap>()
            .ok_or_else(|| mlua::Error::runtime("register_command: not initialized"))?;
        map.entry(Arc::clone(&plugin)).or_default().insert(
            Arc::clone(&name),
            CommandEntry {
                handler: handler_key,
                description,
            },
        );
    }

    let map = lua
        .app_data_ref::<CommandHandlerMap>()
        .ok_or_else(|| mlua::Error::runtime("register_command: not initialized"))?;
    let writer = lua
        .app_data_ref::<LuaCommandWriter>()
        .ok_or_else(|| mlua::Error::runtime("register_command: not initialized"))?;
    publish_command_snapshot(&map, &writer);

    Ok(())
}

pub(crate) type ToolCallResult = Result<String, String>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum LuaOutputFormat {
    #[default]
    Plain,
    Markdown,
}

const LUA_FORMAT_MARKDOWN: &str = "markdown";
const LUA_FORMAT_PLAIN: &str = "plain";

pub(crate) struct DiffPayload {
    pub path: String,
    pub before: String,
    pub after: String,
}

pub(crate) struct ToolCallReply {
    pub result: ToolCallResult,
    pub snapshot: Option<BufferSnapshot>,
    pub header: Option<BufferSnapshot>,
    pub live_buf: Option<Arc<SharedBuf>>,
    pub format: LuaOutputFormat,
    pub annotation: Option<String>,
    pub instructions: Option<Vec<InstructionBlock>>,
    pub written_path: Option<String>,
    pub diff: Option<DiffPayload>,
    /// Set via `image = { media_type = "image/png", data = <base64> }` in the
    /// handler return; becomes `ToolOutput::Image` with `llm_output` as caption.
    pub image: Option<ImageSource>,
    pub state: Option<Value>,
}

impl ToolCallReply {
    pub fn from_lua_value(lua: &Lua, val: &LuaValue) -> Self {
        let mut result = coerce_tool_result(val);
        let LuaValue::Table(t) = val else {
            return Self::plain(result);
        };
        let (snapshot, live_buf) = Self::extract_body_handle(t);
        let header = t
            .get::<LuaValue>("header")
            .ok()
            .and_then(|v| Self::extract_snapshot(&v));
        let format = extract_format(t);
        let annotation = t.get::<String>("annotation").ok();
        let instructions = extract_instructions(t);
        let written_path = t.get::<String>("written_path").ok();
        let diff = t.get::<String>("diff_path").ok().map(|path| DiffPayload {
            path,
            before: t.get::<String>("diff_before").ok().unwrap_or_default(),
            after: t.get::<String>("diff_after").ok().unwrap_or_default(),
        });
        // A malformed image fails the call; dropping it silently would leave
        // a caption claiming pixels the model never receives.
        let image = match extract_image(t) {
            Ok(image) => image,
            Err(e) => {
                result = Err(e);
                None
            }
        };
        let state = match t.get::<LuaValue>("state") {
            Ok(LuaValue::Nil) | Err(_) => None,
            Ok(v) => crate::api::util::convert::lua_to_json(lua, &v)
                .inspect_err(|e| tracing::warn!(error = %e, "tool state is not JSON-serializable, dropping it"))
                .ok(),
        };
        Self {
            result,
            snapshot,
            header,
            live_buf,
            format,
            annotation,
            instructions,
            written_path,
            diff,
            image,
            state,
        }
    }

    fn extract_body_handle(t: &mlua::Table) -> (Option<BufferSnapshot>, Option<Arc<SharedBuf>>) {
        t.get::<LuaValue>("body")
            .ok()
            .and_then(|v| {
                let ud = v.as_userdata()?;
                let h = ud.borrow::<BufHandle>().ok()?;
                Some((Some(h.buf.take()), Some(Arc::clone(&h.buf))))
            })
            .unwrap_or((None, None))
    }

    fn extract_snapshot(val: &LuaValue) -> Option<BufferSnapshot> {
        let ud = val.as_userdata()?;
        let h = ud.borrow::<BufHandle>().ok()?;
        Some(h.buf.take())
    }

    pub fn plain(result: ToolCallResult) -> Self {
        Self {
            result,
            snapshot: None,
            header: None,
            live_buf: None,
            format: LuaOutputFormat::default(),
            annotation: None,
            instructions: None,
            written_path: None,
            diff: None,
            image: None,
            state: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self::plain(Err(msg.into()))
    }
}

fn extract_format(t: &mlua::Table) -> LuaOutputFormat {
    let Ok(LuaValue::String(s)) = t.get::<LuaValue>("format") else {
        return LuaOutputFormat::default();
    };
    let Ok(s) = s.to_str() else {
        return LuaOutputFormat::default();
    };
    match &*s {
        LUA_FORMAT_MARKDOWN => LuaOutputFormat::Markdown,
        LUA_FORMAT_PLAIN => LuaOutputFormat::Plain,
        _ => LuaOutputFormat::default(),
    }
}

fn extract_image(t: &mlua::Table) -> Result<Option<ImageSource>, String> {
    let entry = match t.get::<LuaValue>("image") {
        Ok(LuaValue::Table(entry)) => entry,
        Ok(LuaValue::Nil) | Err(_) => return Ok(None),
        Ok(other) => {
            return Err(format!(
                "tool 'image' field must be a table {{ media_type, data }}, got {}",
                other.type_name()
            ));
        }
    };
    let media_type = entry
        .get::<String>("media_type")
        .map_err(|_| "tool image is missing 'media_type'".to_owned())?;
    let media_type = ImageMediaType::from_mime(&media_type).ok_or_else(|| {
        let supported: Vec<&str> = ImageMediaType::ALL.iter().map(|m| m.mime()).collect();
        format!(
            "unsupported tool image media_type '{media_type}' ({})",
            supported.join(", ")
        )
    })?;
    let data = entry
        .get::<String>("data")
        .map_err(|_| "tool image is missing base64 'data'".to_owned())?;
    // Bad base64 would land in history and fail every later request;
    // validate once at the boundary.
    if data.is_empty() {
        return Err("tool image 'data' is empty".to_owned());
    }
    BASE64
        .decode(data.as_bytes())
        .map_err(|e| format!("tool image 'data' is not valid base64: {e}"))?;
    Ok(Some(ImageSource::new(media_type, Arc::from(data))))
}

fn extract_instructions(t: &mlua::Table) -> Option<Vec<InstructionBlock>> {
    let Ok(LuaValue::Table(arr)) = t.get::<LuaValue>("instructions") else {
        return None;
    };
    let mut blocks = Vec::new();
    for pair in arr.sequence_values::<LuaValue>() {
        let Ok(LuaValue::Table(entry)) = pair else {
            continue;
        };
        let Ok(path) = entry.get::<String>("path") else {
            continue;
        };
        let Ok(content) = entry.get::<String>("content") else {
            continue;
        };
        blocks.push(InstructionBlock { path, content });
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks)
    }
}

pub(crate) fn coerce_tool_result(result: &LuaValue) -> ToolCallResult {
    match result {
        LuaValue::String(s) => s.to_str().map(|s| s.to_owned()).map_err(|e| e.to_string()),
        LuaValue::Table(t) => {
            let output = t.get::<LuaValue>("llm_output").ok().and_then(|v| {
                if let LuaValue::String(s) = v {
                    s.to_str().ok().map(|s| s.to_owned())
                } else {
                    None
                }
            });
            match output {
                Some(s) if matches!(t.get::<LuaValue>("is_error"), Ok(LuaValue::Boolean(true))) => {
                    Err(s)
                }
                Some(s) => Ok(s),
                None => Err(TOOL_HANDLER_RETURN_ERR.to_string()),
            }
        }
        _ => Err(TOOL_HANDLER_RETURN_ERR.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case::test_case("echo", true ; "simple_name")]
    #[test_case::test_case("tool123", true ; "trailing_digits")]
    #[test_case::test_case("_leading", true ; "leading_underscore")]
    #[test_case::test_case("_", true ; "single_underscore")]
    #[test_case::test_case("snake_case_123", true ; "snake_with_digits")]
    #[test_case::test_case(&"a".repeat(TOOL_NAME_MAX), true ; "max_length_ok")]
    #[test_case::test_case("", false ; "empty")]
    #[test_case::test_case("../../bash", false ; "path_traversal")]
    #[test_case::test_case("foo bar", false ; "space")]
    #[test_case::test_case("1foo", false ; "leading_digit")]
    #[test_case::test_case("foo-bar", false ; "hyphen")]
    #[test_case::test_case("foo.bar", false ; "dot")]
    #[test_case::test_case("foo@bar", false ; "at_sign")]
    #[test_case::test_case("café", false ; "non_ascii")]
    #[test_case::test_case(&"a".repeat(TOOL_NAME_MAX + 1), false ; "too_long")]
    fn tool_name_validation(name: &str, expected: bool) {
        assert_eq!(is_valid_tool_name(name), expected);
    }

    #[test_case::test_case(
        r#"{ llm_output = "c", image = { data = "aGVsbG8=" } }"#,
        "missing 'media_type'" ; "missing_media_type")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = { media_type = "image/png" } }"#,
        "missing base64 'data'" ; "missing_data")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = { media_type = "image/png", data = "" } }"#,
        "'data' is empty" ; "empty_data")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = "nope" }"#,
        "must be a table" ; "image_not_a_table")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = { media_type = "image/bmp", data = "aGVsbG8=" } }"#,
        "unsupported tool image media_type" ; "unsupported_media_type")]
    #[test_case::test_case(
        r#"{ llm_output = "c", image = { media_type = "image/png", data = "!!!not base64!!!" } }"#,
        "not valid base64" ; "data_not_base64")]
    fn malformed_image_reply_fails_the_call(src: &str, expected: &str) {
        let lua = Lua::new();
        let val: LuaValue = lua.load(format!("return {src}")).eval().unwrap();
        let reply = ToolCallReply::from_lua_value(&lua, &val);
        assert!(reply.image.is_none());
        let err = reply.result.expect_err("malformed image must error");
        assert!(err.contains(expected), "got: {err}");
    }

    fn invocation(input: Value) -> LuaToolInvocation {
        let (tx, _rx) = flume::unbounded();
        LuaToolInvocation {
            tool: Arc::from("test_tool"),
            plugin: Arc::from("test"),
            has_header_fn: false,
            input,
            tx,
            permission_state: PermissionState::Ready(None),
            mutable_path_field: None,
            timeout: Some(Duration::from_secs(60)),
            start_annotation: None,
            has_start_fn: false,
        }
    }

    #[test_case::test_case(serde_json::json!({"timeout": 90}), Some(timeout_annotation(90)) ; "present")]
    #[test_case::test_case(serde_json::json!({}),              None                        ; "absent")]
    fn start_annotation_timeout(input: Value, expected: Option<String>) {
        let inv = LuaToolInvocation {
            start_annotation: Some(StartAnnotation::Timeout(Arc::from("timeout"))),
            ..invocation(input)
        };
        assert_eq!(inv.start_annotation(), expected);
    }

    #[test]
    fn describe_falls_back_to_static_description_when_runtime_unavailable() {
        use maki_agent::tools::ToolFilter;

        let mut tool = make_lua_tool(None);
        tool.has_describe_fn = true;
        let ctx = DescriptionContext {
            filter: &ToolFilter::All,
            audience: ToolAudience::MAIN,
            workflow: false,
        };
        assert_eq!(tool.description(&ctx), "test");
    }

    fn make_lua_tool(permission_scope_kind: Option<PermissionScopeKind>) -> LuaTool {
        let schema = try_from_json(&serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "format": { "type": "string" },
                "count": { "type": "integer" },
            },
            "required": ["url"],
        }))
        .unwrap();
        let (tx, _rx) = flume::unbounded();
        LuaTool {
            name: Arc::from("test_tool"),
            description: "test".into(),
            schema,
            audience: ToolAudience::default(),
            kind: None,
            tx,
            plugin: Arc::from("test"),
            has_header_fn: false,
            permission_scope_kind,
            mutable_path_field: None,
            timeout: Some(Duration::from_secs(60)),
            start_annotation: None,
            has_start_fn: false,
            examples: None,
            has_describe_fn: false,
        }
    }

    #[test]
    fn permission_scope_extracted_at_parse_time() {
        let tool = make_lua_tool(Some(PermissionScopeKind::Field(Arc::from("url"))));
        let inv = tool
            .parse(&serde_json::json!({"url": "https://example.com"}))
            .unwrap();
        let scopes = smol::block_on(inv.permission_scopes());
        assert_eq!(
            scopes.unwrap().scopes,
            vec!["https://example.com".to_string()]
        );
    }

    #[test_case::test_case("format" ; "absent_field")]
    #[test_case::test_case("count" ; "non_string_field")]
    fn permission_scope_field_invalid_forces_prompt(field: &str) {
        let input = serde_json::json!({"url": "https://example.com", "count": 42});
        let inv = make_lua_tool(Some(PermissionScopeKind::Field(Arc::from(field))))
            .parse(&input)
            .unwrap();
        let scopes = smol::block_on(inv.permission_scopes()).expect("should fail closed");
        assert!(scopes.force_prompt);
        assert_eq!(scopes.scopes, vec![input.to_string()]);
    }

    #[test]
    fn permission_scope_none_when_unconfigured() {
        let unconfigured = make_lua_tool(None)
            .parse(&serde_json::json!({"url": "https://example.com"}))
            .unwrap();
        assert!(smol::block_on(unconfigured.permission_scopes()).is_none());
    }

    #[test]
    fn coerce_string_returns_ok() {
        let lua = Lua::new();
        let val = LuaValue::String(lua.create_string("hello").unwrap());
        assert_eq!(coerce_tool_result(&val), Ok("hello".to_string()));
    }

    #[test]
    fn coerce_table_with_is_error_true() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("llm_output", "boom").unwrap();
        t.set("is_error", true).unwrap();
        assert_eq!(
            coerce_tool_result(&LuaValue::Table(t)),
            Err("boom".to_string())
        );
    }

    #[test]
    fn coerce_error_paths() {
        let lua = Lua::new();
        assert_eq!(
            coerce_tool_result(&LuaValue::Nil),
            Err(TOOL_HANDLER_RETURN_ERR.to_string())
        );
        assert_eq!(
            coerce_tool_result(&LuaValue::Boolean(true)),
            Err(TOOL_HANDLER_RETURN_ERR.to_string())
        );
        assert!(coerce_tool_result(&LuaValue::Table(lua.create_table().unwrap())).is_err());
    }

    #[test_case::test_case(LUA_FORMAT_MARKDOWN, LuaOutputFormat::Markdown ; "markdown")]
    #[test_case::test_case(LUA_FORMAT_PLAIN,    LuaOutputFormat::Plain    ; "plain")]
    #[test_case::test_case("unknown",           LuaOutputFormat::Plain    ; "unknown_defaults_to_plain")]
    fn extract_format_known_values(value: &str, expected: LuaOutputFormat) {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("format", value).unwrap();
        assert_eq!(extract_format(&t), expected);
    }

    #[test]
    fn extract_format_missing_defaults_to_plain() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        assert_eq!(extract_format(&t), LuaOutputFormat::Plain);
    }

    #[test]
    fn needs_compute_fallback_on_failure() {
        // Closed channel → fallback to force_prompt
        let (tx, rx) = flume::bounded(0);
        drop(rx);
        let inv = LuaToolInvocation {
            tool: Arc::from("bash"),
            plugin: Arc::from("test"),
            has_header_fn: false,
            input: serde_json::json!({"command": "ls"}),
            tx,
            permission_state: PermissionState::NeedsCompute,
            mutable_path_field: None,
            timeout: None,
            start_annotation: None,
            has_start_fn: false,
        };
        let scopes = smol::block_on(inv.permission_scopes()).expect("should fallback");
        assert!(scopes.force_prompt);
        assert!(!scopes.scopes.is_empty());

        // Callback returns None → fallback to force_prompt
        let (tx2, rx2) = flume::bounded(1);
        let inv2 = LuaToolInvocation {
            tool: Arc::from("bash"),
            plugin: Arc::from("test"),
            has_header_fn: false,
            input: serde_json::json!({"command": "echo hi"}),
            tx: tx2,
            permission_state: PermissionState::NeedsCompute,
            mutable_path_field: None,
            timeout: None,
            start_annotation: None,
            has_start_fn: false,
        };
        std::thread::spawn(move || {
            if let Ok(Request::ComputePermissionScopes { reply, .. }) = rx2.recv() {
                let _ = reply.send(None);
            }
        });
        let scopes2 = smol::block_on(inv2.permission_scopes()).expect("should fallback");
        assert!(scopes2.force_prompt);
    }

    #[test]
    fn needs_compute_returns_callback_result() {
        let (tx, rx) = flume::bounded(1);
        let inv = LuaToolInvocation {
            tool: Arc::from("bash"),
            plugin: Arc::from("test"),
            has_header_fn: false,
            input: serde_json::json!({"command": "cargo test"}),
            tx,
            permission_state: PermissionState::NeedsCompute,
            mutable_path_field: None,
            timeout: None,
            start_annotation: None,
            has_start_fn: false,
        };
        std::thread::spawn(move || {
            if let Ok(Request::ComputePermissionScopes { reply, .. }) = rx.recv() {
                let _ = reply.send(Some(PermissionScopes {
                    scopes: vec!["cargo".into(), "test".into()],
                    force_prompt: false,
                }));
            }
        });
        let result = smol::block_on(inv.permission_scopes());
        let scopes = result.unwrap();
        assert_eq!(scopes.scopes, vec!["cargo", "test"]);
        assert!(!scopes.force_prompt);
    }

    fn timeout_spec(lua: &Lua, value: LuaValue) -> Table {
        let t = lua.create_table().unwrap();
        if !matches!(value, LuaValue::Nil) {
            t.set("timeout", value).unwrap();
        }
        t
    }

    fn timeout_ok(lua: &Lua, value: LuaValue) -> Option<Duration> {
        parse_timeout(&timeout_spec(lua, value)).unwrap()
    }

    fn timeout_err(lua: &Lua, value: LuaValue) {
        let err = parse_timeout(&timeout_spec(lua, value)).unwrap_err();
        assert!(err.to_string().contains(TIMEOUT_PARSE_ERR));
    }

    #[test]
    fn timeout_parsing_none_means_infinite() {
        let lua = Lua::new();
        assert_eq!(timeout_ok(&lua, LuaValue::Nil), None);
        assert_eq!(timeout_ok(&lua, LuaValue::Boolean(false)), None);
        assert_eq!(timeout_ok(&lua, LuaValue::Integer(0)), None);
        assert_eq!(timeout_ok(&lua, LuaValue::Number(0.0)), None);
    }

    #[test]
    fn timeout_parsing_valid_values() {
        let lua = Lua::new();
        assert_eq!(
            timeout_ok(&lua, LuaValue::Integer(30)),
            Some(Duration::from_secs(30))
        );
        let big: f64 = 1e10;
        assert_eq!(
            timeout_ok(&lua, LuaValue::Number(big)),
            Some(Duration::from_secs(big as u64))
        );
        assert_eq!(
            timeout_ok(&lua, LuaValue::Number(0.5)),
            Some(Duration::from_secs(0))
        );
    }

    #[test]
    fn timeout_parsing_invalid_rejected() {
        let lua = Lua::new();
        timeout_err(&lua, LuaValue::Integer(-1));
        timeout_err(&lua, LuaValue::Number(-1.5));
        timeout_err(&lua, LuaValue::Boolean(true));
        timeout_err(&lua, LuaValue::Number(f64::INFINITY));
        timeout_err(&lua, LuaValue::Number(f64::NEG_INFINITY));
        timeout_err(&lua, LuaValue::Number(f64::NAN));
        let s = lua.create_string("forever").unwrap();
        timeout_err(&lua, LuaValue::String(s));
    }

    fn reply_table(lua: &Lua, output: &str, format: Option<&str>, is_error: bool) -> LuaValue {
        let t = lua.create_table().unwrap();
        t.set("llm_output", output).unwrap();
        if is_error {
            t.set("is_error", true).unwrap();
        }
        if let Some(f) = format {
            t.set("format", f).unwrap();
        }
        LuaValue::Table(t)
    }

    #[test]
    fn from_lua_value_table_with_markdown_format_ok() {
        let lua = Lua::new();
        let val = reply_table(&lua, "hi", Some(LUA_FORMAT_MARKDOWN), false);
        let reply = ToolCallReply::from_lua_value(&lua, &val);
        assert_eq!(reply.result, Ok("hi".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Markdown);
    }

    #[test]
    fn from_lua_value_table_with_markdown_format_and_is_error_captures_format() {
        // The format field is read on its own, separate from is_error, so a
        // handler that fails can still ask for its error message to be rendered
        // as markdown.
        let lua = Lua::new();
        let val = reply_table(&lua, "boom", Some(LUA_FORMAT_MARKDOWN), true);
        let reply = ToolCallReply::from_lua_value(&lua, &val);
        assert_eq!(reply.result, Err("boom".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Markdown);
    }

    #[test]
    fn from_lua_value_table_without_format_defaults_to_plain() {
        let lua = Lua::new();
        let val = reply_table(&lua, "hi", None, false);
        let reply = ToolCallReply::from_lua_value(&lua, &val);
        assert_eq!(reply.result, Ok("hi".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Plain);
    }

    #[test]
    fn from_lua_value_non_table_defaults_to_plain() {
        let lua = Lua::new();
        let string_val = LuaValue::String(lua.create_string("hello").unwrap());
        let reply = ToolCallReply::from_lua_value(&lua, &string_val);
        assert_eq!(reply.result, Ok("hello".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Plain);

        let bool_reply = ToolCallReply::from_lua_value(&lua, &LuaValue::Boolean(true));
        assert_eq!(bool_reply.result, Err(TOOL_HANDLER_RETURN_ERR.to_string()));
        assert_eq!(bool_reply.format, LuaOutputFormat::Plain);
    }

    #[test]
    fn from_lua_value_extracts_instructions() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("llm_output", "file contents").unwrap();

        let inst1 = lua.create_table().unwrap();
        inst1.set("path", "AGENTS.md").unwrap();
        inst1.set("content", "be nice").unwrap();

        let instructions = lua.create_table().unwrap();
        instructions.set(1, inst1).unwrap();
        t.set("instructions", instructions).unwrap();

        let reply = ToolCallReply::from_lua_value(&lua, &LuaValue::Table(t));
        assert_eq!(reply.result, Ok("file contents".to_string()));
        let blocks = reply.instructions.expect("instructions should be Some");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].path, "AGENTS.md");
        assert_eq!(blocks[0].content, "be nice");
    }

    #[test]
    fn from_lua_value_extracts_diff_fields() {
        let lua = Lua::new();

        let t = lua.create_table().unwrap();
        t.set("llm_output", "edited file.rs").unwrap();
        t.set("diff_path", "/tmp/file.rs").unwrap();
        t.set("diff_before", "old content").unwrap();
        t.set("diff_after", "new content").unwrap();
        let diff = ToolCallReply::from_lua_value(&lua, &LuaValue::Table(t))
            .diff
            .expect("diff should be Some");
        assert_eq!(diff.path, "/tmp/file.rs");
        assert_eq!(diff.before, "old content");
        assert_eq!(diff.after, "new content");

        let partial = lua.create_table().unwrap();
        partial.set("llm_output", "patched").unwrap();
        partial.set("diff_path", "/tmp/file.rs").unwrap();
        let d = ToolCallReply::from_lua_value(&lua, &LuaValue::Table(partial))
            .diff
            .unwrap();
        assert_eq!(d.before, "");
        assert_eq!(d.after, "");
    }

    #[test]
    fn start_annotation_returns_none_for_non_array_or_missing() {
        let no_field = invocation(serde_json::json!({"edits": [1, 2]}));
        assert_eq!(no_field.start_annotation(), None);

        let not_array = LuaToolInvocation {
            start_annotation: Some(StartAnnotation::Count(Arc::from("edit"))),
            ..invocation(serde_json::json!({"edit": "not an array"}))
        };
        assert_eq!(not_array.start_annotation(), None);

        let wrong_key = LuaToolInvocation {
            start_annotation: Some(StartAnnotation::Count(Arc::from("edit"))),
            ..invocation(serde_json::json!({"other_field": [1, 2]}))
        };
        assert_eq!(wrong_key.start_annotation(), None);
    }

    #[test_case::test_case("edits", serde_json::json!({"edits": [1]}),      Some("1 edit")   ; "singular")]
    #[test_case::test_case("edits", serde_json::json!({"edits": [1, 2, 3]}), Some("3 edits")  ; "plural")]
    #[test_case::test_case("item",  serde_json::json!({"item": [1, 2]}),     Some("2 items")  ; "field_without_trailing_s")]
    #[test_case::test_case("edits", serde_json::json!({"edits": []}),        Some("0 edits")  ; "empty_array")]
    fn start_annotation_count(field: &str, input: Value, expected: Option<&str>) {
        let inv = LuaToolInvocation {
            start_annotation: Some(StartAnnotation::Count(Arc::from(field))),
            ..invocation(input)
        };
        assert_eq!(inv.start_annotation(), expected.map(String::from));
    }

    #[test]
    fn start_without_tool_use_id_is_noop() {
        let inv = LuaToolInvocation {
            has_start_fn: true,
            ..invocation(serde_json::json!({"code": "x"}))
        };
        let ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
        smol::block_on(inv.start(&ctx));
    }
}
