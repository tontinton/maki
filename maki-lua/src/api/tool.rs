use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flume::Sender;
use maki_agent::ToolOutput;
use maki_agent::tools::Tool;
use maki_agent::tools::schema::{
    ParamSchema, extract_summary_keys, to_json_schema, try_from_json, validate,
};
use maki_agent::tools::{
    Deadline, DescriptionContext, ExecFuture, ParseError, ToolAudience, ToolContext, ToolInvocation,
};
use mlua::{
    Function, Lua, LuaSerdeExt, RegistryKey, Result as LuaResult, Table, Value as LuaValue,
};
use serde_json::Value;

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
    pub(crate) summary_keys: &'static [&'static str],
}

pub(crate) type PendingTools = Arc<Mutex<Vec<PendingTool>>>;

pub(crate) struct LuaTool {
    pub(crate) name: Arc<str>,
    pub(crate) description: String,
    pub(crate) schema: &'static ParamSchema,
    pub(crate) audience: ToolAudience,
    pub(crate) tx: Sender<Request>,
    pub(crate) plugin: Arc<str>,
    pub(crate) summary_keys: &'static [&'static str],
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
        Ok(Box::new(LuaToolInvocation {
            tool: Arc::clone(&self.name),
            plugin: Arc::clone(&self.plugin),
            summary_keys: self.summary_keys,
            input: validated,
            tx: self.tx.clone(),
        }))
    }
}

struct LuaToolInvocation {
    tool: Arc<str>,
    plugin: Arc<str>,
    summary_keys: &'static [&'static str],
    input: Value,
    tx: Sender<Request>,
}

impl ToolInvocation for LuaToolInvocation {
    fn start_summary(&self) -> String {
        let parts: Vec<&str> = self
            .summary_keys
            .iter()
            .filter_map(|k| self.input.get(*k).and_then(Value::as_str))
            .collect();
        if parts.is_empty() {
            self.tool.to_string()
        } else {
            parts.join(" ")
        }
    }

    fn execute<'a>(self: Box<Self>, ctx: &'a ToolContext) -> ExecFuture<'a> {
        let deadline = ctx.deadline;
        let plugin = self.plugin;
        let tool = self.tool;
        let input = self.input;
        let tx = self.tx;

        Box::pin(async move {
            let timeout_secs = deadline.cap_timeout(TOOL_CALL_MAX_TIME.as_secs())?;

            let (reply_tx, reply_rx) = flume::bounded(1);
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
                Some(Ok(result)) => result.map(ToolOutput::Plain),
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
    let summary_keys = extract_summary_keys(&schema_val);
    let audience = parse_audience(audiences)?;
    let handler_key: RegistryKey = lua.create_registry_value(handler)?;
    let name: Arc<str> = Arc::from(name.as_str());

    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(PendingTool {
            name,
            description,
            schema: param_schema,
            audience,
            handler_key,
            summary_keys,
        });

    Ok(())
}

pub(crate) type ToolCallResult = Result<String, String>;
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

    fn invocation(summary_keys: &'static [&'static str], input: Value) -> LuaToolInvocation {
        let (tx, _rx) = flume::unbounded();
        LuaToolInvocation {
            tool: Arc::from("test_tool"),
            plugin: Arc::from("test"),
            summary_keys,
            input,
            tx,
        }
    }

    #[test_case::test_case(&["path"], serde_json::json!({"path": "/home/x/foo.rs"}), "/home/x/foo.rs" ; "extracts_declared_key")]
    #[test_case::test_case(&["path"], serde_json::json!({"count": 5}), "test_tool" ; "key_missing_falls_back_to_tool_name")]
    #[test_case::test_case(&["path"], serde_json::json!({"path": 42}), "test_tool" ; "non_string_value_falls_back_to_tool_name")]
    #[test_case::test_case(&[], serde_json::json!({"path": "/home/x/foo.rs"}), "test_tool" ; "no_keys_declared")]
    #[test_case::test_case(&["pattern", "path"], serde_json::json!({"pattern": "foo", "path": "/src"}), "foo /src" ; "multiple_keys_joined")]
    #[test_case::test_case(&["pattern", "path"], serde_json::json!({"pattern": "foo"}), "foo" ; "multiple_keys_partial")]
    fn start_summary_cases(summary_keys: &'static [&'static str], input: Value, expected: &str) {
        let inv = invocation(summary_keys, input);
        assert_eq!(inv.start_summary(), expected);
    }

    #[test]
    fn summary_extracted_from_schema() {
        let json = serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "summary": true },
                "count": { "type": "integer" }
            },
            "required": ["path"]
        });
        assert_eq!(extract_summary_keys(&json), &["path"]);
    }

    #[test]
    fn summary_multiple_from_schema() {
        let json = serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "summary": true },
                "path": { "type": "string", "summary": true },
                "limit": { "type": "integer" }
            },
            "required": ["pattern"]
        });
        let keys = extract_summary_keys(&json);
        assert!(keys.contains(&"pattern"));
        assert!(keys.contains(&"path"));
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn no_summary_in_schema() {
        let json = serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        });
        assert!(extract_summary_keys(&json).is_empty());
    }
}
