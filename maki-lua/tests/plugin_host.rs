use std::collections::HashMap;
use std::sync::Arc;

use maki_agent::tools::{ToolRegistry, ToolSource, timeout_annotation};
use maki_config::{AlwaysThinking, PluginsConfig, ToolOutputLines};
use maki_lua::{PluginError, PluginHost};
use std::path::Path;

fn fresh_registry() -> Arc<ToolRegistry> {
    Arc::new(ToolRegistry::new())
}

fn builtins_host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = fresh_registry();
    let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_builtins(&PluginsConfig::from_tools(HashMap::new()))
        .unwrap();
    (reg, host)
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: serde_json::Value) -> Result<String, String> {
    exec_tool_in(reg, name, input, None)
}

fn exec_tool_in(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
    registry_override: Option<Arc<ToolRegistry>>,
) -> Result<String, String> {
    exec_output_in(reg, name, input, registry_override).map(|out| match out {
        maki_agent::ToolOutput::Plain(s) => s.text,
        other => panic!("unexpected output: {other:?}"),
    })
}

fn exec_tool_output(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
) -> Result<maki_agent::ToolOutput, String> {
    exec_output_in(reg, name, input, None)
}

fn exec_output_in(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
    registry_override: Option<Arc<ToolRegistry>>,
) -> Result<maki_agent::ToolOutput, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let mut ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    if let Some(r) = registry_override {
        ctx.registry = r;
    }
    smol::block_on(async { inv.execute(&ctx).await }).output
}

const ECHO_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "echo_",
    description = "echo",
    schema = {
        type = "object",
        properties = { msg = { type = "string" } },
        required = { "msg" }
    },
    audiences = { "main" },
    handler = function(input, ctx)
        return input.msg
    end
})
"#;

const MINIMAL_SCHEMA: &str =
    r#"{ type = "object", properties = {}, additionalProperties = false }"#;

const STRING_FIELD_SCHEMA: &str = r#"{
    type = "object",
    properties = { url = { type = "string" } },
    required = { "url" },
}"#;

const INVALID_PERMISSION_SCOPE_ERR: &str = "not in schema properties or not type 'string'";
const BAD_NAME_SRC: &str = r#"name = "bad name!", description = "test""#;
const EMPTY_DESC_SRC: &str = r#"name = "valid_name", description = """#;
const EMPTY_AUD_SRC: &str = r#"name = "no_aud", description = "test", audiences = {}"#;
const UNKNOWN_AUD_SRC: &str =
    r#"name = "bad_aud", description = "test", audiences = { "wurkflow" }"#;
const STRING_EXAMPLES_SRC: &str = r#"name = "ex_bad", description = "test", examples = "[]""#;
const TIMEOUT_FIELD_NOT_IN_SCHEMA_SRC: &str = r#"name = "to_bad", description = "test", start_annotation = { field = "timeout", kind = "timeout" }"#;
const NON_STRING_FIELD_SCHEMA: &str = r#"{
    type = "object",
    properties = { count = { type = "integer" } },
    required = { "count" },
}"#;

const CODE_SCHEMA: &str = r#"{
    type = "object",
    properties = { code = { type = "string" } },
    required = { "code" },
}"#;

const TIMEOUT_SCHEMA: &str = r#"{
    type = "object",
    properties = { timeout = { type = "integer" } },
    required = { "timeout" },
}"#;

const ARRAY_SCHEMA: &str = r#"{
    type = "object",
    properties = { edits = { type = "array", items = { type = "integer" } } },
    required = { "edits" },
}"#;

const START_ANNOTATION_COUNT_NON_ARRAY_SRC: &str =
    r#"name = "sa_bad", description = "test", start_annotation = "name""#;
const STRING_NAME_SCHEMA: &str = r#"{
    type = "object",
    properties = { name = { type = "string" } },
    required = { "name" },
}"#;
const JOB_BAD_CWD: &str = "~/definitely/not/a/dir";
const JOB_BAD_CWD_ERR_PREFIX: &str = "cwd is not a directory: ";
const NIL_WITHOUT_JOBS_ERR: &str =
    "handler returned nil without calling ctx:finish() or starting jobs";
const FINISH_CALLED_TWICE_ERR: &str = "ctx:finish() already called";
const DEADLINE_ALREADY_SET_ERR: &str = "ctx:set_deadline() already called";
const TIMED_OUT_SUBSTR: &str = "timed out";
const ALREADY_CALLED_ERR: &str = "already called";
const UNKNOWN_FIELD_ERR: &str = "unknown field";
const PERMISSION_DENIED_MSG: &str = "permission denied";

#[test]
fn stdlib_globals_accessible() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    for global in &["os", "debug", "string", "table", "math"] {
        let source =
            format!(r#"if {global} == nil then error("stdlib missing: {global} is nil") end"#);
        host.load_source(&format!("stdlib_check_{global}"), &source)
            .unwrap_or_else(|e| panic!("stdlib check for {global} failed: {e}"));
    }
}

#[test]
fn dangerous_globals_blocked() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    for global in &["io", "package"] {
        let source =
            format!(r#"if {global} ~= nil then error("sandbox leak: {global} is not nil") end"#);
        host.load_source(&format!("sandbox_check_{global}"), &source)
            .unwrap_or_else(|e| panic!("sandbox check for {global} failed: {e}"));
    }
}

#[test]
fn register_echo_tool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("echo_plugin", ECHO_PLUGIN).unwrap();

    let entry = reg.get("echo_").expect("echo_ tool not registered");
    assert_eq!(entry.tool.name(), "echo_");
    assert!(
        matches!(entry.source, ToolSource::Lua { ref plugin } if plugin.as_ref() == "echo_plugin"),
    );
    assert_eq!(entry.tool.tool_kind(), None);

    let out = exec_tool(&reg, "echo_", serde_json::json!({"msg": "hello"})).unwrap();
    assert_eq!(out, "hello");
}

#[test]
fn unload_round_trip() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    host.load_source("unload_test", ECHO_PLUGIN).unwrap();
    assert!(reg.has("echo_"));

    host.unload("unload_test").unwrap();
    assert!(!reg.has("echo_"));
}

#[test_case::test_case(BAD_NAME_SRC, "invalid name" ; "invalid_tool_name")]
#[test_case::test_case(EMPTY_DESC_SRC, "description must be non-empty" ; "empty_description")]
#[test_case::test_case(EMPTY_AUD_SRC, "audiences" ; "empty_audiences")]
#[test_case::test_case(UNKNOWN_AUD_SRC, "unknown audience" ; "unknown_audience")]
#[test_case::test_case(STRING_EXAMPLES_SRC, "'examples' must be a table" ; "string_examples")]
#[test_case::test_case(TIMEOUT_FIELD_NOT_IN_SCHEMA_SRC, "not type 'integer'" ; "timeout_field_not_in_schema")]
fn registration_validation_rejects(fields: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            {fields},
            schema = {MINIMAL_SCHEMA},
            handler = function(input, ctx) return "" end
        }})"#,
    );
    let err = host
        .load_source("validation_test", &src)
        .expect_err("expected validation error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains(expected_err), "got: {err}");
}

