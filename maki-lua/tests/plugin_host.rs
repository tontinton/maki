use std::sync::Arc;

use maki_agent::tools::{ToolRegistry, ToolSource};
use maki_config::LuaPluginsConfig;
use maki_lua::{PluginError, PluginHost};

fn enabled_config() -> LuaPluginsConfig {
    LuaPluginsConfig {
        enabled: true,
        builtins: vec![],
        user_dirs: vec![],
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

#[test]
fn sandbox_strips_dangerous_globals() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

    for global in &["os", "io", "debug", "package"] {
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
fn definitions_unchanged_when_disabled() {
    let reg = fresh_registry();
    let names_before = reg.names();

    let config = LuaPluginsConfig {
        enabled: false,
        builtins: vec![],
        user_dirs: vec![],
    };
    let _host = PluginHost::new(&config, Arc::clone(&reg)).unwrap();

    assert_eq!(reg.names(), names_before, "disabled host mutated registry");
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

#[test]
fn invalid_tool_name_rejected() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "bad name!",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            handler = function(input, ctx) return "" end
        }})"#,
    );
    let err = host
        .load_source("bad_name_plugin", &src)
        .expect_err("expected error for invalid tool name");

    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains("invalid name"));
}

#[test]
fn empty_description_rejected() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "valid_name",
            description = "",
            schema = {MINIMAL_SCHEMA},
            handler = function(input, ctx) return "" end
        }})"#,
    );
    let err = host
        .load_source("empty_desc_plugin", &src)
        .expect_err("expected error for empty description");

    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains("description must be non-empty"));
}

#[test]
fn empty_audiences_rejected() {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();

    let src = format!(
        r#"maki.api.register_tool({{
            name = "no_aud",
            description = "test",
            schema = {MINIMAL_SCHEMA},
            audiences = {{}},
            handler = function() return "" end
        }})"#,
    );
    let err = host
        .load_source("empty_aud", &src)
        .expect_err("expected error for empty audiences list");

    assert!(matches!(err, PluginError::Lua { .. }));
    assert!(err.to_string().contains("audiences"));
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
    ctx.deadline = maki_agent::tools::Deadline::after(std::time::Duration::from_millis(500));

    let start = std::time::Instant::now();
    let result = smol::block_on(async { inv.execute(&ctx).await });

    assert!(result.is_err(), "expected error from timed-out loop");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(30),
        "interrupt took unreasonably long"
    );

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

    host.load_source("p1", "").unwrap();
    assert!(!reg.has("echo_"));
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
                return {{ output = "boom", is_error = true }}
            end
        }})"#,
    );
    host.load_source("err_plugin", &src).unwrap();

    let err = exec_tool(&reg, "returns_error", serde_json::json!({})).unwrap_err();
    assert_eq!(err, "boom");
}

const BAD_RETURN_NUMBER: &str = r#"
maki.api.register_tool({
    name = "bad_ret_num",
    description = "returns a number",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function() return 42 end
})
"#;

const BAD_RETURN_NIL: &str = r#"
maki.api.register_tool({
    name = "bad_ret_nil",
    description = "returns nil",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function() return nil end
})
"#;

#[test_case::test_case(BAD_RETURN_NUMBER, "bad_ret_num" ; "number_return")]
#[test_case::test_case(BAD_RETURN_NIL,    "bad_ret_nil" ; "nil_return")]
fn handler_bad_return_type_is_error(plugin_src: &str, tool_name: &str) {
    let reg = fresh_registry();
    let host = PluginHost::new(&enabled_config(), Arc::clone(&reg)).unwrap();
    host.load_source("bad_ret", plugin_src).unwrap();

    let err = exec_tool(&reg, tool_name, serde_json::json!({})).unwrap_err();
    assert!(err.contains("must return string"), "got: {err}");
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
fn load_dir_discovers_lua_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("greet.lua"),
        r#"
maki.api.register_tool({
    name = "greet",
    description = "says hi",
    schema = { type = "object", properties = {}, additionalProperties = false },
    handler = function() return "hi" end
})
"#,
    )
    .unwrap();
    std::fs::write(tmp.path().join("readme.txt"), "not a plugin").unwrap();

    let config = LuaPluginsConfig {
        enabled: true,
        builtins: vec![],
        user_dirs: vec![tmp.path().to_path_buf()],
    };
    let reg = fresh_registry();
    let _host = PluginHost::new(&config, Arc::clone(&reg)).unwrap();

    assert!(reg.has("greet"));
    assert_eq!(reg.names().len(), 1);
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
