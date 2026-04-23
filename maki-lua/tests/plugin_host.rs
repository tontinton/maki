use std::path::PathBuf;
use std::sync::Arc;

use maki_agent::tools::{ToolRegistry, ToolSource};
use maki_config::PluginsConfig;
use maki_lua::{PluginError, PluginHost};

fn enabled_config() -> PluginsConfig {
    PluginsConfig {
        enabled: true,
        builtins: vec![],
        init_file: None,
    }
}

fn init_config(init_file: PathBuf) -> PluginsConfig {
    PluginsConfig {
        enabled: true,
        builtins: vec![],
        init_file: Some(init_file),
    }
}

fn fresh_registry() -> Arc<ToolRegistry> {
    Arc::new(ToolRegistry::new())
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: serde_json::Value) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    smol::block_on(async { inv.execute(&ctx).await }).map(|out| match out {
        maki_agent::ToolOutput::Plain(s) => s,
        other => panic!("unexpected output: {other:?}"),
    })
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
const NON_STRING_FIELD_SCHEMA: &str = r#"{
    type = "object",
    properties = { count = { type = "integer" } },
    required = { "count" },
}"#;
const NIL_WITHOUT_JOBS_ERR: &str =
    "handler returned nil without calling ctx:finish() or starting jobs";
const FINISH_CALLED_TWICE_ERR: &str = "ctx:finish() already called";

#[test]
fn stdlib_globals_accessible() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
    host.load_source("echo_plugin", ECHO_PLUGIN).unwrap();

    let entry = reg.get("echo_").expect("echo_ tool not registered");
    assert_eq!(entry.tool.name(), "echo_");
    assert!(
        matches!(entry.source, ToolSource::Lua { ref plugin } if plugin.as_ref() == "echo_plugin"),
    );

    let out = exec_tool(&reg, "echo_", serde_json::json!({"msg": "hello"})).unwrap();
    assert_eq!(out, "hello");
}

#[test]
fn unload_round_trip() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

    host.load_source("unload_test", ECHO_PLUGIN).unwrap();
    assert!(reg.has("echo_"));

    host.unload("unload_test").unwrap();
    assert!(!reg.has("echo_"));

    host.load_source("unload_test", "").unwrap();
    assert!(!reg.has("echo_"));
}

#[test_case::test_case(BAD_NAME_SRC, "invalid name" ; "invalid_tool_name")]
#[test_case::test_case(EMPTY_DESC_SRC, "description must be non-empty" ; "empty_description")]
#[test_case::test_case(EMPTY_AUD_SRC, "audiences" ; "empty_audiences")]
fn registration_validation_rejects(fields: &str, expected_err: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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
fn interrupt_kills_infinite_loop_and_vm_recovers() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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

    assert!(result.is_err(), "expected error from timed-out loop");

    let ok = exec_tool(&reg, "noop_after_loop", serde_json::json!({}));
    assert!(ok.is_ok(), "VM poisoned after interrupt: {ok:?}");
}

#[test]
fn reload_same_plugin_replaces_tools() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

    host.load_source("p1", ECHO_PLUGIN).unwrap();
    assert!(reg.has("echo_"));

    host.load_source("p1", ECHO_PLUGIN)
        .expect("reload with same plugin name should succeed");
    assert!(reg.has("echo_"));
}

#[test]
fn failed_load_leaves_no_tools() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"
maki.api.register_tool({{
    name = "doomed",
    description = "never registered",
    schema = {MINIMAL_SCHEMA},
    audiences = {{ "main" }},
    handler = function() return "" end
}})
error("plugin blew up after register")
"#,
    );
    let err = host
        .load_source("broken", &src)
        .expect_err("expected lua error");
    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(!reg.has("doomed"));

    host.load_source("broken", ECHO_PLUGIN)
        .expect("retry with good source should succeed");
    assert!(reg.has("echo_"));
}

#[test]
fn is_error_propagated_as_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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

    let config = init_config(tmp.path().join("init.lua"));
    let reg = fresh_registry();
    let _host = PluginHost::new(&config, Arc::clone(&reg)).unwrap();

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

    let config = init_config(tmp.path().join("init.lua"));
    let reg = fresh_registry();
    let _host = PluginHost::new(&config, Arc::clone(&reg)).unwrap();
}

#[test]
fn require_sandbox_escape_blocked() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(tmp.path().join("init.lua"), "require(\"../../escape\")\n").unwrap();

    let config = init_config(tmp.path().join("init.lua"));
    let reg = fresh_registry();
    let result = PluginHost::new(&config, Arc::clone(&reg));
    let err = result.err().expect("expected sandbox error");
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

    let config = init_config(tmp.path().join("init.lua"));
    let reg = fresh_registry();
    let _host = PluginHost::new(&config, Arc::clone(&reg)).unwrap();
}

#[test]
fn require_nonexistent_module_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let lua_dir = tmp.path().join("lua");
    std::fs::create_dir_all(&lua_dir).unwrap();

    std::fs::write(tmp.path().join("init.lua"), "require(\"nonexistent\")\n").unwrap();

    let config = init_config(tmp.path().join("init.lua"));
    let reg = fresh_registry();
    let result = PluginHost::new(&config, Arc::clone(&reg));
    let err = result.err().expect("expected error for missing module");
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

    let config = init_config(tmp.path().join("init.lua"));
    let reg = fresh_registry();
    let _host = PluginHost::new(&config, Arc::clone(&reg)).unwrap();
}

#[test]
fn multi_tool_plugin_registers_and_unloads_all() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
fn async_job_exits_without_finish_is_error() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
fn jobstop_kills_running_job() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
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