#[test_case::test_case(STRING_FIELD_SCHEMA, "nonexistent" ; "missing_field")]
#[test_case::test_case(NON_STRING_FIELD_SCHEMA, "count" ; "non_string_field")]
fn permission_scope_invalid_rejected(schema: &str, scope_field: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "bad_scope",
            description = "test",
            schema = {schema},
            permission_scope = "{scope_field}",
            handler = function() return "" end
        }})"#,
    );
    let err = host
        .load_source("bad_scope_plugin", &src)
        .expect_err("expected error for invalid permission_scope");

    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(
        err.to_string().contains(INVALID_PERMISSION_SCOPE_ERR),
        "got: {err}"
    );
}

#[test]
fn permission_scope_valid_string_field_accepted() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "ok_scope",
            description = "test",
            schema = {STRING_FIELD_SCHEMA},
            permission_scope = "url",
            handler = function() return "" end
        }})"#,
    );
    host.load_source("ok_scope_plugin", &src).unwrap();
    assert!(reg.has("ok_scope"));
}

#[test]
fn tool_kind_flows_to_trait() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "my_fetcher",
            description = "fetches things",
            schema = {MINIMAL_SCHEMA},
            kind = "fetch",
            handler = function() return "" end
        }})"#,
    );
    host.load_source("kind_plugin", &src).unwrap();
    let entry = reg.get("my_fetcher").expect("tool not registered");
    assert_eq!(entry.tool.tool_kind(), Some("fetch"));
}

#[test]
fn examples_table_flows_to_trait() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "with_examples",
            description = "test",
            schema = {STRING_FIELD_SCHEMA},
            examples = {{ {{ url = "https://example.com" }} }},
            handler = function() return "" end
        }})"#,
    );
    host.load_source("examples_plugin", &src).unwrap();
    let entry = reg.get("with_examples").expect("tool not registered");
    assert_eq!(
        entry.tool.examples(),
        Some(serde_json::json!([{"url": "https://example.com"}]))
    );
}

#[test]
fn interrupt_kills_infinite_loop_and_vm_recovers() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"
maki.api.register_tool({{
    name = "infinite_loop_",
    description = "loops forever",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx) while true do end end
}})
maki.api.register_tool({{
    name = "noop_after_loop",
    description = "returns ok",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx) return "ok" end
}})
"#,
    );
    host.load_source("loop_plugin", &src).unwrap();

    let entry = reg.get("infinite_loop_").expect("loop tool not registered");
    let inv = entry.tool.parse(&serde_json::json!({})).unwrap();
    let mut ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    ctx.deadline = maki_agent::tools::Deadline::after(std::time::Duration::from_secs(5));

    let result = smol::block_on(async { inv.execute(&ctx).await });

    assert!(result.output.is_err(), "expected error from timed-out loop");

    let ok = exec_tool(&reg, "noop_after_loop", serde_json::json!({}));
    assert!(ok.is_ok(), "VM poisoned after interrupt: {ok:?}");
}

#[test]
fn failed_load_leaves_no_tools_or_commands() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"
maki.api.register_tool({{
    name = "doomed",
    description = "never registered",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function() return "" end
}})
maki.api.register_command({{
    name = "/doomed",
    handler = function() end,
}})
error("plugin blew up after register")
"#,
    );
    let err = host
        .load_source("broken", &src)
        .expect_err("expected lua error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(!reg.has("doomed"));
    assert_eq!(host.command_reader().load().commands.len(), 0);

    host.load_source("broken", ECHO_PLUGIN)
        .expect("retry with good source should succeed");
    assert!(reg.has("echo_"));
}

#[test]
fn is_error_propagated_as_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "returns_error",
            description = "returns is_error=true",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                return {{ llm_output = "boom", is_error = true }}
            end
        }})"#,
    );
    host.load_source("err_plugin", &src).unwrap();

    let err = exec_tool(&reg, "returns_error", serde_json::json!({})).unwrap_err();
    assert_eq!(err, "boom");
}

#[test]
fn handler_bad_return_type_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "bad_ret_num",
            description = "bad return",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function() return 42 end
        }})"#,
    );
    host.load_source("bad_ret", &src).unwrap();

    let err = exec_tool(&reg, "bad_ret_num", serde_json::json!({})).unwrap_err();
    assert!(err.contains("must return string"), "got: {err}");
}

#[test]
fn handler_nil_without_jobs_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = r#"maki.api.register_tool({
        name = "nil_no_jobs",
        description = "returns nil without starting jobs",
        schema = { type = "object", properties = {} },
        audiences = { "main" },
        handler = function() return nil end
    })"#;
    host.load_source("nil_no_jobs", src).unwrap();
    let err = exec_tool(&reg, "nil_no_jobs", serde_json::json!({})).unwrap_err();
    assert!(err.contains(NIL_WITHOUT_JOBS_ERR), "got: {err}");
}

#[test]
fn handler_lua_error_surfaces_as_tool_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "thrower",
            description = "throws on call",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function() error("intentional kaboom") end
        }})"#,
    );
    host.load_source("thrower_plugin", &src).unwrap();

    let err = exec_tool(&reg, "thrower", serde_json::json!({})).unwrap_err();
    assert!(err.contains("intentional kaboom"), "got: {err}");
}

#[test]
fn lua_tool_schema_rejects_bad_input() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = r#"
maki.api.register_tool({
    name = "needs_name",
    description = "requires a name field",
    schema = {
        type = "object",
        properties = { name = { type = "string" } },
        required = { "name" }
    },
    handler = function(input) return input.name end
})
"#;
    host.load_source("schema_test", src).unwrap();

    let entry = reg.get("needs_name").unwrap();
    let err = entry
        .tool
        .parse(&serde_json::json!({"count": 1}))
        .err()
        .expect("missing required field should fail");
    assert!(err.to_string().contains("name"));

    assert!(
        entry
            .tool
            .parse(&serde_json::json!({"name": "alice"}))
            .is_ok()
    );
}

#[test]
fn init_lua_with_require_registers_tools() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(lua_dir.join("tools")).unwrap();

    std::fs::write(
        lua_dir.join("tools/greet.lua"),
        r#"
local M = {}
function M.setup()
    maki.api.register_tool({
        name = "greet",
        description = "says hi",
        schema = { type = "object", properties = {}, additionalProperties = false },
        handler = function() return "hi" end
    })
end
return M
"#,
    )
    .unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local greet = require("tools.greet")
greet.setup()
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();

    assert!(reg.has("greet"));
    assert_eq!(reg.names().len(), 1);
}

#[test]
fn require_caches_modules() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(lua_dir.join("counter.lua"), "return { value = 42 }\n").unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local a = require("counter")
local b = require("counter")
assert(a == b, "require should return cached module")
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

#[test]
fn require_sandbox_escape_blocked() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(tmp.path().join("init.lua"), "require(\"../../escape\")\n").unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_plugin_file(&init_path)
        .expect_err("expected sandbox error");
    assert!(matches!(err, PluginError::Lua { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains("sandbox") || msg.contains("outside"),
        "got: {msg}"
    );
}

#[test]
fn require_circular_returns_sentinel_and_caches_real_value() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(
        lua_dir.join("a.lua"),
        "local b = require(\"b\")\nreturn { name = \"a\" }\n",
    )
    .unwrap();
    std::fs::write(
        lua_dir.join("b.lua"),
        "local a = require(\"a\")\nassert(a == true, \"circular require should return sentinel\")\nreturn { name = \"b\" }\n",
    )
    .unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
require("a")
local a2 = require("a")
assert(type(a2) == "table", "cached value should be table, got: " .. type(a2))
assert(a2.name == "a", "cached value should have name='a'")
local b2 = require("b")
assert(type(b2) == "table", "cached value should be table, got: " .. type(b2))
assert(b2.name == "b", "cached value should have name='b'")
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

#[test]
fn require_nonexistent_module_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(tmp.path().join("init.lua"), "require(\"nonexistent\")\n").unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_plugin_file(&init_path)
        .expect_err("expected error for missing module");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains("nonexistent"), "got: {err}");
}

