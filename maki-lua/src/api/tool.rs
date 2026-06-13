use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::Sender;
use maki_agent::prompt::{PromptId, Slot};
use maki_agent::tools::Tool;
use maki_agent::tools::schema::{ParamSchema, to_json_schema, try_from_json, validate};
use maki_agent::tools::{
    BoxFuture, Deadline, DescriptionContext, ExecFuture, HeaderFuture, HeaderResult, ParseError,
    PermissionScopes, ToolAudience, ToolContext, ToolExecResult, ToolInvocation,
};
use maki_agent::{AgentEvent, BufferSnapshot, SharedBuf, ToolOutput};
use mlua::{
    Function, Lua, LuaSerdeExt, RegistryKey, Result as LuaResult, Table, Value as LuaValue,
};
use serde_json::Value;

use crate::api::buf::BufHandle;
use crate::api::command::{
    CommandEntry, CommandHandlerMap, LuaCommandWriter, publish_command_snapshot,
};
use crate::api::ctx::LuaCtx;
use crate::runtime::{HintContent, LiveCtx, PromptHintCallbacks, PromptHintRegistration, Request};

const TOOL_NAME_MAX: usize = 64;
const TOOL_HANDLER_RETURN_ERR: &str =
    "tool handler must return string or {output=string, is_error?=bool}";
const TIMEOUT_PARSE_ERR: &str = "register_tool: 'timeout' must be a positive number, 0, or false";

#[derive(Clone)]
pub(crate) enum PermissionScopeKind {
    Field(Arc<str>),
    Callback,
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
    pub(crate) permission_scope_kind: Option<PermissionScopeKind>,
    pub(crate) permission_scopes_key: Option<RegistryKey>,
    pub(crate) timeout: Option<Duration>,
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
    pub(crate) permission_scope_kind: Option<PermissionScopeKind>,
    pub(crate) timeout: Option<Duration>,
}

