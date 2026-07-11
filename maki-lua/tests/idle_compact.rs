use std::sync::Arc;
use std::time::Duration;

use maki_agent::tools::ToolRegistry;
use maki_lua::PluginHost;

const IDLE_COMPACT_PLUGIN: &str = include_str!("fixtures/idle_compact.lua");

fn host() -> PluginHost {
    PluginHost::new(Arc::new(ToolRegistry::new())).unwrap()
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: serde_json::Value) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    smol::block_on(async { inv.execute(&ctx).await })
        .output
        .map(|out| match out {
            maki_agent::ToolOutput::Plain(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

#[test]
fn session_compact_publishes_to_ui_channel() {
    let host = host();
    let rx = host.ui_action_rx().expect("ui action rx present");
    host.load_source(
        "session_compact_present",
        r#"
        maki.session.compact()
        "#,
    )
    .unwrap();
    let action = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(action, maki_lua::UiAction::Compact));
}

#[test]
fn session_history_len_returns_nil_without_history() {
    let host = host();
    host.load_source(
        "session_history_missing",
        r#"
        local n = maki.session.history_len()
        assert(n == nil, "history_len without shared history should be nil, got: " .. tostring(n))
        "#,
    )
    .unwrap();
}

#[test]
fn uv_hrtime_returns_integer() {
    let host = host();
    host.load_source(
        "hrtime_basic",
        r#"
        local t = maki.uv.hrtime()
        assert(type(t) == "number", "hrtime must return a number")
        assert(t >= 0, "hrtime must be non-negative")
        "#,
    )
    .unwrap();
}

#[test]
fn uv_new_timer_methods_exist() {
    let host = host();
    host.load_source(
        "timer_methods",
        r#"
        local t = maki.uv.new_timer()
        assert(type(t.start) == "function")
        assert(type(t.stop) == "function")
        assert(type(t.close) == "function")
        assert(type(t.again) == "function")
        assert(type(t.set_repeat) == "function")
        assert(type(t.get_repeat) == "function")
        t:close()
        "#,
    )
    .unwrap();
}

#[test]
fn user_input_autocmd_fires_on_event() {
    let host = host();
    let rx = host.ui_action_rx().expect("ui action rx present");
    let eh = host.event_handle().expect("event handle available");
    host.load_source(
        "user_input_autocmd",
        r#"
        maki.api.create_autocmd("UserInput", {
          callback = function() maki.session.compact() end,
        })
        "#,
    )
    .unwrap();
    eh.fire_autocmd("UserInput", serde_json::json!({}));
    let action = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(action, maki_lua::UiAction::Compact));
}

#[test]
fn idle_compact_plugin_loads() {
    let host = host();
    host.load_source("idle_compact", IDLE_COMPACT_PLUGIN)
        .unwrap_or_else(|e| panic!("idle_compact plugin failed to load: {e}"));
}

const TIMER_FIRES_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "timer_fires",
    description = "starts a one-shot timer that finishes the tool",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function(input, ctx)
        local t = maki.uv.new_timer()
        t:start(20, 0, function()
            t:close()
            ctx:finish("timer-fired")
        end)
        return nil
    end
})
"#;

#[test]
fn one_shot_timer_callback_runs_on_runtime_thread() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("timer_fires_plugin", TIMER_FIRES_PLUGIN)
        .unwrap();
    let out = exec_tool(&reg, "timer_fires", serde_json::json!({})).unwrap();
    assert_eq!(out, "timer-fired");
}

const REPEAT_TIMER_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "repeat_timer",
    description = "counts timer fires then finishes",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function(input, ctx)
        local count = 0
        local t = maki.uv.new_timer()
        t:start(10, 10, function()
            count = count + 1
            if count >= 3 then
                t:stop()
                t:close()
                ctx:finish("count=" .. count)
            end
        end)
        return nil
    end
})
"#;

#[test]
fn repeating_timer_fires_multiple_times() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("repeat_timer_plugin", REPEAT_TIMER_PLUGIN)
        .unwrap();
    let out = exec_tool(&reg, "repeat_timer", serde_json::json!({})).unwrap();
    assert_eq!(out, "count=3");
}

const AGAIN_AFTER_STOP_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "again_after_stop",
    description = "starts, stops, then again a repeating timer",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function(input, ctx)
        local t = maki.uv.new_timer()
        t:start(10, 10, function()
            t:close()
            ctx:finish("again-fired")
        end)
        t:stop()
        t:again()
        return nil
    end
})
"#;

#[test]
fn again_restarts_after_stop() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("again_after_stop_plugin", AGAIN_AFTER_STOP_PLUGIN)
        .unwrap();
    let out = exec_tool(&reg, "again_after_stop", serde_json::json!({})).unwrap();
    assert_eq!(out, "again-fired");
}

const START_TWICE_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "start_twice",
    description = "starts a timer twice; only the second callback fires",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function(input, ctx)
        local t = maki.uv.new_timer()
        local fired = {}
        t:start(50, 0, function() fired.first = true end)
        t:start(10, 0, function()
            t:close()
            ctx:finish("fired=" .. tostring(fired.first))
        end)
        return nil
    end
})
"#;

#[test]
fn start_twice_replaces_callback_no_double_fire() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("start_twice_plugin", START_TWICE_PLUGIN)
        .unwrap();
    let out = exec_tool(&reg, "start_twice", serde_json::json!({})).unwrap();
    assert_eq!(out, "fired=nil");
}

const CLOSE_WHILE_ACTIVE_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "close_while_active",
    description = "closes an active repeating timer after one fire then waits",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function(input, ctx)
        local t = maki.uv.new_timer()
        local count = 0
        t:start(10, 10, function()
            count = count + 1
            if count >= 1 then
                t:close()
            end
        end)
        local watcher = maki.uv.new_timer()
        watcher:start(60, 0, function()
            watcher:close()
            ctx:finish("count=" .. count)
        end)
        return nil
    end
})
"#;

#[test]
fn close_while_active_stops_fires() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("close_while_active_plugin", CLOSE_WHILE_ACTIVE_PLUGIN)
        .unwrap();
    let out = exec_tool(&reg, "close_while_active", serde_json::json!({})).unwrap();
    assert_eq!(out, "count=1");
}

const DROP_HANDLE_PLUGIN: &str = r#"
maki.api.register_tool({
    name = "drop_handle",
    description = "drops a timer handle without explicit close",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function(input, ctx)
        do
            local t = maki.uv.new_timer()
            t:start(10, 0, function() end)
            assert(t:is_active())
        end
        local t2 = maki.uv.new_timer()
        t2:start(50, 0, function()
            t2:close()
            ctx:finish("dropped")
        end)
        return nil
    end
})
"#;

#[test]
fn drop_handle_without_close_cleans_up() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("drop_handle_plugin", DROP_HANDLE_PLUGIN)
        .unwrap();
    let out = exec_tool(&reg, "drop_handle", serde_json::json!({})).unwrap();
    assert_eq!(out, "dropped");
}
