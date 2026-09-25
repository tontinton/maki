use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::AgentMode;
use maki_agent::cancel::CancelToken;
use maki_agent::permissions::{LayerAnswer, PermissionAnswer};
use maki_agent::tools::hook::{self, Authority, HookCall, HookStage, Verdict};
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{CallOrigin, ToolRegistry};
use maki_lua::{
    Permission, PlanActionOutcome, PlanFormRow, PlanMenu, PlanRowAction, PluginHost,
    PluginPermissions, SessionEndReason,
};
use maki_storage::id::MakiId;
use test_case::test_case;

const PROBE_SCHEMA: &str = r#"{ type = "object", properties = {}, additionalProperties = false }"#;
/// Generous on purpose. A bound a slow machine can trip is a bound that fails
/// for the wrong reason, and this one is only reached when something is stuck.
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);
const DISPATCH_POLL: Duration = Duration::from_millis(10);

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
        .fire_autocmd("TurnEnd", serde_json::json!({ "k": "v" }));
    assert_eq!(exec_tool(&reg, "probe_turn_end"), "TurnEnd|nil|v");
}

/// A handler has to tell the teardown paths apart, so every reason arrives
/// naming the session it left behind. Only the paths someone waits on carry a
/// deadline, and by the time `end_sessions_blocking` is back the handler has
/// already had its say.
#[test]
fn session_end_carries_session_reason_and_deadline() {
    let (reg, host) = host();
    let listener = format!(
        r#"
local log = {{}}
maki.api.create_autocmd("SessionEnd", {{ callback = function(ev)
    log[#log + 1] = string.format("%s|%s|%s", ev.data.session_id, ev.data.reason,
        type(ev.data.deadline_ms))
end }})
{}
"#,
        probe_tool("probe_session_end", "return table.concat(log, \";\")")
    );
    load(&host, "listener", &listener);

    let mut expected = Vec::new();
    for reason in [
        SessionEndReason::Shutdown,
        SessionEndReason::Replaced,
        SessionEndReason::Completed,
    ] {
        let session = MakiId::generate();
        host.event_handle().end_sessions_blocking([session], reason);
        expected.push(format!("{session}|{reason}|number"));
    }
    assert_eq!(
        exec_tool(&reg, "probe_session_end"),
        expected.join(";"),
        "end_sessions_blocking must only return once the handler ran"
    );

    for reason in [
        SessionEndReason::Reset,
        SessionEndReason::Load,
        SessionEndReason::Delete,
    ] {
        let session = MakiId::generate();
        host.event_handle().end_session(session, reason);
        expected.push(format!("{session}|{reason}|nil"));
    }

    // Nobody waits on the queued paths, so the log fills in behind our back.
    let expected = expected.join(";");
    let give_up = Instant::now() + DISPATCH_TIMEOUT;
    loop {
        let seen = exec_tool(&reg, "probe_session_end");
        if seen == expected {
            return;
        }
        assert!(
            Instant::now() < give_up,
            "SessionEnd handler saw {seen}, expected {expected}"
        );
        std::thread::sleep(DISPATCH_POLL);
    }
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

// ------------------------------------------------------ host tool slots

const SLOT_TOOL: &str = "slotted";
const COMMAND_FIELD: &str = "command";
const ARGV_FIELD: &str = "argv";
const TOOL_ID: &str = "t1";
const GUARDED_TOOL: &str = "guarded";
const LAYER_PLUGIN: &str = "policy_layer";
const HIJACKED: &str = "hijacked";
const COMMAND: &str = "ls";
const PASS_THROUGH: (Option<String>, Option<String>) = (None, None);
const NEVER_ANSWERED: &str = "the chain never answered";
const INNER_LAYER: &str = "inner_layer";
const OUTER_LAYER: &str = "outer_layer";
const INNER_MARK: &str = ":inner";
const OUTER_MARK: &str = ":outer";
/// Far longer than [`HOOK_WINDOW`], so a layer parked on it can only answer
/// because the window let it, never because the job came back in time.
const PARKED_FOR: Duration = Duration::from_secs(5);
/// What a call with no deadline of its own would hand a chain, scaled down:
/// long enough that no healthy layer is rushed, short enough to prove a parked
/// one ends on it.
const HOOK_WINDOW: Duration = Duration::from_millis(200);

/// One string field in the schema, so a layer has something to rewrite.
fn slotted_tool(host: &PluginHost) {
    load(
        host,
        "slotted_owner",
        r#"
maki.api.register_tool({
    name = "slotted",
    description = "probe",
    schema = { type = "object", properties = { command = { type = "string" } } },
    audiences = { "main" },
    handler = function(input) return "ran " .. tostring(input.command) end,
})
"#,
    );
}

/// The call every firing here filters, so a test only names the field it is
/// about.
fn call_of<'a>(
    tool: &'a str,
    authority: Authority,
    origin: CallOrigin,
    cancel: &'a CancelToken,
) -> HookCall<'a> {
    HookCall {
        tool,
        tool_id: TOOL_ID,
        session_id: None,
        origin,
        authority,
        cancel,
        deadline: Instant::now() + DISPATCH_TIMEOUT,
    }
}

fn fire_call(
    reg: &ToolRegistry,
    call: &HookCall<'_>,
    stage: HookStage,
    value: serde_json::Value,
) -> Verdict {
    let hook = reg.hook().expect("the plugin host installs one at boot");
    if !hook.wraps(call.tool, stage) {
        return Verdict::Unchanged;
    }
    within(hook.run(stage, value, call))
}

/// Bounded, so a seam that stops answering fails the test instead of hanging
/// the suite.
fn within<T>(work: impl Future<Output = T>) -> T {
    smol::block_on(async {
        let work = async { Some(work.await) };
        let give_up = async {
            smol::Timer::after(DISPATCH_TIMEOUT).await;
            None
        };
        smol::future::or(work, give_up).await.expect(NEVER_ANSWERED)
    })
}

/// Fires one stage the way `tool_dispatch::run` does, with the authority the
/// registered tool lends: its own capability, or everything when it declares
/// none, the way an MCP or client tool is priced.
fn fire(
    reg: &ToolRegistry,
    tool: &str,
    origin: CallOrigin,
    stage: HookStage,
    value: serde_json::Value,
) -> Verdict {
    let entry = reg.get(tool).expect("tool registered");
    let authority = entry
        .tool
        .required_permission()
        .map_or(Authority::Unbounded, Authority::Capability);
    fire_call(
        reg,
        &call_of(tool, authority, origin, &CancelToken::none()),
        stage,
        value,
    )
}

/// `(replacement, denial reason)`, so a test names the one it means.
fn input(reg: &ToolRegistry, tool: &str, command: &str) -> (Option<String>, Option<String>) {
    input_from(reg, tool, CallOrigin::Model, command)
}

fn input_from(
    reg: &ToolRegistry,
    tool: &str,
    origin: CallOrigin,
    command: &str,
) -> (Option<String>, Option<String>) {
    match fire(
        reg,
        tool,
        origin,
        HookStage::Input,
        serde_json::json!({ COMMAND_FIELD: command }),
    ) {
        Verdict::Unchanged => (None, None),
        Verdict::Replaced(v) => (Some(v[COMMAND_FIELD].as_str().unwrap().to_owned()), None),
        Verdict::Denied(reason) => (None, Some(reason)),
    }
}

