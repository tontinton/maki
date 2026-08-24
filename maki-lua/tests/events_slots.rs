use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use maki_agent::cancel::CancelToken;
use maki_agent::tools::hook::{self, Authority, HookCall, HookStage, Verdict};
use maki_agent::tools::{CallOrigin, ToolRegistry};
use maki_lua::{Permission, PluginHost, PluginPermissions, SessionEndReason};
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

#[test]
fn host_slot_names_are_reserved() {
    let (_reg, host) = host();
    let err = host
        .load_source(
            "squatter",
            r#"maki.api.declare_slot("tool.bash.input", function(i) return i end)"#,
        )
        .expect_err("declaring a host slot must fail");
    assert!(format!("{err}").contains("host owned"), "{err}");
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

#[test]
fn set_slot_on_another_plugins_slot_requires_full_trust() {
    let (_reg, host) = host();
    load(
        &host,
        "owner",
        r#"maki.api.declare_slot("task.tools", function(x) return x end)"#,
    );

    let err = host
        .load_source_with_permissions(
            "attacker",
            r#"maki.api.set_slot("task.tools", function(prev, x) return prev({}) end)"#,
            PluginPermissions::denied(),
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("requires full trust"),
        "expected trust error, got: {err}"
    );

    load_granted(
        &host,
        "trusted_wrapper",
        r#"maki.api.set_slot("task.tools", function(prev, x) return prev(x) end)"#,
        PluginPermissions::trusted(),
    );

    load(
        &host,
        "self_wrapper",
        r#"
maki.api.declare_slot("self.slot", function(x) return x end)
maki.api.set_slot("self.slot", function(prev, x) return prev(x) end)
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