#[test]
fn require_error_cleans_loading_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(lua_dir.join("bad.lua"), "error('deliberate')").unwrap();
    std::fs::write(lua_dir.join("good.lua"), "return { ok = true }").unwrap();

    std::fs::write(
        tmp.path().join("init.lua"),
        r#"
local ok, err = pcall(require, "bad")
assert(not ok, "bad module should fail")

-- second require of the same broken module must error again, not return a sentinel
local ok2, err2 = pcall(require, "bad")
assert(not ok2, "broken module should fail on retry too")

-- unrelated modules must still work
local g = require("good")
assert(type(g) == "table", "good module should load, got: " .. type(g))
assert(g.ok == true)
"#,
    )
    .unwrap();

    let init_path = tmp.path().join("init.lua");
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_plugin_file(&init_path).unwrap();
}

#[test]
fn multi_tool_plugin_registers_and_unloads_all() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"
maki.api.register_tool({{
    name = "multi_alpha",
    description = "first tool",
    schema = {MINIMAL_SCHEMA},
    handler = function() return "alpha" end
}})
maki.api.register_tool({{
    name = "multi_beta",
    description = "second tool",
    schema = {MINIMAL_SCHEMA},
    handler = function() return "beta" end
}})
"#,
    );
    host.load_source("multi", &src).unwrap();

    assert!(reg.has("multi_alpha"));
    assert!(reg.has("multi_beta"));

    host.unload("multi").unwrap();
    assert!(!reg.has("multi_alpha"));
    assert!(!reg.has("multi_beta"));
}

#[test]
fn conflict_from_different_plugin_preserves_original() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "evolving",
            description = "version 1",
            schema = {MINIMAL_SCHEMA},
            handler = function() return "v1" end
        }})"#,
    );
    host.load_source("keeper", &src).unwrap();
    assert!(reg.has("evolving"));

    let err = host
        .load_source("intruder", &src)
        .expect_err("expected conflict");
    assert!(matches!(err, PluginError::NameConflict { .. }));

    let entry = reg.get("evolving").unwrap();
    assert!(matches!(entry.source, ToolSource::Lua { ref plugin } if plugin.as_ref() == "keeper"),);
}

#[test]
fn ctx_finish_called_twice_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "double_finish",
            description = "calls finish twice",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:finish("first")
                ctx:finish("second")
            end
        }})"#,
    );
    host.load_source("double_finish", &src).unwrap();
    let err = exec_tool(&reg, "double_finish", serde_json::json!({})).unwrap_err();
    assert!(err.contains(FINISH_CALLED_TWICE_ERR), "got: {err}");
}

#[test]
fn ctx_finish_with_is_error_propagates() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "finish_err",
            description = "finishes with error",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:finish({{ llm_output = "async boom", is_error = true }})
            end
        }})"#,
    );
    host.load_source("finish_err", &src).unwrap();
    let err = exec_tool(&reg, "finish_err", serde_json::json!({})).unwrap_err();
    assert_eq!(err, "async boom");
}

#[test]
fn async_job_on_exit_receives_exit_code() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_exit_code",
            description = "reports exit code",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                maki.fn.jobstart("exit 42", {{
                    on_exit = function(job_id, code)
                        ctx:finish("code=" .. tostring(code))
                    end
                }})
            end
        }})"#,
    );
    host.load_source("job_exit_code", &src).unwrap();
    let out = exec_tool(&reg, "job_exit_code", serde_json::json!({})).unwrap();
    assert_eq!(out, "code=42");
}

#[test]
fn jobstart_invalid_cwd_errors_with_expanded_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_bad_cwd",
            description = "jobstart with missing tilde cwd",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local _, err = pcall(maki.fn.jobstart, "pwd", {{ cwd = "{JOB_BAD_CWD}" }})
                return tostring(err)
            end
        }})"#,
    );
    host.load_source("job_bad_cwd", &src).unwrap();
    let out = exec_tool(&reg, "job_bad_cwd", serde_json::json!({})).unwrap();
    let expanded = maki_storage::paths::home()
        .expect("home dir")
        .join(JOB_BAD_CWD.strip_prefix("~/").unwrap());
    let expected = format!("{JOB_BAD_CWD_ERR_PREFIX}{}", expanded.display());
    assert!(out.contains(&expected), "got: {out}");
}

#[test]
fn async_job_exits_without_finish_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_no_finish",
            description = "job exits but never calls finish",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                maki.fn.jobstart("echo oops", {{
                    on_exit = function(job_id, code) end
                }})
            end
        }})"#,
    );
    host.load_source("job_no_finish", &src).unwrap();
    let err = exec_tool(&reg, "job_no_finish", serde_json::json!({})).unwrap_err();
    assert!(err.contains(NIL_WITHOUT_JOBS_ERR), "got: {err}");
}

#[test]
fn async_job_callback_error_surfaces() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_cb_err",
            description = "callback throws",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                maki.fn.jobstart("echo trigger", {{
                    on_exit = function(job_id, code)
                        error("callback exploded")
                    end
                }})
            end
        }})"#,
    );
    host.load_source("job_cb_err", &src).unwrap();
    let err = exec_tool(&reg, "job_cb_err", serde_json::json!({})).unwrap_err();
    assert!(err.contains("callback exploded"), "got: {err}");
}

#[test]
fn live_click_reaches_running_tool() {
    const LIVE_CLICK_ID: &str = "live-click-1";
    const CLICKED_MSG: &str = "clicked";
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "live_click",
            description = "finishes when clicked while running",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local buf = maki.ui.buf()
                buf:on("click", function()
                    ctx:finish("{CLICKED_MSG}")
                end)
                maki.fn.jobstart("sleep 30", {{}})
            end
        }})"#,
    );
    host.load_source("live_click", &src).unwrap();
    let eh = host.event_handle().expect("event handle available");
    let entry = reg.get("live_click").expect("tool registered");
    let inv = entry.tool.parse(&serde_json::json!({})).expect("parse");
    let worker = std::thread::spawn(move || {
        let ctx = maki_agent::tools::test_support::stub_ctx_with(
            &maki_agent::AgentMode::Build,
            None,
            Some(LIVE_CLICK_ID),
        );
        smol::block_on(inv.execute(&ctx)).output
    });
    for _ in 0..500 {
        if worker.is_finished() {
            break;
        }
        eh.request_click(LIVE_CLICK_ID.to_owned());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = worker.join().expect("worker thread").expect("tool output");
    match out {
        maki_agent::ToolOutput::Plain(s) => assert_eq!(s.text, CLICKED_MSG),
        other => panic!("unexpected output: {other:?}"),
    }
}

#[test]
fn jobstop_kills_running_job() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_stop",
            description = "starts and immediately stops a job",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local id = maki.fn.jobstart("sleep 60", {{
                    on_exit = function(job_id, code)
                        ctx:finish("killed=" .. tostring(code ~= 0))
                    end
                }})
                maki.fn.jobstop(id)
            end
        }})"#,
    );
    host.load_source("job_stop", &src).unwrap();
    let out = exec_tool(&reg, "job_stop", serde_json::json!({})).unwrap();
    assert_eq!(out, "killed=true");
}