impl Tool for LuaTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self, _ctx: &DescriptionContext) -> Cow<'_, str> {
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

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let validated = validate(self.schema, input.clone())?;
        let permission_state = match &self.permission_scope_kind {
            Some(PermissionScopeKind::Field(field)) => {
                let scope = validated
                    .get(field.as_ref())
                    .and_then(|v| v.as_str())
                    .map(|s| PermissionScopes::single(s.to_owned()));
                PermissionState::Ready(scope)
            }
            Some(PermissionScopeKind::Callback) => PermissionState::NeedsCompute,
            None => PermissionState::Ready(None),
        };
        Ok(Box::new(LuaToolInvocation {
            tool: Arc::clone(&self.name),
            plugin: Arc::clone(&self.plugin),
            has_header_fn: self.has_header_fn,
            input: validated,
            tx: self.tx.clone(),
            permission_state,
            timeout: self.timeout,
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
    input: Value,
    tx: Sender<Request>,
    permission_state: PermissionState,
    timeout: Option<Duration>,
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
            let lua_ctx = LuaCtx {
                cancel: ctx.cancel.clone(),
                config: ctx.config.clone(),
                tool_output_lines: ctx.tool_output_lines,
                finish_tx: None,
            };

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
                    ToolExecResult {
                        output: reply.result.map(|s| match format {
                            LuaOutputFormat::Markdown => ToolOutput::Markdown(s),
                            LuaOutputFormat::Plain => ToolOutput::Plain(s),
                        }),
                        annotation: reply.annotation,
                    }
                }
            }
        })
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
                let slot: Slot = spec
                    .get::<String>("slot")
                    .map_err(|_| mlua::Error::runtime("'slot' is required"))?
                    .parse()
                    .map_err(|_| {
                        mlua::Error::runtime(
                            "unknown 'slot'. Valid: tool_usage, efficient_tools, conventions, after_instructions",
                        )
                    })?;

                let parse_prompt = |s: &str| -> mlua::Result<PromptId> {
                    s.parse().map_err(|_| {
                        mlua::Error::runtime("unknown 'prompt'. Valid: system, research, general")
                    })
                };
                let prompts: Option<Vec<PromptId>> = match spec.get::<LuaValue>("prompt") {
                    Ok(LuaValue::String(s)) => Some(vec![parse_prompt(&s.to_str()?)?]),
                    Ok(LuaValue::Table(t)) => {
                        let mut ids = Vec::new();
                        for pair in t.sequence_values::<mlua::String>() {
                            ids.push(parse_prompt(&pair?.to_str()?)?);
                        }
                        Some(ids)
                    }
                    Ok(LuaValue::Nil) | Err(_) => None,
                    Ok(_) => {
                        return Err(mlua::Error::runtime(
                            "'prompt' must be a string or list of strings",
                        ));
                    }
                };

                let content = match spec
                    .get("content")
                    .map_err(|_| mlua::Error::runtime("'content' is required"))?
                {
                    LuaValue::String(s) => HintContent::Static(s.to_string_lossy()),
                    LuaValue::Function(f) => HintContent::Callback(lua.create_registry_value(f)?),
                    _ => {
                        return Err(mlua::Error::runtime(
                            "'content' must be a string or function",
                        ));
                    }
                };

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

    t.set(
        "register_command",
        lua.create_function(move |lua, spec: Table| {
            register_command_from_lua(lua, &spec, Arc::clone(&plugin))
        })?,
    )?;

    Ok(t)
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
            "main" => ToolAudience::MAIN,
            "research_sub" => ToolAudience::RESEARCH_SUB,
            "general_sub" => ToolAudience::GENERAL_SUB,
            "interpreter" => ToolAudience::INTERPRETER,
            _ => {
                return Err(mlua::Error::runtime(format!("unknown audience: {s}")));
            }
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

    let permission_scope_field: Option<Arc<str>> = spec
        .get::<String>("permission_scope")
        .ok()
        .map(|s| Arc::from(s.as_str()));
    if let Some(ref field) = permission_scope_field {
        let is_string = schema_val
            .get("properties")
            .and_then(|p| p.get(field.as_ref()))
            .and_then(|s| s.get("type"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| t == "string");
        if !is_string {
            return Err(mlua::Error::runtime(format!(
                "register_tool: permission_scope field '{field}' not in schema properties or not type 'string'"
            )));
        }
    }

    let permission_scopes_fn: Option<Function> = spec.get("permission_scopes").ok();
    if permission_scope_field.is_some() && permission_scopes_fn.is_some() {
        return Err(mlua::Error::runtime(
            "register_tool: cannot specify both 'permission_scope' and 'permission_scopes'",
        ));
    }
    let permission_scopes_key = permission_scopes_fn
        .map(|f| lua.create_registry_value(f))
        .transpose()?;
    let permission_scope_kind = if permission_scopes_key.is_some() {
        Some(PermissionScopeKind::Callback)
    } else {
        permission_scope_field.map(PermissionScopeKind::Field)
    };

    let header_fn: Option<Function> = spec.get("header").ok();
    let restore_fn: Option<Function> = spec.get("restore").ok();
    let kind: Option<Arc<str>> = spec
        .get::<String>("kind")
        .ok()
        .map(|s| Arc::from(s.as_str()));
    let audience = parse_audience(audiences)?;
    let timeout = parse_timeout(spec)?;
    let handler_key: RegistryKey = lua.create_registry_value(handler)?;
    let header_key = header_fn
        .map(|f| lua.create_registry_value(f))
        .transpose()?;
    let restore_key = restore_fn
        .map(|f| lua.create_registry_value(f))
        .transpose()?;
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
            permission_scope_kind,
            permission_scopes_key,
            timeout,
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

pub(crate) struct ToolCallReply {
    pub result: ToolCallResult,
    pub snapshot: Option<BufferSnapshot>,
    pub header: Option<BufferSnapshot>,
    pub live_buf: Option<Arc<SharedBuf>>,
    pub format: LuaOutputFormat,
    pub annotation: Option<String>,
}

impl ToolCallReply {
    pub fn from_lua_value(val: &LuaValue) -> Self {
        let result = coerce_tool_result(val);
        let LuaValue::Table(t) = val else {
            return Self {
                result,
                snapshot: None,
                header: None,
                live_buf: None,
                format: LuaOutputFormat::default(),
                annotation: None,
            };
        };
        let (snapshot, live_buf) = Self::extract_body_handle(t);
        let header = t
            .get::<LuaValue>("header")
            .ok()
            .and_then(|v| Self::extract_snapshot(&v));
        let format = extract_format(t);
        let annotation = t.get::<String>("annotation").ok();
        Self {
            result,
            snapshot,
            header,
            live_buf,
            format,
            annotation,
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

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            result: Err(msg.into()),
            snapshot: None,
            header: None,
            live_buf: None,
            format: LuaOutputFormat::default(),
            annotation: None,
        }
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
    #[test_case::test_case("", false ; "empty")]
    #[test_case::test_case("../../bash", false ; "path_traversal")]
    #[test_case::test_case("foo bar", false ; "space")]
    #[test_case::test_case("1foo", false ; "leading_digit")]
    fn tool_name_validation(name: &str, expected: bool) {
        assert_eq!(is_valid_tool_name(name), expected);
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
            timeout: Some(Duration::from_secs(60)),
        }
    }

    #[test]
    fn no_header_fn_returns_tool_name() {
        let inv = invocation(serde_json::json!({"path": "/home/x/foo.rs"}));
        assert_eq!(inv.start_header().into_ready().text(), "test_tool");
    }

    fn make_lua_tool(permission_scope_kind: Option<PermissionScopeKind>) -> LuaTool {
        let schema = try_from_json(&serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "format": { "type": "string" },
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
            timeout: Some(Duration::from_secs(60)),
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

    #[test]
    fn permission_scope_none_when_field_absent_or_unconfigured() {
        let absent = make_lua_tool(Some(PermissionScopeKind::Field(Arc::from("format"))))
            .parse(&serde_json::json!({"url": "https://example.com"}))
            .unwrap();
        assert!(smol::block_on(absent.permission_scopes()).is_none());

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
            timeout: None,
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
            timeout: None,
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
            timeout: None,
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

    #[test]
    fn permission_scope_field_non_string_value_returns_none() {
        let schema = try_from_json(&serde_json::json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" },
            },
            "required": ["count"],
        }))
        .unwrap();
        let (tx, _rx) = flume::unbounded();
        let tool = LuaTool {
            name: Arc::from("test_tool"),
            description: "test".into(),
            schema,
            audience: ToolAudience::default(),
            kind: None,
            tx,
            plugin: Arc::from("test"),
            has_header_fn: false,
            permission_scope_kind: Some(PermissionScopeKind::Field(Arc::from("count"))),
            timeout: Some(Duration::from_secs(60)),
        };
        let inv = tool.parse(&serde_json::json!({"count": 42})).unwrap();
        assert!(smol::block_on(inv.permission_scopes()).is_none());
    }

    fn timeout_spec(lua: &Lua, value: LuaValue) -> Table {
        let t = lua.create_table().unwrap();
        if !matches!(value, LuaValue::Nil) {
            t.set("timeout", value).unwrap();
        }
        t
    }

    #[test]
    fn timeout_parsing_nil_yields_infinite() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Nil);
        assert_eq!(parse_timeout(&spec).unwrap(), None);
    }

    #[test]
    fn timeout_parsing_false_yields_infinite() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Boolean(false));
        assert_eq!(parse_timeout(&spec).unwrap(), None);
    }

    #[test]
    fn timeout_parsing_zero_yields_infinite() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Integer(0));
        assert_eq!(parse_timeout(&spec).unwrap(), None);
    }

    #[test]
    fn timeout_parsing_positive_seconds() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Integer(30));
        assert_eq!(parse_timeout(&spec).unwrap(), Some(Duration::from_secs(30)));
    }

    #[test_case::test_case(LuaValue::Integer(-1) ; "negative_integer")]
    #[test_case::test_case(LuaValue::Number(-1.5) ; "negative_float")]
    #[test_case::test_case(LuaValue::Boolean(true) ; "true_value")]
    fn timeout_parsing_invalid_rejected(value: LuaValue) {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, value);
        let err = parse_timeout(&spec).unwrap_err();
        assert!(err.to_string().contains(TIMEOUT_PARSE_ERR));
    }

    #[test]
    fn timeout_parsing_invalid_string_rejected() {
        let lua = Lua::new();
        let s = lua.create_string("forever").unwrap();
        let spec = timeout_spec(&lua, LuaValue::String(s));
        let err = parse_timeout(&spec).unwrap_err();
        assert!(err.to_string().contains(TIMEOUT_PARSE_ERR));
    }

    #[test]
    fn timeout_parsing_sub_second_float_truncates_to_zero() {
        // A sub-second float slips past `n > 0.0 && n.is_finite()`, then the
        // `n as u64` cast truncates it to 0, so the timeout fires right away.
        // Pinning this down so a future refactor does not silently change it.
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Number(0.5));
        assert_eq!(parse_timeout(&spec).unwrap(), Some(Duration::from_secs(0)));
    }

    #[test_case::test_case(f64::INFINITY ; "positive_infinity")]
    #[test_case::test_case(f64::NEG_INFINITY ; "negative_infinity")]
    #[test_case::test_case(f64::NAN ; "nan")]
    fn timeout_parsing_non_finite_float_rejected(n: f64) {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Number(n));
        let err = parse_timeout(&spec).unwrap_err();
        assert!(err.to_string().contains(TIMEOUT_PARSE_ERR));
    }

    #[test]
    fn timeout_parsing_large_finite_float_accepted() {
        let lua = Lua::new();
        let big: f64 = 1e10;
        let spec = timeout_spec(&lua, LuaValue::Number(big));
        assert_eq!(
            parse_timeout(&spec).unwrap(),
            Some(Duration::from_secs(big as u64))
        );
    }

    #[test]
    fn timeout_parsing_zero_float_yields_infinite() {
        let lua = Lua::new();
        let spec = timeout_spec(&lua, LuaValue::Number(0.0));
        assert_eq!(parse_timeout(&spec).unwrap(), None);
    }

    #[test]
    fn lua_output_format_default_is_plain() {
        assert_eq!(LuaOutputFormat::default(), LuaOutputFormat::Plain);
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
        let reply = ToolCallReply::from_lua_value(&val);
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
        let reply = ToolCallReply::from_lua_value(&val);
        assert_eq!(reply.result, Err("boom".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Markdown);
    }

    #[test]
    fn from_lua_value_table_without_format_defaults_to_plain() {
        let lua = Lua::new();
        let val = reply_table(&lua, "hi", None, false);
        let reply = ToolCallReply::from_lua_value(&val);
        assert_eq!(reply.result, Ok("hi".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Plain);
    }

    #[test]
    fn from_lua_value_string_value_defaults_to_plain() {
        let lua = Lua::new();
        let val = LuaValue::String(lua.create_string("hello").unwrap());
        let reply = ToolCallReply::from_lua_value(&val);
        assert_eq!(reply.result, Ok("hello".to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Plain);
        assert!(reply.snapshot.is_none());
        assert!(reply.live_buf.is_none());
        assert!(reply.header.is_none());
    }

    #[test]
    fn from_lua_value_non_table_non_string_is_err_with_default_format() {
        let reply = ToolCallReply::from_lua_value(&LuaValue::Boolean(true));
        assert_eq!(reply.result, Err(TOOL_HANDLER_RETURN_ERR.to_string()));
        assert_eq!(reply.format, LuaOutputFormat::Plain);
    }

    #[test]
    fn coerce_table_with_is_error_false_returns_ok() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("llm_output", "fine").unwrap();
        t.set("is_error", false).unwrap();
        assert_eq!(
            coerce_tool_result(&LuaValue::Table(t)),
            Ok("fine".to_string())
        );
    }

    #[test]
    fn coerce_table_with_non_string_output_is_err() {
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        t.set("llm_output", 123).unwrap();
        assert_eq!(
            coerce_tool_result(&LuaValue::Table(t)),
            Err(TOOL_HANDLER_RETURN_ERR.to_string())
        );
    }

    #[test_case::test_case("_leading", true ; "leading_underscore_allowed")]
    #[test_case::test_case("_", true ; "single_underscore")]
    #[test_case::test_case("snake_case_123", true ; "snake_with_digits")]
    #[test_case::test_case("foo-bar", false ; "hyphen_rejected")]
    #[test_case::test_case("foo.bar", false ; "dot_rejected")]
    #[test_case::test_case("foo@bar", false ; "at_sign_rejected")]
    #[test_case::test_case("café", false ; "non_ascii_rejected")]
    #[test_case::test_case("名前", false ; "unicode_rejected")]
    fn tool_name_validation_extra(name: &str, expected: bool) {
        assert_eq!(is_valid_tool_name(name), expected);
    }

    #[test]
    fn tool_name_validation_length_boundaries() {
        let max_ok: String = "a".repeat(TOOL_NAME_MAX);
        assert!(is_valid_tool_name(&max_ok));
        let too_long: String = "a".repeat(TOOL_NAME_MAX + 1);
        assert!(!is_valid_tool_name(&too_long));
    }

    #[test]
    fn lua_tool_kind_returns_configured_value() {
        let schema = try_from_json(&serde_json::json!({
            "type": "object",
            "properties": {},
        }))
        .unwrap();
        let (tx, _rx) = flume::unbounded();
        let tool = LuaTool {
            name: Arc::from("my_reader"),
            description: "reads things".into(),
            schema,
            audience: ToolAudience::default(),
            kind: Some(Arc::from("read")),
            tx,
            plugin: Arc::from("test"),
            has_header_fn: false,
            permission_scope_kind: None,
            timeout: None,
        };
        assert_eq!(tool.tool_kind(), Some("read"));
    }

    #[test]
    fn lua_tool_kind_none_by_default() {
        let tool = make_lua_tool(None);
        assert_eq!(tool.tool_kind(), None);
    }
}