fn output(reg: &ToolRegistry, tool: &str, text: &str, is_error: bool) -> Option<(String, bool)> {
    let value = serde_json::json!({ hook::OUTPUT_TEXT: text, hook::OUTPUT_IS_ERROR: is_error });
    match fire(reg, tool, CallOrigin::Model, HookStage::Output, value) {
        Verdict::Unchanged => None,
        Verdict::Denied(reason) => Some((reason, true)),
        Verdict::Replaced(v) => Some((
            v[hook::OUTPUT_TEXT].as_str().unwrap().to_owned(),
            v[hook::OUTPUT_IS_ERROR].as_bool().unwrap_or(is_error),
        )),
    }
}

#[test]
fn tool_input_slot_rewrites_denies_and_passes_through() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        "policy",
        r#"
maki.api.set_slot("tool.slotted.input", function(prev, input, ctx)
    assert(ctx.tool == "slotted", ctx.tool)
    assert(ctx.origin == "model", ctx.origin)
    if input.command == "denied" then
        return nil, "use rg"
    end
    if input.command == "grep -r x ." then
        input.command = "rg x"
        return prev(input, ctx)
    end
end)
"#,
    );

    assert_eq!(
        input(&reg, SLOT_TOOL, "grep -r x ."),
        (Some("rg x".to_owned()), None)
    );
    assert_eq!(
        input(&reg, SLOT_TOOL, "denied"),
        (None, Some("use rg".to_owned()))
    );
    assert_eq!(
        input(&reg, SLOT_TOOL, "ls"),
        (None, None),
        "a layer that returns nothing leaves the call alone"
    );
}

/// Same shape as `bash`: permission checked, so layering it hands the layer
/// that tool's authority.
fn guarded_tool(host: &PluginHost) {
    load(
        host,
        "guarded_owner",
        r#"
maki.api.register_tool({
    name = "guarded",
    description = "probe",
    schema = { type = "object", properties = { command = { type = "string" } } },
    audiences = { "main" },
    permission = "run",
    permission_scopes = function(input)
        return { scopes = { input.command }, force_prompt = false }
    end,
    handler = function(input) return "ran " .. tostring(input.command) end,
})
"#,
    );
}

fn load_granted(host: &PluginHost, plugin: &str, source: &str, granted: PluginPermissions) {
    host.load_source_with_permissions(plugin, source, granted)
        .expect("layer plugin loads whatever it is granted");
}

fn only_run() -> PluginPermissions {
    let mut permissions = PluginPermissions::denied();
    permissions.set(Permission::Run, true);
    permissions
}

fn only_net() -> PluginPermissions {
    let mut permissions = PluginPermissions::denied();
    permissions.set(Permission::Net, true);
    permissions
}

/// Everything but one, because the point is that "almost all" is not all.
fn all_but_run() -> PluginPermissions {
    let mut permissions = PluginPermissions::trusted();
    permissions.set(Permission::Run, false);
    permissions
}

/// A layer on one host slot whose body is the whole contract under test.
fn layer(tool: &str, stage: HookStage, body: &str) -> String {
    format!(
        r#"maki.api.set_slot("tool.{tool}.{}", function(prev, value, ctx) {body} end)"#,
        stage.as_str()
    )
}

/// Rewrites, so `Replaced` means the layer ran and `Unchanged` means it was
/// skipped. That is the answer every entitlement test reads.
fn hijack_layer(tool: &str) -> String {
    layer(
        tool,
        HookStage::Input,
        &format!(r#"value.{COMMAND_FIELD} = "{HIJACKED}"; return prev(value, ctx)"#),
    )
}

/// Appends {mark}, so the order two of them ran in survives into the answer.
fn marking_layer(tool: &str, mark: &str) -> String {
    layer(
        tool,
        HookStage::Input,
        &format!(
            r#"value.{COMMAND_FIELD} = value.{COMMAND_FIELD} .. "{mark}"; return prev(value, ctx)"#
        ),
    )
}

/// The price list. A tool that declares a capability sells its layers exactly
/// that one. A tool declaring none, like an MCP server's tool, has said nothing
/// about how far it reaches, so layering it costs every capability.
///
/// The layer is registered before its target exists, because nobody guarantees
/// load order between a layer and the tool it wraps.
#[test_case(GUARDED_TOOL, only_run,                   true  ; "the_declared_capability_is_enough")]
#[test_case(SLOT_TOOL,    only_run,                   false ; "an_undeclared_reach_takes_more_than_one")]
#[test_case(SLOT_TOOL,    all_but_run,                false ; "almost_every_capability_is_not_every_one")]
#[test_case(SLOT_TOOL,    PluginPermissions::trusted, true  ; "full_trust_layers_anything")]
fn a_layer_pays_the_authority_of_the_call_it_filters(
    tool: &str,
    granted: fn() -> PluginPermissions,
    runs: bool,
) {
    let (reg, host) = host();
    load_granted(&host, LAYER_PLUGIN, &hijack_layer(tool), granted());
    slotted_tool(&host);
    guarded_tool(&host);

    let expected = runs.then(|| HIJACKED.to_owned());
    assert_eq!(input(&reg, tool, COMMAND), (expected, None));
}

#[test]
fn tool_slot_with_no_layers_never_reaches_lua() {
    let (reg, host) = host();
    slotted_tool(&host);
    assert_eq!(input(&reg, SLOT_TOOL, COMMAND), PASS_THROUGH);
    assert_eq!(output(&reg, SLOT_TOOL, "out", false), None);
}

/// The chain owns a task, which is what delivers job events to a layer waiting
/// on one.
#[test]
fn tool_input_layer_may_run_a_job() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        "job_policy",
        r#"
maki.api.set_slot("tool.slotted.input", function(prev, input, ctx)
    local result = maki.fn.jobwait(maki.fn.jobstart({ "echo", "from-job" }))
    input.command = result.stdout
    return prev(input, ctx)
end)
"#,
    );

    assert_eq!(
        input(&reg, SLOT_TOOL, "ls"),
        (Some("from-job".to_owned()), None)
    );
}

#[test]
fn tool_output_slot_rewrites_text_and_error_flag() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        "trimmer",
        r#"
maki.api.set_slot("tool.slotted.output", function(prev, out, ctx)
    out.text = out.text:sub(1, 3)
    out.is_error = true
    return prev(out, ctx)
end)
"#,
    );

    assert_eq!(
        output(&reg, SLOT_TOOL, "0123456789", false),
        Some(("012".to_owned(), true))
    );
}

#[test]
fn unloading_the_layer_owner_restores_the_fast_path() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(&host, "temporary", &hijack_layer(SLOT_TOOL));
    assert_eq!(
        input(&reg, SLOT_TOOL, COMMAND),
        (Some(HIJACKED.to_owned()), None)
    );

    host.unload("temporary").unwrap();
    assert_eq!(
        input(&reg, SLOT_TOOL, COMMAND),
        PASS_THROUGH,
        "the layer index drops with the plugin that registered it"
    );
}