#[test]
fn vm_recovers_after_async_job_tool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"
maki.api.register_tool({{
    name = "async_first",
    description = "async tool",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function(input, ctx)
        maki.fn.jobstart("echo hi", {{
            on_exit = function(job_id, code) ctx:finish("ok1") end
        }})
    end
}})
maki.api.register_tool({{
    name = "sync_after",
    description = "sync tool",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function() return "ok2" end
}})
"#,
    );
    host.load_source("recovery", &src).unwrap();
    let out1 = exec_tool(&reg, "async_first", serde_json::json!({})).unwrap();
    assert_eq!(out1, "ok1");
    let out2 = exec_tool(&reg, "sync_after", serde_json::json!({})).unwrap();
    assert_eq!(out2, "ok2");
}

#[test]
fn setup_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            "maki.setup({ agent = { bash_timeout_secs = 120 } })".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap();
    let raw = raw.expect("expected Some(RawConfig)");
    assert_eq!(raw.agent.bash_timeout_secs, Some(120));
}

#[test_case::test_case(
    "maki.setup({ ui = { splash_animaton = false } })",
    UNKNOWN_FIELD_ERR
    ; "unknown_field"
)]
#[test_case::test_case(
    r#"maki.setup({ agent = { bash_timeout_secs = "not a number" } })"#,
    ""
    ; "wrong_type"
)]
fn setup_rejects_bad_input(lua_src: &str, expected_substr: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .send_run_init_lua(lua_src.to_owned(), "test_init.lua".to_owned(), None)
        .expect_err("expected error");
    assert!(matches!(err, PluginError::Lua { .. }), "got: {err}");
    if !expected_substr.is_empty() {
        assert!(err.to_string().contains(expected_substr), "got: {err}");
    }
}

#[test]
fn setup_double_call_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .send_run_init_lua(
            "maki.setup({})\nmaki.setup({})".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .expect_err("expected error for double setup");
    assert!(err.to_string().contains(ALREADY_CALLED_ERR), "got: {err}");
}

#[test]
fn setup_not_called_returns_none() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            "-- no setup call".to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap();
    assert!(raw.is_none());
}

#[test]
fn setup_all_sections_at_once() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            r#"maki.setup({
                always_yolo = true,
                always_fast = true,
                always_thinking = "adaptive",
                ui = { splash_animation = false, mouse_scroll_lines = 5 },
                agent = { bash_timeout_secs = 120, max_output_lines = 9000 },
                provider = { default_model = "anthropic/claude-opus-4-6" },
                storage = { max_log_files = 3 },
                index = { max_file_size_mb = 8 },
                tools = { bash = { enabled = true }, websearch = { enabled = false } },
            })"#
            .to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.always_yolo, Some(true));
    assert_eq!(raw.always_fast, Some(true));
    assert_eq!(
        raw.always_thinking,
        Some(AlwaysThinking::Mode("adaptive".into()))
    );
    assert_eq!(raw.ui.splash_animation, Some(false));
    assert_eq!(raw.ui.mouse_scroll_lines, Some(5));
    assert_eq!(raw.agent.bash_timeout_secs, Some(120));
    assert_eq!(raw.agent.max_output_lines, Some(9000));
    assert_eq!(
        raw.provider.default_model.as_deref(),
        Some("anthropic/claude-opus-4-6")
    );
    assert_eq!(raw.storage.max_log_files, Some(3));
    assert_eq!(raw.index.max_file_size_mb, Some(8));
    assert_eq!(raw.tools["bash"].enabled, Some(true));
    assert_eq!(raw.tools["websearch"].enabled, Some(false));
}

#[test_case::test_case("true", AlwaysThinking::Toggle(true) ; "bool")]
#[test_case::test_case("8192", AlwaysThinking::Budget(8192) ; "number")]
#[test_case::test_case("\"adaptive\"", AlwaysThinking::Mode("adaptive".into()) ; "string")]
fn setup_always_thinking_variants(lua_val: &str, expected: AlwaysThinking) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let raw = host
        .send_run_init_lua(
            format!("maki.setup({{ always_thinking = {lua_val} }})"),
            "test_init.lua".to_owned(),
            None,
        )
        .unwrap()
        .expect("expected Some(RawConfig)");
    assert_eq!(raw.always_thinking, Some(expected));
}

#[test]
fn setup_no_tool_registration_in_init_env() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .send_run_init_lua(
            r#"maki.register_tool({
                name = "sneaky",
                description = "should fail",
                audiences = { "main" },
                handler = function() return "nope" end
            })"#
            .to_owned(),
            "test_init.lua".to_owned(),
            None,
        )
        .expect_err("register_tool should not be available in init.lua env");
    assert!(
        matches!(err, PluginError::Lua { .. }),
        "expected Lua error, got: {err}"
    );
}

#[test]
fn register_command_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "cmd_plugin",
        r#"
        maki.api.register_command({
            name = "/hello",
            description = "says hello",
            handler = function(args) end,
        })
        "#,
    )
    .unwrap();

    let reader = host.command_reader();
    let snap = reader.load();
    assert_eq!(snap.commands.len(), 1);
    assert_eq!(snap.commands[0].name.as_ref(), "/hello");
    assert_eq!(snap.commands[0].description.as_ref(), "says hello");
    assert_eq!(snap.commands[0].plugin.as_ref(), "cmd_plugin");
}

#[test_case::test_case(
    r#"maki.api.register_command({ name = "", handler = function() end })"#,
    "non-empty" ; "empty_name"
)]
#[test_case::test_case(
    r#"maki.api.register_command({ name = "/test", description = "no handler" })"#,
    "handler" ; "missing_handler"
)]
fn register_command_validation_rejects(src: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let err = host
        .load_source("bad_cmd", src)
        .expect_err("expected validation error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains(expected_err), "got: {err}");
}

#[test]
fn reload_replaces_commands() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "reload_cmd",
        r#"maki.api.register_command({ name = "/v1", handler = function() end })"#,
    )
    .unwrap();

    host.load_source(
        "reload_cmd",
        r#"maki.api.register_command({ name = "/v2", handler = function() end })"#,
    )
    .unwrap();
    let snap = host.command_reader().load();
    assert_eq!(snap.commands.len(), 1);
    assert_eq!(snap.commands[0].name.as_ref(), "/v2");
}

#[test]
fn unload_clears_commands() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(
        "cmd_only",
        r#"maki.api.register_command({ name = "/bye", handler = function() end })"#,
    )
    .unwrap();
    assert_eq!(host.command_reader().load().commands.len(), 1);

    host.unload("cmd_only").unwrap();
    assert_eq!(host.command_reader().load().commands.len(), 0);
}

#[test]
fn job_callback_finishes_after_handler_returns_nil() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "job_after_return",
            description = "on_exit finishes after handler returns nil",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                maki.fn.jobstart("true", {{
                    on_exit = function(_, code)
                        ctx:finish("exit=" .. tostring(code))
                    end,
                }})
                return nil
            end
        }})"#,
    );
    host.load_source("job_after_return", &src).unwrap();
    let out = exec_tool(&reg, "job_after_return", serde_json::json!({})).unwrap();
    assert_eq!(out, "exit=0");
}

