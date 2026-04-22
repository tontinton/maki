use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::Sender;
use maki_agent::tools::Tool;
use maki_agent::tools::schema::{ParamSchema, to_json_schema, try_from_json, validate};
use maki_agent::tools::{
    Deadline, DescriptionContext, ExecFuture, ParseError, SummaryFuture, ToolAudience, ToolContext,
    ToolInvocation,
};
use maki_agent::{AgentEvent, BufferSnapshot, RawRenderHints, ToolOutput};
use mlua::{
    Function, Lua, LuaSerdeExt, RegistryKey, Result as LuaResult, Table, Value as LuaValue,
};
use serde_json::Value;

use crate::api::buf::{BufHandle, BufferStore};
use crate::api::ctx::LuaCtx;
use crate::runtime::Request;

const TOOL_NAME_MAX: usize = 64;
const TOOL_HANDLER_RETURN_ERR: &str =
    "tool handler must return string or {output=string, is_error?=bool}";
const TOOL_CALL_MAX_TIME: Duration = Duration::from_secs(30);

pub(crate) struct PendingTool {
    pub(crate) name: Arc<str>,
    pub(crate) description: String,
    pub(crate) schema: &'static ParamSchema,
    pub(crate) audience: ToolAudience,
    pub(crate) handler_key: RegistryKey,
    pub(crate) summary_key: Option<RegistryKey>,
    pub(crate) permission_scope_field: Option<Arc<str>>,
    pub(crate) render_hints: Option<RawRenderHints>,
}

pub(crate) type PendingTools = Arc<Mutex<Vec<PendingTool>>>;

pub(crate) struct LuaTool {
    pub(crate) name: Arc<str>,
    pub(crate) description: String,
    pub(crate) schema: &'static ParamSchema,
    pub(crate) audience: ToolAudience,
    pub(crate) tx: Sender<Request>,
    pub(crate) plugin: Arc<str>,
    pub(crate) has_summary_fn: bool,
    pub(crate) permission_scope_field: Option<Arc<str>>,
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

    fn parse(&self, input: &Value) -> Result<Box<dyn ToolInvocation>, ParseError> {
        let validated = validate(self.schema, input.clone())?;
        let permission_scope = self.permission_scope_field.as_deref().and_then(|field| {
            validated
                .get(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
        });
        Ok(Box::new(LuaToolInvocation {
            tool: Arc::clone(&self.name),
            plugin: Arc::clone(&self.plugin),
            has_summary_fn: self.has_summary_fn,
            input: validated,
            tx: self.tx.clone(),
            permission_scope,
        }))
    }
}

struct LuaToolInvocation {
    tool: Arc<str>,
    plugin: Arc<str>,
    has_summary_fn: bool,
    input: Value,
    tx: Sender<Request>,
    permission_scope: Option<String>,
}

impl ToolInvocation for LuaToolInvocation {
    fn start_summary(&self) -> SummaryFuture {
        if !self.has_summary_fn {
            return SummaryFuture::Ready(self.tool.to_string());
        }
        let (reply_tx, reply_rx) = flume::bounded(1);
        let tool = Arc::clone(&self.tool);
        let plugin = Arc::clone(&self.plugin);
        let input = self.input.clone();
        let tx = self.tx.clone();
        let fallback = tool.to_string();
        SummaryFuture::Pending {
            fallback: fallback.clone(),
            fut: Box::pin(async move {
                let sent = tx
                    .send_async(Request::ComputeSummary {
                        plugin: Arc::clone(&plugin),
                        tool: Arc::clone(&tool),
                        input,
                        reply: reply_tx,
                    })
                    .await;
                if sent.is_err() {
                    return fallback;
                }
                reply_rx.recv_async().await.unwrap_or(fallback)
            }),
        }
    }