/// Chains overlap whenever tools run in parallel, and each one still has to
/// run its layer. A reentrancy bound that read overlap as nesting would drop
/// layers exactly when a `batch` is widest.
#[test]
fn overlapping_chains_all_run() {
    const CONCURRENT: usize = maki_lua::test_support::MAX_HOOK_DEPTH as usize * 2;
    let (reg, host) = host();
    slotted_tool(&host);
    // Parks first, then marks: a pass-through is reported as untouched, so the
    // rewrite is what says this chain's own layer ran.
    load(
        &host,
        "parking_policy",
        &format!(
            r#"
maki.api.set_slot("tool.slotted.input", function(prev, input, ctx)
    maki.fs.read("/nope")
    input.{COMMAND_FIELD} = input.{COMMAND_FIELD} .. "{INNER_MARK}"
    return prev(input, ctx)
end)
"#
        ),
    );
    let hook = reg.hook().expect("the plugin host installs one at boot");
    let cancel = CancelToken::none();
    let call = call_of(SLOT_TOOL, Authority::Unbounded, CallOrigin::Model, &cancel);

    // Joined, so the chains are genuinely in flight at once. Awaiting them one
    // at a time would never overlap and never notice.
    let verdicts = within(futures::future::join_all((0..CONCURRENT).map(|i| {
        let value = serde_json::json!({ COMMAND_FIELD: i.to_string() });
        hook.run(HookStage::Input, value, &call)
    })));
    let ran = verdicts
        .iter()
        .filter(|v| matches!(v, Verdict::Replaced(_)))
        .count();
    assert_eq!(ran, CONCURRENT, "every overlapping chain ran its layer");
}

/// The documented layer shape returns `prev(...)`, and the identity default
/// hands back what it was given, so a layer that changed nothing still answers
/// with a table. Reporting that as a rewrite would swap the input the caller
/// holds for a re-encode of it, and a JSON null is a Lua nil, which is an
/// absent key, so the re-encode would quietly lose fields on the way.
#[test_case(serde_json::json!({ COMMAND_FIELD: COMMAND }) ; "a_value_the_layer_could_have_touched")]
#[test_case(serde_json::json!({ COMMAND_FIELD: null, ARGV_FIELD: [COMMAND, null] }) ; "nulls_no_layer_ever_sees")]
fn a_pass_through_layer_leaves_the_input_untouched(value: serde_json::Value) {
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        LAYER_PLUGIN,
        &layer(SLOT_TOOL, HookStage::Input, "return prev(value, ctx)"),
    );

    let verdict = fire(&reg, SLOT_TOOL, CallOrigin::Model, HookStage::Input, value);

    assert!(
        matches!(verdict, Verdict::Unchanged),
        "a layer that only deferred is not a rewrite"
    );
}

/// The chain runs on the Lua thread, so the agent side giving up on the reply
/// is not the same as the chain ending: without the call's own token the
/// watchdog has nothing to interrupt this layer with, and it spins forever.
#[test]
fn cancelling_the_call_kills_the_chain() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        LAYER_PLUGIN,
        &layer(
            SLOT_TOOL,
            HookStage::Input,
            &format!(
                r#"local n = 0
                while true do n = n + 1 end
                value.{COMMAND_FIELD} = "{HIJACKED}"
                return prev(value, ctx)"#
            ),
        ),
    );

    let (trigger, cancel) = CancelToken::new();
    trigger.cancel();
    let call = call_of(SLOT_TOOL, Authority::Unbounded, CallOrigin::Model, &cancel);
    let value = serde_json::json!({ COMMAND_FIELD: COMMAND });

    let verdict = fire_call(&reg, &call, HookStage::Input, value);

    assert!(
        matches!(verdict, Verdict::Unchanged),
        "the killed layer never reached its rewrite"
    );
}

/// The watchdog only interrupts Lua that runs, and a layer parked in an await
/// runs none: it renews its grace at every yield and would sit there as long as
/// whatever it waits on. Only the window dispatch hands the chain ends this
/// one, well before the job it is parked on comes back.
#[test]
fn a_parked_layer_ends_at_the_window_it_was_given() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        LAYER_PLUGIN,
        &layer(
            SLOT_TOOL,
            HookStage::Input,
            &format!(
                r#"maki.fn.jobwait(maki.fn.jobstart({{ "sleep", "{}" }}), {})
                value.{COMMAND_FIELD} = "{HIJACKED}"
                return prev(value, ctx)"#,
                PARKED_FOR.as_secs(),
                PARKED_FOR.as_millis()
            ),
        ),
    );

    let cancel = CancelToken::none();
    let mut call = call_of(SLOT_TOOL, Authority::Unbounded, CallOrigin::Model, &cancel);
    call.deadline = Instant::now() + HOOK_WINDOW;
    let value = serde_json::json!({ COMMAND_FIELD: COMMAND });

    let verdict = fire_call(&reg, &call, HookStage::Input, value);

    assert!(
        matches!(verdict, Verdict::Unchanged),
        "the abandoned layer never reached its rewrite"
    );
}

#[test_case("tool.bash.input" ; "tool_stage")]
#[test_case("ui.plan_form" ; "ui_surface")]
#[test_case("ui.plan_form.actions" ; "ui_menu")]
#[test_case("permission.prompt" ; "permission_prompt")]
fn host_slot_names_are_reserved(name: &str) {
    let (_reg, host) = host();
    let err = host
        .load_source(
            "squatter",
            &format!(r#"maki.api.declare_slot("{name}", function(i) return i end)"#),
        )
        .expect_err("declaring a host slot must fail");
    // A plugin that used to declare one meets this message, so it has to name
    // the prefix and what to do instead.
    let err = format!("{err}");
    for expected in ["host owned", "reserved", "set_slot"] {
        assert!(err.contains(expected), "{err}");
    }
}

// -------------------------------------------------- permission.prompt slot

const PROMPT_ALLOW: &str = r#"return { decision = "allow" }"#;
const PROMPT_PASS: &str = "return prev(req, ctx)";
const PROMPT_GUIDANCE: &str = "use trash instead";

fn prompt_layer(body: &str) -> String {
    format!(r#"maki.api.set_slot("permission.prompt", function(prev, req, ctx) {body} end)"#)
}

/// Asks the chain what `enforce` asks it about a call to {SLOT_TOOL} that
/// would otherwise prompt.
fn ask_prompt(reg: &ToolRegistry, authority: Authority) -> Option<LayerAnswer> {
    let hook = reg.hook().expect("the plugin host installs one at boot");
    let cancel = CancelToken::none();
    let call = call_of(SLOT_TOOL, authority, CallOrigin::Model, &cancel);
    let input = serde_json::json!({ COMMAND_FIELD: COMMAND });
    let scopes = [COMMAND.to_owned()];
    let ctx = stub_ctx(&AgentMode::Build);
    within(hook.prompt(&input, &scopes, &call, &ctx))
}

#[test_case(r#"return { decision = "allow_session" }"#, Some(PermissionAnswer::AllowSession) ; "answers_for_the_user")]
#[test_case(PROMPT_PASS, None ; "passing_it_on_leaves_it_to_the_prompt")]
#[test_case(r#"return { decision = "maybe" }"#, None ; "an_answer_the_prompt_does_not_offer_prompts")]
#[test_case(r#"error("boom")"#, None ; "a_throwing_layer_prompts")]
#[test_case(
    r#"if req.tool == "slotted" and req.tool_id == "t1" and req.input.command == "ls"
        and req.scopes[1] == "ls" and ctx:task_id() == "main" then
        return { decision = "allow" }
    end
    return prev(req, ctx)"#,
    Some(PermissionAnswer::AllowOnce) ;
    "req_and_ctx_describe_the_call"
)]
#[test_case(
    r#"local judge = maki.agent.session(ctx, { name = "judge" })
    if not judge then return prev(req, ctx) end
    judge:close()
    return { decision = "allow" }"#,
    Some(PermissionAnswer::AllowOnce) ;
    "ctx_can_open_a_session_to_ask_a_model"
)]
fn a_prompt_layer_answers_with_the_prompts_options(body: &str, expected: Option<PermissionAnswer>) {
    let (reg, host) = host();
    load(&host, LAYER_PLUGIN, &prompt_layer(body));

    let answered = ask_prompt(&reg, Authority::Unbounded);

    assert_eq!(
        answered,
        expected.map(|answer| LayerAnswer {
            plugin: Arc::from(LAYER_PLUGIN),
            answer,
        })
    );
}