#[test]
fn ctx_set_deadline_times_out() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "deadline_test",
            description = "uses ctx:set_deadline",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:set_deadline(2)
                maki.fn.jobstart("sleep 30", {{
                    on_exit = function(_, _) ctx:finish("should-not-reach") end,
                }})
                return nil
            end
        }})"#,
    );
    host.load_source("deadline_test", &src).unwrap();
    let err = exec_tool(&reg, "deadline_test", serde_json::json!({})).unwrap_err();
    assert!(err.contains(TIMED_OUT_SUBSTR), "got: {err}");
}

#[test]
fn ctx_set_deadline_twice_errors() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "deadline_twice",
            description = "calls set_deadline twice",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                ctx:set_deadline(5)
                ctx:set_deadline(5)
            end
        }})"#,
    );
    host.load_source("deadline_twice", &src).unwrap();
    let err = exec_tool(&reg, "deadline_twice", serde_json::json!({})).unwrap_err();
    assert!(err.contains(DEADLINE_ALREADY_SET_ERR), "got: {err}");
}

#[test]
fn restore_tool_async_ordering_and_delivery() {
    let (_reg, host) = builtins_host();

    let input = serde_json::json!({"command": "echo ok", "timeout": 1});

    let handle = host.event_handle().expect("event handle available");
    let (tx, rx) = flume::unbounded();
    let event_tx = maki_agent::EventSender::new(tx, 0);

    let bash_item = |id: &str| maki_lua::RestoreItem {
        tool: Arc::from("bash"),
        tool_use_id: id.to_owned(),
        output: "tool bash timed out after 1s".to_owned(),
        input: input.clone(),
        is_error: true,
        tool_output_lines: ToolOutputLines::default(),
        theme_gen: None,
        expanded: false,
    };
    let unknown_item = maki_lua::RestoreItem {
        tool: Arc::from("definitely_not_a_tool"),
        tool_use_id: "unknown_id".to_owned(),
        output: "ignored".to_owned(),
        input: serde_json::json!({}),
        is_error: false,
        tool_output_lines: ToolOutputLines::default(),
        theme_gen: None,
        expanded: false,
    };

    handle.request_restore(unknown_item, event_tx.clone());
    handle.request_restore(bash_item("a"), event_tx.clone());
    handle.request_restore(bash_item("b"), event_tx.clone());

    let _ = handle.collect_prompt_slots();

    let snapshots: Vec<maki_agent::Envelope> = rx.drain().collect();

    let tool_ids: Vec<&str> = snapshots
        .iter()
        .filter_map(|env| match &env.event {
            maki_agent::AgentEvent::ToolSnapshot { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();

    assert!(
        !tool_ids.contains(&"unknown_id"),
        "unknown tool should emit no snapshots"
    );
    assert!(
        tool_ids.contains(&"a"),
        "known tool 'a' should emit snapshot"
    );
    assert!(
        tool_ids.contains(&"b"),
        "known tool 'b' should emit snapshot"
    );
}

#[test_case::test_case(
    "write",
    serde_json::json!({"path": "/tmp/x.md", "content": "alpha\nbeta"}),
    "wrote 10 bytes to /tmp/x.md",
    &["alpha", "beta"]
    ; "write_tool_restores_file_content"
)]
#[test_case::test_case(
    "memory",
    serde_json::json!({"command": "write", "path": "n.md", "content": "gamma"}),
    "wrote n.md (1 lines)",
    &["gamma"]
    ; "memory_write_restores_saved_content"
)]
fn restore_rebuilds_body_from_input_content(
    tool: &str,
    input: serde_json::Value,
    summary: &str,
    expected: &[&str],
) {
    let (_reg, host) = builtins_host();
    let handle = host.event_handle().expect("event handle available");
    let (tx, rx) = flume::unbounded();

    handle.request_restore(
        maki_lua::RestoreItem {
            tool: Arc::from(tool),
            tool_use_id: "restore_id".to_owned(),
            output: summary.to_owned(),
            input,
            is_error: false,
            tool_output_lines: ToolOutputLines::default(),
            theme_gen: None,
            expanded: true,
        },
        maki_agent::EventSender::new(tx, 0),
    );
    let _ = handle.collect_prompt_slots();

    let mut text = String::new();
    for env in rx.drain() {
        if let maki_agent::AgentEvent::ToolSnapshot { snapshot, .. } = env.event {
            for line in snapshot.lines.iter() {
                for span in &line.spans {
                    text.push_str(&span.text);
                }
            }
        }
    }

    for needle in expected {
        assert!(
            text.contains(needle),
            "restored body missing '{needle}', got: {text}"
        );
    }
    assert!(
        !text.contains(summary),
        "restored body should show content, not the summary: {text}"
    );
}

/// Guards the stale-cancelled-handle bug: `permission_scopes` must call
/// the plugin callback and return parsed scopes, not fall back to raw JSON.
/// A leaked `{"command":...}` scope would break allow rules.
#[test_case::test_case("git status" ; "parseable command")]
#[test_case::test_case("echo 'unterminated" ; "unparseable command")]
fn bash_permission_scopes_never_falls_back_to_json(command: &str) {
    let (reg, _host) = builtins_host();

    let input = serde_json::json!({ "command": command });
    let entry = reg.get("bash").expect("bash registered");
    let inv = entry.tool.parse(&input).expect("parse failed");
    let scopes = smol::block_on(inv.permission_scopes())
        .expect("permission_scopes returned None (would fall back to raw JSON)");

    assert!(
        !scopes.scopes.iter().any(|s| s.contains("\"command\"")),
        "fell back to raw JSON scope: {:?}",
        scopes.scopes
    );
}

fn exec_tool_with_perms(
    perms: maki_lua::PluginPermissions,
    src: &str,
    tool: &str,
    input: serde_json::Value,
) -> Result<String, String> {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source_with_permissions("perm_test", src, perms)
        .unwrap();
    exec_tool(&reg, tool, input)
}

fn perm_tool_src(name: &str, handler_body: &str) -> String {
    format!(
        r#"maki.api.register_tool({{
            name = "{name}",
            description = "d",
            schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
            handler = function(input, ctx)
                {handler_body}
            end,
        }})"#
    )
}

#[test_case::test_case(
    "read_deny",
    r#"local ok, err = pcall(function() maki.fs.read("/etc/hostname") end)
                return tostring(err)"#,
    "fs_read"
    ; "fs_read_denied"
)]
#[test_case::test_case(
    "write_deny",
    r#"local ok, err = pcall(function() maki.fs.write("/tmp/test", "x") end)
                return tostring(err)"#,
    "fs_write"
    ; "fs_write_denied"
)]
#[test_case::test_case(
    "run_deny",
    r#"local ok, err = pcall(function() maki.fn.jobstart("echo hi") end)
                return tostring(err)"#,
    "run"
    ; "run_denied"
)]
fn denied_permission_blocks_api(tool_name: &str, handler_body: &str, expected_perm: &str) {
    let src = perm_tool_src(tool_name, handler_body);
    let result = exec_tool_with_perms(
        maki_lua::PluginPermissions::denied(),
        &src,
        tool_name,
        serde_json::json!({}),
    )
    .unwrap();
    assert!(result.contains(PERMISSION_DENIED_MSG), "got: {result}");
    assert!(result.contains(expected_perm), "got: {result}");
}

