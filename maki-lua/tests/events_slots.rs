use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_lua::PluginHost;

const PROBE_SCHEMA: &str = r#"{ type = "object", properties = {}, additionalProperties = false }"#;

fn host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    (reg, host)
}

fn load(host: &PluginHost, name: &str, source: &str) {
    host.load_source(name, source)
        .unwrap_or_else(|e| panic!("{name} failed:\n{e}"));
}

fn exec_tool(reg: &ToolRegistry, name: &str) -> String {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry
        .tool
        .parse(&serde_json::json!({}))
        .expect("parse failed");
    let ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
    let out = smol::block_on(async { inv.execute(&ctx).await })
        .output
        .unwrap_or_else(|e| panic!("tool {name} failed: {e}"));
    match out {
        maki_agent::ToolOutput::Plain(s) => s.text,
        other => panic!("unexpected output: {other:?}"),
    }
}

fn probe_tool(name: &str, body: &str) -> String {
    format!(
        r#"
maki.api.register_tool({{
    name = "{name}",
    description = "probe",
    schema = {PROBE_SCHEMA},
    audiences = {{ "main" }},
    handler = function()
        {body}
    end
}})
"#
    )
}

// ---------------------------------------------------------------- events

#[test]
fn exec_autocmds_pattern_routing_and_ev_shape() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_shape",
        r#"
local got, unfiltered = {}, 0
maki.api.create_autocmd("User", { callback = function() unfiltered = unfiltered + 1 end })
maki.api.create_autocmd("User", { pattern = "deploy", callback = function(ev)
    got[#got + 1] = ev
end })
maki.api.exec_autocmds("User", { pattern = "other", data = { n = 1 } })
maki.api.exec_autocmds("User")
assert(#got == 0, "non-matching or absent pattern must not fire filtered listener")
maki.api.exec_autocmds("User", { pattern = "deploy", data = { n = 2 } })
assert(#got == 1, "matching pattern fires")
assert(unfiltered == 3, "unfiltered listener fires always: " .. unfiltered)
local ev = got[1]
assert(ev.event == "User", "ev.event: " .. tostring(ev.event))
assert(ev.match == "deploy", "ev.match: " .. tostring(ev.match))
assert(type(ev.data) == "table" and ev.data.n == 2, "data nested under ev.data")
assert(type(ev.id) == "number", "ev.id is the autocmd id")
"#,
    );
}

#[test]
fn autocmd_error_isolation() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_isolation",
        r#"
local ran = false
maki.api.create_autocmd("Err", { callback = function() error("boom") end })
maki.api.create_autocmd("Err", { callback = function() ran = true end })
maki.api.exec_autocmds("Err")
assert(ran, "second callback runs after first errors")
"#,
    );
}

#[test]
fn once_callback_semantics() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_once",
        r#"
local n, filtered = 0, 0
maki.api.create_autocmd("Once", { once = true, callback = function()
    n = n + 1
    maki.api.exec_autocmds("Once")
end })
maki.api.create_autocmd("Once", { once = true, pattern = "p", callback = function()
    filtered = filtered + 1
end })
maki.api.exec_autocmds("Once")
assert(n == 1, "reentrant refire: once callback ran " .. n .. " times")
assert(filtered == 0)
maki.api.exec_autocmds("Once", { pattern = "p" })
maki.api.exec_autocmds("Once", { pattern = "p" })
assert(n == 1, "consumed once entry must stay consumed")
assert(filtered == 1, "non-matching fire must not consume a once entry: " .. filtered)
"#,
    );
}

#[test]
fn mutual_recursion_stops_at_depth_guard() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_mutual",
        r#"
local x, y = 0, 0
maki.api.create_autocmd("X", { callback = function() x = x + 1; maki.api.exec_autocmds("Y") end })
maki.api.create_autocmd("Y", { callback = function() y = y + 1; maki.api.exec_autocmds("X") end })
maki.api.exec_autocmds("X")
assert(x > 1, "reentrant cross-event dispatch must nest, got " .. x)
assert(x == y and x < 100, "bounded by depth guard, got x=" .. x .. " y=" .. y)
"#,
    );
}

#[test]
fn ev_table_fresh_per_callback() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_fresh",
        r#"
local first_saw, second_saw
maki.api.create_autocmd("Fresh", { callback = function(ev)
    ev.injected = true
    first_saw = ev.injected
end })
maki.api.create_autocmd("Fresh", { callback = function(ev) second_saw = ev.injected end })
maki.api.exec_autocmds("Fresh")
assert(first_saw == true and second_saw == nil, "ev mutation must not leak to next callback")
"#,
    );
}

#[test]
fn del_autocmd_stops_delivery() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_del",
        r#"
local n = 0
local id = maki.api.create_autocmd("Del", { callback = function() n = n + 1 end })
maki.api.exec_autocmds("Del")
maki.api.del_autocmd(id)
maki.api.exec_autocmds("Del")
assert(n == 1, "deleted autocmd must not fire")
"#,
    );
}