/// The tool row names the plugin that allowed a call and a deny speaks in
/// its voice, so the answer has to be pinned on the layer that built it, not
/// on whichever one handed it up last.
#[test_case(PROMPT_PASS, INNER_LAYER, PermissionAnswer::AllowOnce ; "passed_up")]
#[test_case(
    &format!(r#"prev(req, ctx); return {{ decision = "deny", guidance = "{PROMPT_GUIDANCE}" }}"#),
    OUTER_LAYER,
    PermissionAnswer::DenyWithGuidance(PROMPT_GUIDANCE.into()) ;
    "overruled"
)]
fn the_layer_that_built_the_answer_is_named(outer: &str, decider: &str, answer: PermissionAnswer) {
    let (reg, host) = host();
    load(&host, INNER_LAYER, &prompt_layer(PROMPT_ALLOW));
    load(&host, OUTER_LAYER, &prompt_layer(outer));

    let answered = ask_prompt(&reg, Authority::Unbounded);

    assert_eq!(
        answered,
        Some(LayerAnswer {
            plugin: Arc::from(decider),
            answer,
        })
    );
}

/// Allowing a call lends the call's reach, so a layer pays for it the way a
/// `tool.<name>.input` layer does.
#[test_case(Authority::Capability(Permission::Run), true ; "the_calls_capability_is_enough")]
#[test_case(Authority::Unbounded, false ; "an_undeclared_reach_takes_every_one")]
fn a_prompt_layer_pays_the_authority_of_the_call(authority: Authority, answers: bool) {
    let (reg, host) = host();
    load_granted(&host, LAYER_PLUGIN, &prompt_layer(PROMPT_ALLOW), only_run());

    assert_eq!(ask_prompt(&reg, authority).is_some(), answers);
}

// ------------------------------------------------------ ui.plan_form slots

const PLAN_PATH: &str = "/tmp/plan.md";
const PLAN_SESSION: &str = "s1";
const PLANNER: &str = "planner";
const BUILTIN_ID: &str = "implement";
const BUILTIN_LABEL: &str = "Implement plan";
const PLUGIN_ID: &str = "commit_and_implement";
const PLUGIN_LABEL: &str = "Commit and implement";
/// Longer than the window the host gives the plan form chains, so a layer
/// parked on it can only answer after the form has given up on it.
const PLAN_PARKED_FOR: Duration = Duration::from_secs(15);
const NO_PICK: &str = "none";

/// Stands in for what the UI proposes: one row it knows how to run itself.
fn builtin_rows() -> Vec<PlanFormRow> {
    vec![PlanFormRow {
        id: BUILTIN_ID.to_owned(),
        label: BUILTIN_LABEL.to_owned(),
        desc: String::new(),
        action: Some(PlanRowAction::Implement),
        plugin: None,
    }]
}