#[test]
fn user_plugin_with_fs_read_can_read_but_not_write() {
    let src = perm_tool_src(
        "rw_test",
        r#"local read_ok = pcall(function() maki.fs.read("/dev/null") end)
                local write_ok = pcall(function() maki.fs.write("/tmp/test", "x") end)
                return "read=" .. tostring(read_ok) .. ",write=" .. tostring(write_ok)"#,
    );
    let mut perms = maki_lua::PluginPermissions::denied();
    perms.set(maki_lua::Permission::FsRead, true);
    let result = exec_tool_with_perms(perms, &src, "rw_test", serde_json::json!({})).unwrap();
    assert!(result.contains("read=true"), "got: {result}");
    assert!(result.contains("write=false"), "got: {result}");
}

#[test]
fn builtin_plugin_has_all_permissions() {
    let src = perm_tool_src(
        "trusted_test",
        r#"local cwd_ok = pcall(function() maki.uv.cwd() end)
                local env_ok = pcall(function() maki.env.state_dir() end)
                return "cwd=" .. tostring(cwd_ok) .. ",env=" .. tostring(env_ok)"#,
    );
    let result = exec_tool_with_perms(
        maki_lua::PluginPermissions::trusted(),
        &src,
        "trusted_test",
        serde_json::json!({}),
    )
    .unwrap();
    assert!(result.contains("cwd=true"), "got: {result}");
    assert!(result.contains("env=true"), "got: {result}");
}

#[test]
fn env_permission_guards_uv_and_env() {
    let src = perm_tool_src(
        "env_guard_test",
        r#"local cwd_ok = pcall(function() maki.uv.cwd() end)
                local home_ok = pcall(function() maki.uv.os_homedir() end)
                local env_ok = pcall(function() maki.env.state_dir() end)
                local exec_ok = pcall(function() maki.fn.executable("ls") end)
                return "cwd=" .. tostring(cwd_ok) .. ",home=" .. tostring(home_ok) .. ",env=" .. tostring(env_ok) .. ",exec=" .. tostring(exec_ok)"#,
    );
    let result = exec_tool_with_perms(
        maki_lua::PluginPermissions::denied(),
        &src,
        "env_guard_test",
        serde_json::json!({}),
    )
    .unwrap();
    assert!(result.contains("cwd=false"), "got: {result}");
    assert!(result.contains("home=false"), "got: {result}");
    assert!(result.contains("env=false"), "got: {result}");
    assert!(result.contains("exec=false"), "got: {result}");
}

const PATH_FIELD_SCHEMA: &str = r#"{
    type = "object",
    properties = { path = { type = "string" } },
    required = { "path" },
}"#;

#[test_case::test_case(STRING_FIELD_SCHEMA, "nonexistent" ; "missing_field")]
#[test_case::test_case(NON_STRING_FIELD_SCHEMA, "count" ; "non_string_field")]
fn mutable_path_invalid_rejected(schema: &str, scope_field: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "bad_mpath",
            description = "test",
            schema = {schema},
            mutable_path = "{scope_field}",
            handler = function() return "" end
        }})"#,
    );
    let err = host
        .load_source("bad_mpath_plugin", &src)
        .expect_err("expected error for invalid mutable_path");

    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(
        err.to_string().contains("mutable_path")
            && err.to_string().contains(INVALID_PERMISSION_SCOPE_ERR),
        "got: {err}"
    );
}

#[test]
fn mutable_path_returns_path_from_input() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "mp_read",
            description = "test",
            schema = {PATH_FIELD_SCHEMA},
            mutable_path = "path",
            handler = function() return "" end
        }})"#,
    );
    host.load_source("mp_read_plugin", &src).unwrap();

    let entry = reg.get("mp_read").expect("tool not registered");
    let inv = entry
        .tool
        .parse(&serde_json::json!({ "path": "/tmp/foo.txt" }))
        .expect("parse failed");
    assert_eq!(inv.mutable_path(), Some(Path::new("/tmp/foo.txt")));
}

#[test]
fn pure_functions_not_guarded() {
    let src = perm_tool_src(
        "pure_test",
        r#"local dirname_ok = pcall(function() maki.fs.dirname("/foo/bar") end)
                local basename_ok = pcall(function() maki.fs.basename("/foo/bar") end)
                local json_ok = pcall(function() maki.json.encode({a=1}) end)
                return "dirname=" .. tostring(dirname_ok) .. ",basename=" .. tostring(basename_ok) .. ",json=" .. tostring(json_ok)"#,
    );
    let result = exec_tool_with_perms(
        maki_lua::PluginPermissions::denied(),
        &src,
        "pure_test",
        serde_json::json!({}),
    )
    .unwrap();
    assert!(result.contains("dirname=true"), "got: {result}");
    assert!(result.contains("basename=true"), "got: {result}");
    assert!(result.contains("json=true"), "got: {result}");
}