#[test]
fn exec_autocmds_throws_on_bad_arg_types() {
    let (_reg, host) = host();
    load(
        &host,
        "ev_bad_args",
        r#"
assert(not pcall(maki.api.exec_autocmds, 42), "event must be string or string[]")
assert(not pcall(maki.api.exec_autocmds, "E", { pattern = 42 }), "pattern must be a string")
assert(not pcall(maki.api.create_autocmd, "E", { callback = function() end, pattern = 42 }))
"#,
    );
}

#[test]
fn cross_plugin_event_delivery() {
    let (reg, host) = host();
    let listener = format!(
        r#"
local log = {{}}
maki.api.create_autocmd("User", {{ pattern = "deploy", callback = function(ev)
    log[#log + 1] = string.format("%s|%s|%s", ev.event, tostring(ev.match), tostring(ev.data and ev.data.msg))
end }})
{}
"#,
        probe_tool("probe_events", "return table.concat(log, \";\")")
    );
    load(&host, "listener", &listener);
    load(
        &host,
        "firer",
        r#"
maki.api.exec_autocmds("User", { pattern = "deploy", data = { msg = "hi" } })
maki.api.exec_autocmds("User", { pattern = "nope", data = { msg = "skipped" } })
"#,
    );
    assert_eq!(exec_tool(&reg, "probe_events"), "User|deploy|hi");
}

#[test]
fn host_fired_event_has_new_ev_shape() {
    let (reg, host) = host();
    let listener = format!(
        r#"
local log = {{}}
maki.api.create_autocmd("TurnEnd", {{ callback = function(ev)
    log[#log + 1] = string.format("%s|%s|%s", ev.event, tostring(ev.match), tostring(ev.data and ev.data.k))
end }})
{}
"#,
        probe_tool("probe_turn_end", "return table.concat(log, \";\")")
    );
    load(&host, "listener", &listener);
    host.event_handle()
        .unwrap()
        .fire_autocmd("TurnEnd", serde_json::json!({ "k": "v" }));
    assert_eq!(exec_tool(&reg, "probe_turn_end"), "TurnEnd|nil|v");
}

#[test]
fn unload_clears_autocmds_but_keeps_others() {
    let (reg, host) = host();
    let listener = |tool: &str| {
        format!(
            r#"
local n = 0
maki.api.create_autocmd("Shared", {{ callback = function() n = n + 1 end }})
{}
"#,
            probe_tool(tool, "return tostring(n)")
        )
    };
    load(&host, "keep", &listener("probe_keep"));
    load(&host, "gone", &listener("probe_gone"));
    host.unload("gone").unwrap();
    load(&host, "firer", r#"maki.api.exec_autocmds("Shared")"#);
    assert_eq!(exec_tool(&reg, "probe_keep"), "1");
}

// ---------------------------------------------------------------- slots

#[test]
fn slot_layering_wraps_and_overrides() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_order",
        r#"
local greet = maki.api.declare_slot("greet", function(name) return "hello " .. name end)
maki.api.set_slot("greet", function(prev, name) return prev(name) .. "!" end)
maki.api.set_slot("greet", function(prev, name) return "<" .. prev(name) .. ">" end)
assert(greet("bob") == "<hello bob!>", greet("bob"))

local ov = maki.api.declare_slot("ov", function() return "default" end)
maki.api.set_slot("ov", function(prev) return "override" end)
assert(ov() == "override", "layer may replace without calling prev")
"#,
    );
}

#[test]
fn slot_error_after_prev_returns_prev_result_exactly_once() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_late_error",
        r#"
local runs = 0
local s = maki.api.declare_slot("eo", function() runs = runs + 1; return "base" end)
maki.api.set_slot("eo", function(prev)
    local r = prev()
    error("late boom")
end)
local r = s()
assert(r == "base", "chain returns prev's stored result: " .. tostring(r))
assert(runs == 1, "downstream ran exactly once: " .. runs)
"#,
    );
}

#[test]
fn slot_error_before_prev_passes_through_once() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_early_error",
        r#"
local runs = 0
local s = maki.api.declare_slot("pb", function(x) runs = runs + 1; return x end)
maki.api.set_slot("pb", function(prev, x) error("early boom") end)
assert(s("v") == "v", "pass-through degradation keeps the chain working")
assert(runs == 1, "rest of chain ran exactly once: " .. runs)
"#,
    );
}

#[test]
fn slot_prev_called_twice_errors() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_prev_twice",
        r#"
local s = maki.api.declare_slot("tw", function() return 1 end)
maki.api.set_slot("tw", function(prev)
    prev()
    local ok, err = pcall(prev)
    assert(not ok and tostring(err):find("already consumed"), tostring(err))
    return "done"
end)
assert(s() == "done")
"#,
    );
}

#[test]
fn slot_stashed_prev_expires_after_chain_returns() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_stashed_prev",
        r#"