fn plan_slot_source(slot: &str, body: &str) -> String {
    format!(r#"maki.api.set_slot("{slot}", function(prev, ev) {body} end)"#)
}

fn plan_slot_layer(host: &PluginHost, plugin: &str, slot: &str, body: &str) {
    load(host, plugin, &plan_slot_source(slot, body));
}

fn plan_form_layer(host: &PluginHost, plugin: &str, body: &str) {
    plan_slot_layer(host, plugin, "ui.plan_form", body);
}

fn actions_layer(host: &PluginHost, plugin: &str, body: &str) {
    plan_slot_layer(host, plugin, "ui.plan_form.actions", body);
}

/// A layer that never comes back, for the slot it is handed to.
fn parked_layer_body() -> String {
    format!(
        r#"maki.fn.jobwait(maki.fn.jobstart({{ "sleep", "{}" }}), {})
           return prev(ev)"#,
        PLAN_PARKED_FOR.as_secs(),
        PLAN_PARKED_FOR.as_millis()
    )
}

/// The row a layer adds, with whatever extra fields the case needs.
fn plugin_row(extra: &str) -> String {
    format!(r#"{{ id = "{PLUGIN_ID}", label = "{PLUGIN_LABEL}", {extra} }}"#)
}

fn ask_plan_form(host: &PluginHost) -> Option<PlanMenu> {
    host.event_handle()
        .open_plan_form(
            PLAN_PATH.to_owned(),
            PLAN_SESSION.to_owned(),
            builtin_rows(),
        )
        .recv_timeout(DISPATCH_TIMEOUT)
        .expect("the plan form chains must answer")
}

fn plan_menu(host: &PluginHost) -> PlanMenu {
    ask_plan_form(host).expect("the built-in form must open")
}

fn menu_labels(host: &PluginHost) -> Vec<String> {
    plan_menu(host)
        .rows
        .into_iter()
        .map(|row| row.label)
        .collect()
}

/// Picks {row} of {menu} and waits for the host to say what became of it.
fn pick_row(host: &PluginHost, menu: &PlanMenu, row: usize) -> PlanActionOutcome {
    host.event_handle()
        .run_plan_action(
            PLAN_SESSION.to_owned(),
            menu.generation,
            row,
            PLAN_PATH.to_owned(),
            true,
        )
        .recv_timeout(DISPATCH_TIMEOUT)
        .expect("a pick must be answered")
}

fn plugin_row_index(menu: &PlanMenu) -> usize {
    menu.rows
        .iter()
        .position(|r| r.plugin.is_some())
        .expect("the layer's row must carry a handler")
}

/// The chain decides whether the built-in form opens. Deferring to `prev`
/// reaches the host default, and answering without it takes the surface over.
/// A layer that throws never costs the user the form.
#[test_case("return false", false ; "layer_owns_the_surface")]
#[test_case("return", false ; "layer_answers_with_nothing")]
#[test_case("return prev(ev)", true ; "layer_defers_to_the_builtin")]
#[test_case("error('boom')", true ; "broken_layer_leaves_the_builtin")]
fn the_plan_form_slot_decides_whether_the_builtin_opens(body: &str, opens: bool) {
    let (_reg, host) = host();
    plan_form_layer(&host, PLANNER, body);
    assert_eq!(ask_plan_form(&host).is_some(), opens);
}

/// A layer parked in an await runs no Lua for the watchdog to interrupt, so
/// without a deadline the draft would sit there with no surface at all.
#[test_case("ui.plan_form" ; "the_form_chain")]
#[test_case("ui.plan_form.actions" ; "the_actions_chain")]
fn a_parked_plan_form_layer_falls_back_to_the_builtin(slot: &str) {
    let (_reg, host) = host();
    plan_slot_layer(&host, PLANNER, slot, &parked_layer_body());
    assert_eq!(menu_labels(&host), [BUILTIN_LABEL]);
}

/// Taking the form over decides what the user is shown once a plan lands, so
/// it is priced like layering a call nobody declared the reach of: a plugin
/// short of one grant is skipped and the built-in form opens.
#[test]
fn an_ungranted_layer_cannot_take_the_plan_form_over() {
    let (_reg, host) = host();
    load_granted(
        &host,
        PLANNER,
        &plan_slot_source("ui.plan_form", "return false"),
        all_but_run(),
    );
    assert!(
        ask_plan_form(&host).is_some(),
        "the layer must not have been asked"
    );
}

/// The same price for the menu, and for the sharper reason: a row may keep a
/// built-in row's wording and swap the outcome under it, so pressing Enter on
/// what reads like "Refine plan" would start a build-mode turn.
#[test]
fn an_ungranted_actions_layer_cannot_reach_the_menu() {
    let (_reg, host) = host();
    load_granted(
        &host,
        PLANNER,
        &plan_slot_source(
            "ui.plan_form.actions",
            &format!("return {{ {} }}", plugin_row(r#"action = "implement""#)),
        ),
        all_but_run(),
    );
    assert_eq!(menu_labels(&host), [BUILTIN_LABEL]);
}

/// Plan state is per session, so the layer is told which one it is answering
/// for.
#[test]
fn the_plan_form_slot_carries_the_path_and_session() {
    let (_reg, host) = host();
    plan_form_layer(
        &host,
        PLANNER,
        &format!(r#"return ev.path == "{PLAN_PATH}" and ev.session == "{PLAN_SESSION}""#),
    );
    assert!(
        ask_plan_form(&host).is_some(),
        "the layer saw the wrong event"
    );
}

/// Nothing layered means nothing to ask, and the default answer is the
/// built-in form with the rows the host proposed.
#[test]
fn an_unlayered_plan_form_opens_the_builtin() {
    let (_reg, host) = host();
    assert_eq!(menu_labels(&host), [BUILTIN_LABEL]);
}

/// Form suppression is a slot, so unloading the plugin that took the form
/// over tears the layer down and the built-in comes back.
#[test]
fn unloading_the_plan_form_owner_hands_the_form_back() {
    let (_reg, host) = host();
    plan_form_layer(&host, PLANNER, "return false");
    assert!(ask_plan_form(&host).is_none());

    host.unload(PLANNER).unwrap();
    assert_eq!(
        menu_labels(&host),
        [BUILTIN_LABEL],
        "the built-in form has to come back with the layer gone"
    );
}

/// A layer that appends to what `prev` gave it lands its row next to the
/// built-in ones.
#[test]
fn an_actions_layer_adds_its_row_to_the_menu() {
    let (_reg, host) = host();
    actions_layer(
        &host,
        PLANNER,
        &format!(
            r#"local rows = prev(ev)
               table.insert(rows, {})
               return rows"#,
            plugin_row("handler = function() end")
        ),
    );
    assert_eq!(menu_labels(&host), [BUILTIN_LABEL, PLUGIN_LABEL]);
}

/// The rows `prev` hands back are the plugin's to reorder or drop.
#[test]
fn an_actions_layer_can_reorder_and_drop_builtin_rows() {
    let (_reg, host) = host();
    actions_layer(
        &host,
        PLANNER,
        &format!("return {{ {} }}", plugin_row("handler = function() end")),
    );
    assert_eq!(menu_labels(&host), [PLUGIN_LABEL]);
}

/// A reload clears the plugin's layers before its source runs again, so the
/// same row registered twice is still one row.
#[test]
fn reloading_an_actions_layer_does_not_stack_duplicates() {
    let (_reg, host) = host();
    let source = format!(
        r#"maki.api.set_slot("ui.plan_form.actions", function(prev, ev)
               local rows = prev(ev)
               table.insert(rows, {})
               return rows
           end)"#,
        plugin_row("handler = function() end")
    );
    load(&host, PLANNER, &source);
    load(&host, PLANNER, &source);
    assert_eq!(menu_labels(&host), [BUILTIN_LABEL, PLUGIN_LABEL]);
}

/// Unload the plugin and the host's own rows are what is left.
#[test]
fn unloading_an_actions_layer_restores_the_builtin_rows() {
    let (_reg, host) = host();
    actions_layer(
        &host,
        PLANNER,
        &format!(
            r#"local rows = prev(ev)
               table.insert(rows, {})
               return rows"#,
            plugin_row("handler = function() end")
        ),
    );
    assert_eq!(menu_labels(&host).len(), 2);

    host.unload(PLANNER).unwrap();
    assert_eq!(menu_labels(&host), [BUILTIN_LABEL]);
}

/// An off-contract answer leaves the host's own rows.
#[test_case("return prev(ev) and 42" ; "not_a_table")]
#[test_case(r#"return { { id = "x", label = "nope" } }"# ; "row_with_no_handler_or_action")]
#[test_case(r#"return { { label = "nope", action = "implement" } }"# ; "row_with_no_id")]
#[test_case(r#"local rows = prev(ev) table.insert(rows, rows[1]) return rows"# ; "duplicate_ids")]
fn a_broken_actions_layer_leaves_the_builtin_menu(body: &str) {
    let (_reg, host) = host();
    actions_layer(&host, PLANNER, body);
    assert_eq!(menu_labels(&host), [BUILTIN_LABEL]);
}

/// A plugin cannot hand out more rows than the form can draw.
#[test]
fn an_actions_layer_cannot_grow_the_menu_without_bound() {
    let (_reg, host) = host();
    actions_layer(
        &host,
        PLANNER,
        r#"local rows = prev(ev)
           for i = 1, 5000 do
               table.insert(rows, { id = "r" .. i, label = "row", handler = function() end })
           end
           return rows"#,
    );
    let drawn = plan_menu(&host).rows.len();
    assert!(drawn > 1, "the layer's rows are not all dropped: {drawn}");
    assert!(drawn < 5000, "the menu has to be capped: {drawn}");
}

/// The handler runs on the Lua thread when the user picks the row, told which
/// session's plan it is acting on so it can name it back to `maki.plan.read`
/// and `maki.session.*`.
#[test]
fn a_picked_row_runs_its_handler_with_the_plan_context() {
    let (reg, host) = host();
    load(
        &host,
        PLANNER,
        &format!(
            r#"
local seen = nil
maki.api.set_slot("ui.plan_form.actions", function(prev, ev)
    local rows = prev(ev)
    table.insert(rows, {})
    return rows
end)
{}
"#,
            plugin_row("handler = function(opts) seen = opts end"),
            probe_tool(
                "probe_plan_row",
                &format!(
                    r#"
if not seen then return "{NO_PICK}" end
return table.concat({{ seen.session, seen.path, tostring(seen.parallel) }}, "|")
"#
                )
            )
        ),
    );

    let menu = plan_menu(&host);
    pick_row(&host, &menu, plugin_row_index(&menu));

    assert_eq!(
        exec_tool(&reg, "probe_plan_row"),
        format!("{PLAN_SESSION}|{PLAN_PATH}|true")
    );
}

/// Handler-then-action: a row that kept a built-in outcome runs the handler
/// first and the outcome after, unless the handler said otherwise.
///
/// A handler that declines and one that failed are told apart, since only the
/// second is worth showing the user.
#[test_case("", PlanActionOutcome::Proceed ; "a_handler_that_says_nothing_keeps_the_action")]
#[test_case("return true", PlanActionOutcome::Proceed ; "a_handler_that_agrees")]
#[test_case("return false", PlanActionOutcome::Vetoed ; "a_handler_that_declines")]
#[test_case("error('boom')", PlanActionOutcome::Failed ; "a_handler_that_fails")]
fn a_handler_runs_before_the_action_it_kept(body: &str, outcome: PlanActionOutcome) {
    let (_reg, host) = host();
    actions_layer(
        &host,
        PLANNER,
        &format!(
            r#"local rows = prev(ev)
               rows[1].handler = function() {body} end
               return rows"#
        ),
    );

    let menu = plan_menu(&host);
    assert_eq!(menu.rows[0].action, Some(PlanRowAction::Implement));
    assert_eq!(menu.rows[0].plugin.as_deref(), Some(PLANNER));
    assert_eq!(pick_row(&host, &menu, 0), outcome);
}

/// The menu is every layer's work. A row one layer got wrong costs that row,
/// not the rows every other layer put there: dropping the whole menu would
/// hand any plugin a way to suppress every other plugin's.
#[test]
fn one_bad_row_does_not_cost_the_other_layers_theirs() {
    let (_reg, host) = host();
    actions_layer(
        &host,
        PLANNER,
        &format!(
            r#"local rows = prev(ev)
               table.insert(rows, {})
               return rows"#,
            plugin_row("handler = function() end")
        ),
    );
    actions_layer(
        &host,
        "broken",
        r#"local rows = prev(ev)
           table.insert(rows, { id = "", label = "" })
           return rows"#,
    );

    assert_eq!(menu_labels(&host), [BUILTIN_LABEL, PLUGIN_LABEL]);
}

/// Attribution is by handler, not by row id. A layer that lifts another
/// plugin's handler out of `prev(ev)` and files it under an id that plugin
/// never used would otherwise own it, and the unload meant to reap it would
/// walk straight past it. The re-keyed row goes, the rest of the menu stays.
#[test]
fn a_layer_cannot_refile_another_plugins_handler_under_a_new_id() {
    let (_reg, host) = host();
    actions_layer(
        &host,
        PLANNER,
        &format!(
            r#"local rows = prev(ev)
               table.insert(rows, {})
               return rows"#,
            plugin_row("handler = function() end")
        ),
    );
    actions_layer(
        &host,
        "thief",
        &format!(
            r#"local rows = prev(ev)
               for _, row in ipairs(rows) do
                   if row.id == "{PLUGIN_ID}" then row.id = "stolen" end
               end
               return rows"#
        ),
    );

    assert_eq!(
        menu_labels(&host),
        [BUILTIN_LABEL],
        "the re-keyed row is the only one dropped"
    );
}

/// The handler holds the plugin's permissions, so an unload takes it with
/// everything else and a pick after that reaches nothing.
#[test]
fn unloading_a_plugin_reaps_the_handlers_of_a_drawn_menu() {
    let (_reg, host) = host();
    actions_layer(
        &host,
        PLANNER,
        &format!(
            r#"local rows = prev(ev)
               table.insert(rows, {})
               return rows"#,
            plugin_row("handler = function() end, action = \"implement\"")
        ),
    );

    let menu = plan_menu(&host);
    let row = plugin_row_index(&menu);
    assert_eq!(
        pick_row(&host, &menu, row),
        PlanActionOutcome::Proceed,
        "the handler answers while loaded"
    );

    host.unload(PLANNER).unwrap();
    assert_eq!(
        pick_row(&host, &menu, row),
        PlanActionOutcome::Failed,
        "a reaped handler cannot answer for the row, and a pick that reached \
         nothing is a failure, not a veto"
    );
}

/// A chain that parked and resumed installs its handlers over a menu already
/// on screen, and the generation keeps a pick out of a menu the user never
/// saw.
#[test]
fn a_pick_from_a_replaced_menu_reaches_no_handler() {
    let (_reg, host) = host();
    actions_layer(
        &host,
        PLANNER,
        &format!(
            r#"local rows = prev(ev)
               table.insert(rows, {})
               return rows"#,
            plugin_row("handler = function() end")
        ),
    );

    let drawn = plan_menu(&host);
    let row = plugin_row_index(&drawn);
    let replacement = plan_menu(&host);
    assert_ne!(drawn.generation, replacement.generation);

    assert_eq!(
        pick_row(&host, &drawn, row),
        PlanActionOutcome::Failed,
        "a pick from the replaced menu must not reach the new one's handlers"
    );
    assert_eq!(
        pick_row(&host, &replacement, row),
        PlanActionOutcome::Proceed
    );
}

/// A layer answering off contract costs what no layer costs: dispatch keeps the
/// call it already had rather than hand the tool a shape nobody promised. One
/// case is a table conversion that genuinely fails, the only way to reach the
/// "not json" arm.
#[test_case(r#"return "nope""# ; "string")]
#[test_case("return 42" ; "number")]
#[test_case("return true" ; "boolean")]
#[test_case("return nil, 42" ; "non_string_reason")]
#[test_case(r#"return { "\255\254" }"# ; "table_that_is_not_json")]
fn a_malformed_layer_answer_leaves_the_call_alone(answer: &str) {
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        LAYER_PLUGIN,
        &layer(SLOT_TOOL, HookStage::Input, answer),
    );

    assert_eq!(input(&reg, SLOT_TOOL, COMMAND), PASS_THROUGH);
}

/// The reason is what the model reads instead of the output, so it has to
/// arrive verbatim rather than as a replacement value.
#[test]
fn an_output_layer_stops_the_call_with_its_reason() {
    const REASON: &str = "redacted";
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        LAYER_PLUGIN,
        &layer(
            SLOT_TOOL,
            HookStage::Output,
            &format!(r#"return nil, "{REASON}""#),
        ),
    );

    assert_eq!(
        output(&reg, SLOT_TOOL, "secret", false),
        Some((REASON.to_owned(), true))
    );
}

/// One plugin's broken layer must not take the seam down or swallow the layers
/// another plugin registered underneath it.
#[test]
fn a_broken_layer_is_skipped_and_the_chain_still_answers() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(&host, INNER_LAYER, &hijack_layer(SLOT_TOOL));
    load(
        &host,
        OUTER_LAYER,
        &layer(SLOT_TOOL, HookStage::Input, r#"error("boom")"#),
    );

    assert_eq!(
        input(&reg, SLOT_TOOL, COMMAND),
        (Some(HIJACKED.to_owned()), None)
    );
}

/// Layers wrap in registration order across plugins too, so the last one
/// registered sees the call first. Otherwise two plugins that both rewrite
/// would compose differently depending on a load order nobody can see.
#[test]
fn layers_compose_with_the_last_registered_outermost() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(&host, INNER_LAYER, &marking_layer(SLOT_TOOL, INNER_MARK));
    load(&host, OUTER_LAYER, &marking_layer(SLOT_TOOL, OUTER_MARK));

    assert_eq!(
        input(&reg, SLOT_TOOL, COMMAND),
        (Some(format!("{COMMAND}{OUTER_MARK}{INNER_MARK}")), None)
    );
}

/// Entitlement is per layer, not per slot. The plugin holding the tool's
/// capability keeps its rewrite, the one without it is dropped from the chain,
/// and being dropped is not a denial.
#[test]
fn only_the_entitled_layer_of_two_runs() {
    let (reg, host) = host();
    guarded_tool(&host);
    let denied = PluginPermissions::denied();
    load_granted(
        &host,
        INNER_LAYER,
        &marking_layer(GUARDED_TOOL, INNER_MARK),
        denied,
    );
    load_granted(
        &host,
        OUTER_LAYER,
        &marking_layer(GUARDED_TOOL, OUTER_MARK),
        only_run(),
    );

    assert_eq!(
        input(&reg, GUARDED_TOOL, COMMAND),
        (Some(format!("{COMMAND}{OUTER_MARK}")), None),
        "the denied layer is skipped, not consulted and not a denial"
    );
}

/// A layer owns the decision it makes: answering without calling `prev` is how
/// it stops the layers below from seeing the call at all.
#[test]
fn a_layer_that_never_calls_prev_short_circuits() {
    const PROBE: &str = "probe_inner_ran";
    const SHORT: &str = "short";
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        LAYER_PLUGIN,
        &format!(
            r#"
local inner_ran = false
{}
{}
{}
"#,
            layer(
                SLOT_TOOL,
                HookStage::Input,
                "inner_ran = true; return prev(value, ctx)"
            ),
            layer(
                SLOT_TOOL,
                HookStage::Input,
                &format!(r#"return {{ {COMMAND_FIELD} = "{SHORT}" }}"#)
            ),
            probe_tool(PROBE, "return tostring(inner_ran)")
        ),
    );

    assert_eq!(
        input(&reg, SLOT_TOOL, COMMAND),
        (Some(SHORT.to_owned()), None)
    );
    assert_eq!(
        exec_tool(&reg, PROBE),
        "false",
        "the layer below the one that answered must never have run"
    );
}

/// The grant is read when the chain fires, so narrowing it costs one reload
/// rather than a restart.
#[test]
fn a_reload_that_narrows_permissions_applies_to_the_next_call() {
    let (reg, host) = host();
    guarded_tool(&host);
    load_granted(&host, LAYER_PLUGIN, &hijack_layer(GUARDED_TOOL), only_run());
    assert_eq!(
        input(&reg, GUARDED_TOOL, COMMAND),
        (Some(HIJACKED.to_owned()), None)
    );

    let source = hijack_layer(GUARDED_TOOL);
    load_granted(&host, LAYER_PLUGIN, &source, PluginPermissions::denied());
    assert_eq!(
        input(&reg, GUARDED_TOOL, COMMAND),
        PASS_THROUGH,
        "the reloaded plugin is weighed by what this load granted it"
    );
}

/// The hook outlives the runtime that installed it, and a call in flight at
/// shutdown still has to be answered. The only honest answer left is the one
/// no layer would have changed.
#[test]
fn a_dropped_host_leaves_the_call_unchanged() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(&host, LAYER_PLUGIN, &hijack_layer(SLOT_TOOL));
    let hook = reg.hook().expect("the plugin host installs one at boot");
    assert!(hook.wraps(SLOT_TOOL, HookStage::Input));

    drop(host);

    let cancel = CancelToken::none();
    let call = call_of(SLOT_TOOL, Authority::Unbounded, CallOrigin::Model, &cancel);
    let value = serde_json::json!({ COMMAND_FIELD: COMMAND });
    let verdict = within(hook.run(HookStage::Input, value, &call));

    assert!(
        matches!(verdict, Verdict::Unchanged),
        "a runtime that is gone is not a denial"
    );
}

/// `tool.` is the host's namespace, but only two names in it mean anything. A
/// third is accepted and inert rather than rejected, and above all it must not
/// enrol the tool into a stage it never named.
#[test]
fn a_tool_slot_that_names_no_stage_never_fires() {
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        LAYER_PLUGIN,
        &format!(
            r#"maki.api.set_slot("tool.{SLOT_TOOL}.header", function(prev, value, ctx)
                value.{COMMAND_FIELD} = "{HIJACKED}"
                return prev(value, ctx)
            end)"#
        ),
    );

    let hook = reg.hook().expect("the plugin host installs one at boot");
    for stage in HookStage::ALL {
        assert!(
            !hook.wraps(SLOT_TOOL, stage),
            "a name that is not a stage must not wrap {stage:?}"
        );
    }
    assert_eq!(input(&reg, SLOT_TOOL, COMMAND), PASS_THROUGH);
}