#[test]
fn runaway_allocation_hits_memory_limit_instead_of_oom() {
    const LIMITED: &str = "limited";
    let src = r#"
        local ok, err = pcall(function()
            local t = {}
            local chunk = string.rep("x", 1024 * 1024)
            while true do
                t[#t + 1] = chunk .. tostring(#t)
            end
        end)
        if ok then error("expected allocation to fail under the memory limit") end
        if not string.find(tostring(err), "memory") then
            error("expected an out-of-memory error, got: " .. tostring(err))
        end
    "#;
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(LIMITED, src)
        .expect("plugin should hit the memory limit and recover, not crash the process");
}

#[test]
fn start_hook_publishes_live_buf_for_tool_use_id() {
    let (reg, _host) = start_hook_fixture();
    let rx = run_start(&reg, "st_tool", serde_json::json!({"code": "line1\nline2"}));
    let body = recv_live_buf(&rx, START_TOOL_USE_ID).expect("start must publish a LiveToolBuf");
    let text = body.take().text();
    assert!(text.contains("line1"), "preview must render input: {text}");
}

#[test]
fn start_hook_error_does_not_fail_tool() {
    let (reg, _host) = start_hook_fixture();
    let _rx = run_start(&reg, "st_boom", serde_json::json!({"code": "x"}));
    let out = exec_tool(&reg, "st_boom", serde_json::json!({"code": "x"})).expect("handler ok");
    assert_eq!(out, "handled");
}

#[test]
fn start_skipped_for_tool_without_start_fn() {
    let (reg, _host) = start_hook_fixture();
    let rx = run_start(&reg, "st_plain", serde_json::json!({"code": "x"}));
    assert!(
        recv_live_buf(&rx, START_TOOL_USE_ID).is_none(),
        "no start fn must mean no preview"
    );
}

/// `start` runs before permission checks, so its ctx is not a `LuaCtx` and
/// `maki.agent.call_tool` (which borrows `LuaCtx`) rejects it outright.
#[test]
fn start_ctx_cannot_dispatch_tools() {
    let (reg, _host) = start_hook_fixture();
    let rx = run_start(&reg, "st_probe", serde_json::json!({"code": "x"}));
    let body = recv_live_buf(&rx, START_TOOL_USE_ID).expect("probe publishes a buf");
    let text = body.take().text();
    assert_eq!(
        text, "call_tool_rejected finish_missing set_deadline_missing",
        "StartCtx must expose no dispatch/finish/deadline capability"
    );
}

const START_TOOL_USE_ID: &str = "start-tu-1";

fn start_hook_fixture() -> (Arc<ToolRegistry>, PluginHost) {
    let src = format!(
        r#"
local function preview(input, ctx)
    local buf = maki.ui.buf()
    buf:set_lines({{ input.code }})
    ctx:live_buf(buf)
end
maki.api.register_tool({{
    name = "st_tool",
    description = "test",
    schema = {CODE_SCHEMA},
    start = preview,
    handler = function(input, ctx) return "handled" end,
}})
maki.api.register_tool({{
    name = "st_boom",
    description = "test",
    schema = {CODE_SCHEMA},
    start = function(input, ctx) error("boom") end,
    handler = function(input, ctx) return "handled" end,
}})
maki.api.register_tool({{
    name = "st_plain",
    description = "test",
    schema = {CODE_SCHEMA},
    handler = function(input, ctx) return "handled" end,
}})
maki.api.register_tool({{
    name = "st_probe",
    description = "test",
    schema = {CODE_SCHEMA},
    start = function(input, ctx)
        local parts = {{}}
        local ok = pcall(function() return maki.agent.call_tool(ctx, "st_plain", {{ code = "x" }}) end)
        parts[1] = ok and "call_tool_allowed" or "call_tool_rejected"
        parts[2] = ctx.finish == nil and "finish_missing" or "finish_present"
        parts[3] = ctx.set_deadline == nil and "set_deadline_missing" or "set_deadline_present"
        local buf = maki.ui.buf()
        buf:set_lines({{ table.concat(parts, " ") }})
        ctx:live_buf(buf)
    end,
    handler = function(input, ctx) return "handled" end,
}})
"#
    );
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("start_hooks", &src).unwrap();
    (reg, host)
}

/// `start` is awaited to completion, so the returned receiver already holds
/// everything the hook emitted.
fn run_start(
    reg: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
) -> flume::Receiver<maki_agent::Envelope> {
    let (tx, rx) = flume::unbounded::<maki_agent::Envelope>();
    let event_tx = maki_agent::EventSender::new(tx, 0);
    let ctx = maki_agent::tools::test_support::stub_ctx_with(
        &maki_agent::AgentMode::Build,
        Some(&event_tx),
        Some(START_TOOL_USE_ID),
    );
    let inv = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"))
        .tool
        .parse(&input)
        .expect("parse failed");
    smol::block_on(inv.start(&ctx));
    rx
}

fn recv_live_buf(
    rx: &flume::Receiver<maki_agent::Envelope>,
    id: &str,
) -> Option<Arc<maki_agent::SharedBuf>> {
    rx.drain().find_map(|env| match env.event {
        maki_agent::AgentEvent::LiveToolBuf { id: got, body } if got == id => Some(body),
        _ => None,
    })
}

#[test]
fn start_annotation_timeout_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "sa_to",
            description = "test",
            schema = {TIMEOUT_SCHEMA},
            start_annotation = {{ field = "timeout", kind = "timeout" }},
            handler = function(input, ctx) return "" end
        }})"#,
    );
    host.load_source("sa_to_plugin", &src).unwrap();
    let entry = reg.get("sa_to").expect("tool not registered");
    let inv = entry
        .tool
        .parse(&serde_json::json!({"timeout": 90}))
        .expect("parse failed");
    assert_eq!(inv.start_annotation(), Some(timeout_annotation(90)));
}

#[test]
fn start_annotation_count_happy_path() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "sa_ct",
            description = "test",
            schema = {ARRAY_SCHEMA},
            start_annotation = "edits",
            handler = function(input, ctx) return "" end
        }})"#,
    );
    host.load_source("sa_ct_plugin", &src).unwrap();
    let entry = reg.get("sa_ct").expect("tool not registered");
    let inv = entry
        .tool
        .parse(&serde_json::json!({"edits": [1, 2, 3]}))
        .expect("parse failed");
    assert_eq!(inv.start_annotation(), Some("3 edits".to_owned()));
}

#[test_case::test_case(START_ANNOTATION_COUNT_NON_ARRAY_SRC, STRING_NAME_SCHEMA, "not in schema properties or not type 'array'" ; "start_annotation_count_non_array")]
fn registration_with_schema_rejects(fields: &str, schema: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            {fields},
            schema = {schema},
            handler = function(input, ctx) return "" end
        }})"#,
    );
    let err = host
        .load_source("schema_val_test", &src)
        .expect_err("expected validation error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains(expected_err), "got: {err}");
}

#[test]
fn interpreter_on_output_streams_lines() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "interp_stream",
            description = "streams interpreter output",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local lines = {{}}
                local result, err = maki.interpreter.run("print('a')\nprint('b')", {{
                    timeout = 10,
                    max_memory_mb = 50,
                    on_output = function(line)
                        table.insert(lines, line)
                    end,
                }})
                if err then return "err: " .. err end
                return table.concat(lines, "|") .. ";stdout=" .. (result.stdout or "")
            end
        }})"#,
    );
    host.load_source("interp_stream_plugin", &src).unwrap();
    let out = exec_tool(&reg, "interp_stream", serde_json::json!({})).unwrap();
    assert_eq!(out, "a|b;stdout=a\nb");
}

const SESSION_CLOSED_ERR: &str = "session closed";

fn interp_tool_plugin(name: &str, python: &str, tools_lua: &str) -> String {
    format!(
        r#"maki.api.register_tool({{
            name = "{name}",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local lines = {{}}
                local result, err = maki.interpreter.run("{python}", {{
                    timeout = 10,
                    max_memory_mb = 50,
                    on_output = function(line) table.insert(lines, line) end,
                    tools = {tools_lua},
                }})
                if err then return "err: " .. err end
                return table.concat(lines, "|")
            end
        }})"#
    )
}

#[test]
fn interpreter_tools_fn_map_kwargs_reach_lua_tool() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = interp_tool_plugin(
        "interp_tools",
        r"r = await greet(name='bob')\nprint(r)",
        "{ greet = function(input) return 'hi:' .. input.name end }",
    );
    host.load_source("interp_tools_plugin", &src).unwrap();
    let out = exec_tool(&reg, "interp_tools", serde_json::json!({})).unwrap();
    assert_eq!(out, "hi:bob");
}

#[test]
fn interpreter_tools_nil_err_pair_fails_call() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = interp_tool_plugin(
        "interp_err",
        r"await bad()",
        "{ bad = function(input) return nil, 'boom' end }",
    );
    host.load_source("interp_err_plugin", &src).unwrap();
    let out = exec_tool(&reg, "interp_err", serde_json::json!({})).unwrap();
    assert!(out.starts_with("err: "), "got: {out}");
    assert!(out.contains("boom"), "got: {out}");
}

#[test]
fn interpreter_tools_gather_resolves_parallel_batch() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = interp_tool_plugin(
        "interp_gather",
        r"import asyncio\nasync def main():\n    a, b = await asyncio.gather(t_a(), t_b())\n    print(a + '|' + b)\nawait main()",
        "{ t_a = function(input) return 'A' end, t_b = function(input) return 'B' end }",
    );
    host.load_source("interp_gather_plugin", &src).unwrap();
    let out = exec_tool(&reg, "interp_gather", serde_json::json!({})).unwrap();
    assert_eq!(out, "A|B");
}