    fn permission_scope(&self) -> Option<String> {
        self.permission_scope.clone()
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        let deadline = ctx.deadline;
        let plugin = self.plugin;
        let tool = self.tool;
        let input = self.input;
        let tx = self.tx;

        Box::pin(async move {
            let timeout_secs = deadline.cap_timeout(TOOL_CALL_MAX_TIME.as_secs())?;

            let (reply_tx, reply_rx) = flume::bounded::<ToolCallReply>(1);
            let lua_ctx = LuaCtx {
                cancel: ctx.cancel.clone(),
                config: ctx.config.clone(),
            };

            tx.send_async(Request::CallTool {
                plugin: Arc::clone(&plugin),
                tool: Arc::clone(&tool),
                input,
                ctx: lua_ctx,
                deadline: match deadline {
                    Deadline::At(t) => Some(t),
                    Deadline::None => None,
                },
                reply: reply_tx,
            })
            .await
            .map_err(|_| "lua thread disconnected".to_string())?;

            let timeout = smol::Timer::after(Duration::from_secs(timeout_secs));
            let result =
                futures_lite::future::race(async { Some(reply_rx.recv_async().await) }, async {
                    timeout.await;
                    None
                })
                .await;

            match result {
                None => Err(format!(
                    "plugin {} tool {} exceeded timeout ({timeout_secs}s)",
                    plugin, tool
                )),
                Some(Err(_)) => Err("lua thread disconnected".to_string()),
                Some(Ok(reply)) => {
                    if let Some(ref id) = ctx.tool_use_id {
                        if let Some(snapshot) = reply.snapshot {
                            let _ = ctx.event_tx.send(AgentEvent::ToolSnapshot {
                                id: id.clone(),
                                snapshot,
                            });
                        }
                        if let Some(annotation) = reply.annotation {
                            let _ = ctx.event_tx.send(AgentEvent::ToolAnnotation {
                                id: id.clone(),
                                annotation,
                            });
                        }
                    }
                    reply.result.map(ToolOutput::Plain)
                }
            }
        })
    }
}

pub(crate) fn create_api_table(lua: &Lua, pending: PendingTools) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "register_tool",
        lua.create_function(move |lua, spec: Table| {
            register_tool_from_lua(lua, &spec, pending.clone())
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

fn parse_render_hints(spec: &Table) -> Option<RawRenderHints> {
    let render: Table = spec.get("render").ok()?;
    Some(RawRenderHints {
        header_style: render.get::<String>("header_style").ok(),
        output_lines: render.get::<usize>("output_lines").ok(),
        output_keep: render.get::<String>("output_keep").ok(),
        output_separator: render.get::<String>("output_separator").ok(),
        always_annotate: render.get::<bool>("always_annotate").ok(),
        skip_done_truncation: render.get::<bool>("skip_done_truncation").ok(),
    })
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

    let summary_fn: Option<Function> = spec.get("summary").ok();
    let audience = parse_audience(audiences)?;
    let handler_key: RegistryKey = lua.create_registry_value(handler)?;
    let summary_key = summary_fn
        .map(|f| lua.create_registry_value(f))
        .transpose()?;
    let name: Arc<str> = Arc::from(name.as_str());

    let render_hints = parse_render_hints(spec);

    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(PendingTool {
            name,
            description,
            schema: param_schema,
            audience,
            handler_key,
            summary_key,
            permission_scope_field,
            render_hints,
        });

    Ok(())
}

pub(crate) type ToolCallResult = Result<String, String>;

pub(crate) struct ToolCallReply {
    pub result: ToolCallResult,
    pub snapshot: Option<BufferSnapshot>,
    pub annotation: Option<String>,
}

impl ToolCallReply {
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            result: Err(msg.into()),
            snapshot: None,
            annotation: None,
        }
    }
}

pub(crate) fn extract_tool_return(lua: &Lua, result: &LuaValue) -> ToolCallReply {
    let text_result = coerce_tool_result(result);

    let (snapshot, annotation) = if let LuaValue::Table(t) = result {
        let snap = t.get::<LuaValue>("render_buf").ok().and_then(|v| {
            let ud = v.as_userdata()?;
            let h = ud.borrow::<BufHandle>().ok()?;
            lua.app_data_mut::<BufferStore>()?.take(h.0)
        });

        let ann = t
            .get::<LuaValue>("annotation")
            .ok()
            .and_then(|v| v.as_string().map(|s| s.to_string_lossy().to_string()));

        (snap, ann)
    } else {
        (None, None)
    };

    if let Some(mut store) = lua.app_data_mut::<BufferStore>() {
        store.clear();
    }

    ToolCallReply {
        result: text_result,
        snapshot,
        annotation,
    }
}

pub(crate) fn coerce_tool_result(result: &LuaValue) -> ToolCallResult {
    match result {
        LuaValue::String(s) => s.to_str().map(|s| s.to_owned()).map_err(|e| e.to_string()),
        LuaValue::Table(t) => {
            let output = t.get::<LuaValue>("output").ok().and_then(|v| {
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
    #[test_case::test_case("echo_tool", true ; "with_underscore")]
    #[test_case::test_case("_private", true ; "leading_underscore")]
    #[test_case::test_case("tool123", true ; "trailing_digits")]
    #[test_case::test_case("a", true ; "single_char")]
    #[test_case::test_case("", false ; "empty")]
    #[test_case::test_case("../../bash", false ; "path_traversal")]
    #[test_case::test_case("foo bar", false ; "space")]
    #[test_case::test_case("foo.bar", false ; "dot")]
    #[test_case::test_case("foo/bar", false ; "slash")]
    #[test_case::test_case("1foo", false ; "leading_digit")]
    fn tool_name_validation(name: &str, expected: bool) {
        assert_eq!(is_valid_tool_name(name), expected);
    }

    fn invocation(input: Value) -> LuaToolInvocation {
        let (tx, _rx) = flume::unbounded();
        LuaToolInvocation {
            tool: Arc::from("test_tool"),
            plugin: Arc::from("test"),
            has_summary_fn: false,
            input,
            tx,
            permission_scope: None,
        }
    }

    #[test]
    fn no_summary_fn_returns_tool_name() {
        let inv = invocation(serde_json::json!({"path": "/home/x/foo.rs"}));
        assert_eq!(inv.start_summary().into_ready(), "test_tool");
    }

    fn make_lua_tool(permission_scope_field: Option<&str>) -> LuaTool {
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
            tx,
            plugin: Arc::from("test"),
            has_summary_fn: false,
            permission_scope_field: permission_scope_field.map(Arc::from),
        }
    }

    #[test]
    fn permission_scope_extracted_at_parse_time() {
        let tool = make_lua_tool(Some("url"));
        let inv = tool
            .parse(&serde_json::json!({"url": "https://example.com"}))
            .unwrap();
        assert_eq!(
            inv.permission_scope(),
            Some("https://example.com".to_string())
        );
    }

    #[test_case::test_case(Some("format"), None ; "optional_field_absent_in_input")]
    #[test_case::test_case(None, None ; "no_field_configured")]
    fn permission_scope_none(field: Option<&str>, expected: Option<&str>) {
        let tool = make_lua_tool(field);
        let inv = tool
            .parse(&serde_json::json!({"url": "https://example.com"}))
            .unwrap();
        assert_eq!(inv.permission_scope(), expected.map(String::from));
    }

    fn lua_with_buf_store() -> Lua {
        let lua = Lua::new();
        lua.set_app_data(BufferStore::new());
        lua
    }

    #[test]
    fn extract_tool_return_plain_string() {
        let lua = lua_with_buf_store();
        let val = LuaValue::String(lua.create_string("result text").unwrap());
        let reply = extract_tool_return(&lua, &val);
        assert_eq!(reply.result, Ok("result text".to_string()));
        assert!(reply.snapshot.is_none());
        assert!(reply.annotation.is_none());
    }

    #[test]
    fn extract_tool_return_table_with_output() {
        let lua = lua_with_buf_store();
        let t = lua.create_table().unwrap();
        t.set("output", "hello").unwrap();
        let reply = extract_tool_return(&lua, &LuaValue::Table(t));
        assert_eq!(reply.result, Ok("hello".to_string()));
        assert!(reply.snapshot.is_none());
        assert!(reply.annotation.is_none());
    }

    #[test]
    fn extract_tool_return_table_with_annotation() {
        let lua = lua_with_buf_store();
        let t = lua.create_table().unwrap();
        t.set("output", "data").unwrap();
        t.set("annotation", "42 lines").unwrap();
        let reply = extract_tool_return(&lua, &LuaValue::Table(t));
        assert_eq!(reply.result, Ok("data".to_string()));
        assert_eq!(reply.annotation.as_deref(), Some("42 lines"));
    }

    #[test]
    fn extract_tool_return_with_render_buf() {
        let lua = lua_with_buf_store();
        let buf_id = {
            let mut store = lua.app_data_mut::<BufferStore>().unwrap();
            let id = store.create();
            store.append_line(
                id,
                maki_agent::SnapshotLine {
                    spans: vec![maki_agent::SnapshotSpan {
                        text: "styled".into(),
                        style: maki_agent::SpanStyle::Named("keyword".into()),
                    }],
                },
            );
            id
        };
        let handle = lua.create_userdata(BufHandle(buf_id)).unwrap();
        let t = lua.create_table().unwrap();
        t.set("output", "plain for llm").unwrap();
        t.set("render_buf", handle).unwrap();
        let reply = extract_tool_return(&lua, &LuaValue::Table(t));
        assert_eq!(reply.result, Ok("plain for llm".to_string()));
        let snap = reply.snapshot.unwrap();
        assert_eq!(snap.lines.len(), 1);
        assert_eq!(snap.lines[0].spans[0].text, "styled");
    }

    #[test]
    fn extract_tool_return_clears_orphaned_buffers() {
        let lua = lua_with_buf_store();
        {
            let mut store = lua.app_data_mut::<BufferStore>().unwrap();
            store.create();
            store.create();
        }
        let t = lua.create_table().unwrap();
        t.set("output", "ok").unwrap();
        let _reply = extract_tool_return(&lua, &LuaValue::Table(t));
        let mut store = lua.app_data_mut::<BufferStore>().unwrap();
        assert!(
            store.take(1).is_none(),
            "orphaned buffers should be cleared after extract"
        );
    }

    #[test]
    fn extract_tool_return_is_error_flag() {
        let lua = lua_with_buf_store();
        let t = lua.create_table().unwrap();
        t.set("output", "something broke").unwrap();
        t.set("is_error", true).unwrap();
        let reply = extract_tool_return(&lua, &LuaValue::Table(t));
        assert_eq!(reply.result, Err("something broke".to_string()));
    }

    #[test]
    fn extract_tool_return_missing_output_field() {
        let lua = lua_with_buf_store();
        let t = lua.create_table().unwrap();
        t.set("annotation", "some annotation").unwrap();
        let reply = extract_tool_return(&lua, &LuaValue::Table(t));
        assert!(reply.result.is_err(), "missing output should be an error");
        assert_eq!(reply.annotation.as_deref(), Some("some annotation"));
    }

    #[test]
    fn extract_tool_return_non_string_non_table() {
        let lua = lua_with_buf_store();
        let reply = extract_tool_return(&lua, &LuaValue::Integer(42));
        assert!(reply.result.is_err());
        assert!(reply.snapshot.is_none());
        assert!(reply.annotation.is_none());
    }
}