/// What a layer is told about the call it filters. `origin` is the one field it
/// cannot get anywhere else, and it says whether the model asked for this call
/// or another tool did.
#[test_case(CallOrigin::Model, "model" ; "model")]
#[test_case(CallOrigin::Nested, "nested" ; "nested")]
fn the_ctx_table_names_the_call(origin: CallOrigin, expected_origin: &str) {
    let (reg, host) = host();
    slotted_tool(&host);
    load(
        &host,
        LAYER_PLUGIN,
        &layer(
            SLOT_TOOL,
            HookStage::Input,
            &format!(
                r#"value.{COMMAND_FIELD} = ctx.tool_id .. "|" .. ctx.origin; return prev(value, ctx)"#
            ),
        ),
    );

    assert_eq!(
        input_from(&reg, SLOT_TOOL, origin, COMMAND),
        (Some(format!("{TOOL_ID}|{expected_origin}")), None)
    );
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

/// Chains are async all the way down, so a layer can wait for the answer it
/// needs before deciding. Without that, wrapping a seam would only pay off for
/// decisions that need nothing but the argument.
#[test]
fn slot_chain_may_suspend() {
    let (_reg, host) = host();
    load(
        &host,
        "slot_suspends",
        r#"
local d = maki.api.declare_slot("sd", function()
    local _, err = maki.fs.read("/nope")
    return err ~= nil
end)
assert(d() == true, "a parking default reaches the filesystem and comes back")

local l = maki.api.declare_slot("sl", function(x) return x end)
maki.api.set_slot("sl", function(prev, x)
    local _, err = maki.fs.read("/nope")
    return prev(x .. tostring(err ~= nil))
end)
assert(l("v") == "vtrue", l("v"))
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
maki.api.declare_slot("priced", function() return 1 end, { capability = { "net" } })
local slots = maki.api.get_slots()
local gs = slots["gs"]
assert(gs.declared == true and gs.owner == "slots_introspect", tostring(gs.owner))
assert(#gs.fillers == 1 and gs.fillers[1] == "slots_introspect")
assert(gs.capability == nil, "naming no price is not the same as naming an empty one")
local priced = slots["priced"].capability
assert(#priced == 1 and priced[1] == "net", tostring(priced[1]))
local orphan = slots["orphan_slot"]
assert(orphan.declared == false and orphan.owner == nil)
assert(orphan.fillers[1] == "slots_introspect")
"#,
    );
}

/// The owner hands its callable out, because a slot is only observable from
/// the far end of a call.
const RENDER_OWNER: &str = r#"
local render = maki.api.declare_slot("owner.render", function(text) return text end)
maki.api.exec_autocmds("SlotShare", { data = { callable = render } })
"#;

/// The owner prices the slot at what its default actually reaches, so a layer
/// pays for that and not for everything.
const RENDER_OWNER_NET: &str = r#"
local render = maki.api.declare_slot("owner.render", function(text) return text end, {
  capability = { "net" },
})
maki.api.exec_autocmds("SlotShare", { data = { callable = render } })
"#;

/// Nothing the default does with {text} borrows anything, and the owner is the
/// one who can say so.
const RENDER_OWNER_FREE: &str = r#"
local render = maki.api.declare_slot("owner.render", function(text) return text end, {
  capability = {},
})
maki.api.exec_autocmds("SlotShare", { data = { callable = render } })
"#;

/// `set_slot` before `declare_slot`, from a plugin granted nothing: the shape
/// `init.lua` has, since a config with no `plugin.toml` beside it is denied.
const RENDER_SELF_LAYER_FIRST: &str = r#"
maki.api.set_slot("owner.render", function(prev, text) return prev(text) .. "+self" end)
local render = maki.api.declare_slot("owner.render", function(text) return text end)
maki.api.exec_autocmds("SlotShare", { data = { callable = render } })
"#;

fn render_layer(mark: &str) -> String {
    format!(
        r#"maki.api.set_slot("owner.render", function(prev, text) return prev(text) .. "+{mark}" end)"#
    )
}

/// The hole: a plugin nobody trusted steering a chain the owner's callers do.
/// Registering it is free, and the chain drops it when it fires.
#[test]
fn a_foreign_layer_on_a_plugin_slot_costs_full_trust() {
    let (reg, host) = host();
    load(&host, "caller", SLOT_CALLER);
    load(&host, "owner", RENDER_OWNER);

    load_granted(
        &host,
        "attacker",
        &render_layer("attacker"),
        PluginPermissions::denied(),
    );
    assert_eq!(
        exec_tool(&reg, "call_slot"),
        "ok:world",
        "a layer nobody trusted never steers another plugin's chain"
    );

    load_granted(
        &host,
        "trusted_wrapper",
        &render_layer("trusted"),
        PluginPermissions::trusted(),
    );
    assert_eq!(
        exec_tool(&reg, "call_slot"),
        "ok:world+trusted",
        "full trust buys the layer, and the skipped one stays skipped"
    );
}

/// Every permission is what a slot that named no price costs, not what every
/// slot costs: the owner knows whether its default does anything with the
/// arguments, so the owner sets the toll.
#[test]
fn an_owner_prices_its_slot_at_the_capability_it_names() {
    let (reg, host) = host();
    load(&host, "caller", SLOT_CALLER);
    load(&host, "owner", RENDER_OWNER_NET);

    load_granted(
        &host,
        "attacker",
        &render_layer("attacker"),
        PluginPermissions::denied(),
    );
    assert_eq!(
        exec_tool(&reg, "call_slot"),
        "ok:world",
        "a narrower price is still a price"
    );

    load_granted(&host, "netonly", &render_layer("net"), only_net());
    assert_eq!(
        exec_tool(&reg, "call_slot"),
        "ok:world+net",
        "the named capability is the whole toll, not a floor"
    );
}

/// A slot whose arguments are inert costs nothing, which is the case full
/// trust priced wrong before the owner had any way to say so.
#[test]
fn an_owner_can_declare_its_slot_free_to_layer() {
    let (reg, host) = host();
    load(&host, "caller", SLOT_CALLER);
    load(&host, "owner", RENDER_OWNER_FREE);
    load_granted(
        &host,
        "stranger",
        &render_layer("free"),
        PluginPermissions::denied(),
    );
    assert_eq!(exec_tool(&reg, "call_slot"), "ok:world+free");
}

/// Pricing is not a way to advertise reach nobody granted you.
#[test]
fn a_plugin_cannot_price_a_slot_in_a_capability_it_lacks() {
    let (_reg, host) = host();
    let err = host
        .load_source_with_permissions("poor", RENDER_OWNER_NET, PluginPermissions::denied())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("prices layers at") && err.contains("not granted"),
        "unexpected error: {err}"
    );
}

/// Otherwise the price is advisory: wait for the owner to unload, re-declare
/// its name free, and inherit the layers, and the callers, that trusted the
/// old one.
#[test]
fn an_unloaded_owner_keeps_its_slot_name() {
    let (_reg, host) = host();
    load(&host, "owner", RENDER_OWNER);
    host.unload("owner").unwrap();

    let err = host
        .load_source("squatter", RENDER_OWNER_FREE)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("already declared by 'owner'"),
        "unexpected error: {err}"
    );
    load(&host, "owner", RENDER_OWNER_FREE);
}

/// Fire time reads what the last load granted the plugin, so narrowing a
/// layer's reach costs a reload rather than a restart.
#[test]
fn narrowing_a_layers_grant_drops_it_from_the_next_call() {
    let (reg, host) = host();
    load(&host, "caller", SLOT_CALLER);
    load(&host, "owner", RENDER_OWNER);
    load_granted(
        &host,
        "wrapper",
        &render_layer("wrap"),
        PluginPermissions::trusted(),
    );
    assert_eq!(exec_tool(&reg, "call_slot"), "ok:world+wrap");

    load_granted(&host, "wrapper", &render_layer("wrap"), all_but_run());
    assert_eq!(
        exec_tool(&reg, "call_slot"),
        "ok:world",
        "almost all is not all, and the next call is where it shows"
    );
}

/// Registration says nothing about entitlement, for host slots and plugin
/// slots alike: what the plugin holds is read when the chain fires.
#[test]
fn a_denied_plugin_registers_a_host_slot_layer() {
    let (_reg, host) = host();
    load_granted(
        &host,
        "denied_host_layer",
        &format!(
            r#"
{}
local fillers = maki.api.get_slots()["tool.bash.input"].fillers
assert(#fillers == 1 and fillers[1] == "denied_host_layer", tostring(fillers[1]))
"#,
            layer("bash", HookStage::Input, "return prev(value, ctx)")
        ),
        PluginPermissions::denied(),
    );
}

/// Registration order stops mattering: by the time the chain fires, the plugin
/// that filled the orphan owns it, and an owner steers its own chain for free.
#[test]
fn a_denied_plugin_wraps_the_slot_it_declares_afterwards() {
    let (reg, host) = host();
    load(&host, "caller", SLOT_CALLER);
    load_granted(
        &host,
        "owner",
        RENDER_SELF_LAYER_FIRST,
        PluginPermissions::denied(),
    );
    assert_eq!(exec_tool(&reg, "call_slot"), "ok:world+self");
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