local stash
local s = maki.api.declare_slot("st", function() return 1 end)
maki.api.set_slot("st", function(prev)
    stash = prev
    return prev()
end)
assert(s() == 1)
local ok, err = pcall(stash)
assert(not ok and tostring(err):find("expired"), tostring(err))
"#,
    );
}

#[test]
fn slot_default_error_propagates_through_layers() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_default_error",
        r#"
local s = maki.api.declare_slot("de", function() error("default boom") end)
maki.api.set_slot("de", function(prev) return prev() end)
local ok, err = pcall(s)
assert(not ok and tostring(err):find("default boom"), tostring(err))

local r = maki.api.declare_slot("rc", function() error("db") end)
maki.api.set_slot("rc", function(prev)
    local ok2 = pcall(prev)
    assert(not ok2)
    return "recovered"
end)
assert(r() == "recovered", "layer may recover from a failed prev")
"#,
    );
}

#[test]
fn slot_recursion_bounded_by_depth_guard() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_recursion",
        r#"
local rd
rd = maki.api.declare_slot("recd", function() return rd() end)
local ok, err = pcall(rd)
assert(not ok and tostring(err):find("exceeded max depth"), tostring(err))

local rf
rf = maki.api.declare_slot("recf", function() return "base" end)
maki.api.set_slot("recf", function(prev) return rf() end)
assert(rf() == "base", "recursive filler degrades to pass-through instead of hanging")
"#,
    );
}

#[test]
fn slot_orphan_filler_attaches_on_declare() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_orphan",
        r#"
maki.api.set_slot("oa", function(prev, x) return prev(x) .. "+f" end)
local s = maki.api.declare_slot("oa", function(x) return x end)
assert(s("v") == "v+f", s("v"))
"#,
    );
}

#[test]
fn slot_redeclare_errors_including_self() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_dup_self",
        r#"
maki.api.declare_slot("dup", function() end)
local ok, err = pcall(maki.api.declare_slot, "dup", function() end)
assert(not ok and tostring(err):find("already declared"), tostring(err))
"#,
    );
    let err = host
        .load_source(
            "slot_dup_other",
            r#"maki.api.declare_slot("dup", function() end)"#,
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("already declared by 'slot_dup_self'"),
        "unexpected error: {err}"
    );
}

#[test]
fn get_slots_reports_owner_fillers_and_orphans() {
    let (_reg, host) = host();
    load(
        &host,
        "slots_introspect",
        r#"
maki.api.set_slot("orphan_slot", function(prev) return prev() end)
maki.api.declare_slot("gs", function() return 1 end)
maki.api.set_slot("gs", function(prev) return prev() end)
local slots = maki.api.get_slots()
local gs = slots["gs"]
assert(gs.declared == true and gs.owner == "slots_introspect", tostring(gs.owner))
assert(#gs.fillers == 1 and gs.fillers[1] == "slots_introspect")
local orphan = slots["orphan_slot"]
assert(orphan.declared == false and orphan.owner == nil)
assert(orphan.fillers[1] == "slots_introspect")
"#,
    );
}

const SLOT_CALLER: &str = r#"
local stash
maki.api.create_autocmd("SlotShare", { callback = function(ev) stash = ev.data.callable end })
maki.api.register_tool({
    name = "call_slot",
    description = "probe",
    schema = { type = "object", properties = {}, additionalProperties = false },
    audiences = { "main" },
    handler = function()
        local ok, res = pcall(stash, "world")
        if ok then return "ok:" .. tostring(res) end
        return "err:" .. tostring(res)
    end
})
"#;

const SLOT_OWNER: &str = r#"
local greet = maki.api.declare_slot("greet", function(name) return "hello " .. name end)
maki.api.exec_autocmds("SlotShare", { data = { callable = greet } })
"#;

const FILLER_EXCLAIM: &str =
    r#"maki.api.set_slot("greet", function(prev, name) return prev(name) .. "!" end)"#;
const FILLER_WRAP: &str =
    r#"maki.api.set_slot("greet", function(prev, name) return "<" .. prev(name) .. ">" end)"#;

#[test]
fn slot_reload_semantics() {
    let (reg, host) = host();
    load(&host, "caller", SLOT_CALLER);
    load(&host, "owner", SLOT_OWNER);
    load(&host, "exclaim", FILLER_EXCLAIM);
    load(&host, "wrap", FILLER_WRAP);
    assert_eq!(exec_tool(&reg, "call_slot"), "ok:<hello world!>");

    host.unload("exclaim").unwrap();
    assert_eq!(
        exec_tool(&reg, "call_slot"),
        "ok:<hello world>",
        "middle filler removed, chain still works"
    );

    host.unload("owner").unwrap();
    let out = exec_tool(&reg, "call_slot");
    assert!(
        out.starts_with("err:") && out.contains("slot 'greet' is not declared"),
        "escaped callable after owner unload: {out}"
    );

    load(&host, "owner", SLOT_OWNER);
    assert_eq!(
        exec_tool(&reg, "call_slot"),
        "ok:<hello world>",
        "surviving filler re-attaches after owner reload"
    );
}