#[test]
fn call_tool_resolves_lua_tool_and_reports_unknown() {
    let reg = Arc::clone(ToolRegistry::native_arc());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("echo_plugin", ECHO_PLUGIN).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "call_tool_probe",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local out, err = maki.agent.call_tool(ctx, "echo_", {{ msg = "hello" }})
                if err ~= nil then return "unexpected err: " .. err end
                local out2, err2 = maki.agent.call_tool(ctx, "no_such_tool_xyz", {{}})
                if out2 ~= nil then return "unexpected output: " .. out2 end
                if err2 == nil then return "expected err for unknown tool" end
                return out
            end
        }})"#
    );
    host.load_source("call_tool_plugin", &src).unwrap();
    let out = exec_tool_in(
        &reg,
        "call_tool_probe",
        serde_json::json!({}),
        Some(Arc::clone(&reg)),
    )
    .unwrap();
    assert_eq!(out, "hello");
    host.unload("call_tool_plugin").unwrap();
    host.unload("echo_plugin").unwrap();
}

#[test]
fn session_close_idempotent_and_prompt_after_close_errors() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "session_probe",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                local sess = maki.agent.session(ctx, {{}})
                sess:close()
                sess:close()
                local result, err = sess:prompt("x")
                if result ~= nil then return "unexpected result" end
                return err or "no error"
            end
        }})"#
    );
    host.load_source("session_plugin", &src).unwrap();
    let out = exec_tool(&reg, "session_probe", serde_json::json!({})).unwrap();
    assert_eq!(out, SESSION_CLOSED_ERR);
}

#[test_case::test_case("{ audience = 'wurkflow' }", "unknown audience: wurkflow" ; "unknown_audience")]
#[test_case::test_case("{ local_tools = { foo = { handler = function() return '' end } } }", "local_tools.foo: 'description' is required" ; "local_tool_missing_description")]
#[test_case::test_case("{ local_tools = { foo = { description = 'd' } } }", "local_tools.foo: 'handler' is required" ; "local_tool_missing_handler")]
fn session_opts_validation_rejects(opts: &str, expected: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let src = format!(
        r#"maki.api.register_tool({{
            name = "session_opts_probe",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                maki.agent.session(ctx, {opts})
                return "no error"
            end
        }})"#
    );
    host.load_source("session_opts_plugin", &src).unwrap();
    let err = exec_tool(&reg, "session_opts_probe", serde_json::json!({})).unwrap_err();
    assert!(err.contains(expected), "got: {err}");
}

fn load_img_tool(host: &PluginHost) {
    let src = format!(
        r#"maki.api.register_tool({{
            name = "img_probe",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{ "main" }},
            handler = function(input, ctx)
                return {{
                    llm_output = "[image: test 1x1]",
                    image = {{ media_type = "image/png", data = "aGVsbG8=" }},
                }}
            end
        }})"#
    );
    host.load_source("img_plugin", &src).unwrap();
}

#[test]
fn lua_tool_image_reply_maps_to_image_output() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    load_img_tool(&host);
    let out = exec_tool_output(&reg, "img_probe", serde_json::json!({})).unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Png);
    assert_eq!(&*source.data, "aGVsbG8=");
    assert_eq!(text, "[image: test 1x1]");
}

#[test]
fn view_image_tool_returns_image_output() {
    use base64::Engine as _;

    let (reg, _host) = builtins_host();

    // The code_execution bridge flattens output to text, so view_image is
    // pointless from the interpreter.
    let audience = reg.get("view_image").unwrap().tool.audience();
    assert!(audience.contains(maki_agent::tools::ToolAudience::MAIN));
    assert!(!audience.contains(maki_agent::tools::ToolAudience::INTERPRETER));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.png");
    let img = image::DynamicImage::new_rgb8(4, 2);
    img.save_with_format(&path, image::ImageFormat::Png)
        .unwrap();

    let out = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Png);
    assert!(text.contains("tiny.png"), "caption: {text}");
    assert!(text.contains("4x2"), "caption: {text}");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&*source.data)
        .unwrap();
    assert_eq!(decoded, std::fs::read(&path).unwrap());
}

#[test]
fn view_image_tool_rejects_non_image() {
    let (reg, _host) = builtins_host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "plain text").unwrap();
    let err = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap_err();
    assert!(err.contains("not an image"), "got: {err}");
}

fn probe_output(data: &str) -> (image::ImageFormat, u32, u32) {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    let reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .unwrap();
    let format = reader.format().unwrap();
    let (w, h) = reader.into_dimensions().unwrap();
    (format, w, h)
}

#[test]
fn view_image_downscales_oversized_png_with_honest_caption() {
    let (reg, _host) = builtins_host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wide.png");
    image::DynamicImage::new_rgb8(2000, 100)
        .save_with_format(&path, image::ImageFormat::Png)
        .unwrap();

    let out = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Png);
    assert!(text.contains("downscaled from 2000x100"), "caption: {text}");

    let (format, w, h) = probe_output(&source.data);
    assert_eq!(format, image::ImageFormat::Png);
    assert_eq!(w, 1568, "long edge must land exactly on the API limit");
    assert!(h <= 79, "aspect ratio broken: {w}x{h}");
    // Caption must report the dimensions actually shipped, not the original.
    assert!(text.contains(&format!("{w}x{h}")), "caption: {text}");
}

#[test]
fn view_image_oversized_gif_reencodes_to_png_first_frame() {
    let (reg, _host) = builtins_host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("banner.gif");
    image::DynamicImage::new_rgb8(2000, 8)
        .save_with_format(&path, image::ImageFormat::Gif)
        .unwrap();

    let out = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    // gif encoding is unsupported, so downscaling forces png; the caption
    // must confess the downscale and the lost animation.
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Png);
    assert!(text.contains("downscaled from 2000x8"), "caption: {text}");
    assert!(text.contains("first frame only"), "caption: {text}");
    assert_eq!(probe_output(&source.data).0, image::ImageFormat::Png);
}

#[test]
fn view_image_small_gif_passes_through_unchanged() {
    use base64::Engine as _;

    let (reg, _host) = builtins_host();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.gif");
    image::DynamicImage::new_rgb8(4, 2)
        .save_with_format(&path, image::ImageFormat::Gif)
        .unwrap();

    let out = exec_tool_output(
        &reg,
        "view_image",
        serde_json::json!({"path": path.to_str().unwrap()}),
    )
    .unwrap();
    let maki_agent::ToolOutput::Image { source, text } = out else {
        panic!("expected Image output, got {out:?}");
    };
    assert_eq!(source.media_type, maki_agent::ImageMediaType::Gif);
    assert!(
        !text.contains("first frame only"),
        "pass-through keeps animation, caption must not claim otherwise: {text}"
    );
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&*source.data)
        .unwrap();
    assert_eq!(
        decoded,
        std::fs::read(&path).unwrap(),
        "under-limit gif must ship byte-identical, not re-encoded"
    );
}

#[test]
fn interpreter_bridge_flattens_image_with_visibility_note() {
    let reg = fresh_registry();
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    load_img_tool(&host);

    let mut ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    ctx.registry = Arc::clone(&reg);
    let out = smol::block_on(maki_agent::tools::interpreter_bridge::dispatch(
        &ctx,
        "img_probe",
        &serde_json::json!({}),
    ))
    .unwrap();
    assert!(out.starts_with("[image: test 1x1]"), "got: {out}");
    assert!(
        out.contains(maki_agent::tools::interpreter_bridge::IMAGE_NOT_VISIBLE_NOTE),
        "got: {out}"
    );
}
