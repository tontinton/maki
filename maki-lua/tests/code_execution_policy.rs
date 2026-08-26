//! Tests the code_execution plugin's interpreter visibility: one predicate
//! gates both `describe` text and the handler's fn-map, so what the model
//! sees is exactly what the interpreter can call.

use std::collections::HashMap;
use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::mcp::McpSession;
use maki_agent::mcp::test_support::stub_session;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{
    DescriptionContext, ToolAudience, ToolContext, ToolFilter, ToolRegistry, local_tool,
};
use maki_lua::PluginHost;

const CODE_EXECUTION_SRC: &str = include_str!("../../plugins/code_execution/init.lua");

const ECHO_PREFIX: &str = "echo:";
const FAIL_MSG: &str = "fixture blew up";
const ERROR_PREFIX: &str = "[ERROR] ";
const GATHER_HINT_SUBSTR: &str = "`gather(...)` keeps the other results";
const TASK_PREFIX: &str = "task:";
const WORKFLOW_NOTE_SUBSTR: &str = "Workflow mode: orchestrate subagents";
const INTERP_ECHO_SIG: &str = "- interp_echo(msg: str, count: int = None, flag: bool = None, items: list = None, raw: any = None) -> str";
const WF_TASK_SIG: &str = "- wf_task(prompt: str, model_tier: str = None) -> str";
const SUB_TOOL_SIG: &str = "- sub_tool() -> str";
const MCP_NOTE_SUBSTR: &str = "callable here too";
const MCP_TOOL_QUALIFIED: &str = "srv.fetch_issue";
const MCP_TOOL_WIRE: &str = "srv__fetch_issue";
const GATED_QUALIFIED: &str = "srv.gated";
const GATED_WIRE: &str = "srv__gated";
/// The stub transport fails every request with this, so seeing it proves the
/// call reached MCP instead of dying at name lookup.
const MCP_REACHED_ERR: &str = "unknown MCP tool";
const NAME_ERROR: &str = "NameError";
const CODE_EXECUTION: &str = "code_execution";
const TOOLS_MCP_PROBE: &str = "tools_mcp_probe";
/// Stands in for an ACP client tool: dispatched from the context, not the
/// registry.
const CLIENT_TOOL: &str = "client_probe";
const CLIENT_TOOL_OUT: &str = "client ran";
const MAIN_ONLY_NAME: &str = "main_only_probe";

fn fixture_plugin() -> String {
    format!(
        r#"
maki.api.register_tool({{
    name = "wf_task",
    description = "workflow-only fixture",
    audiences = {{ "main", "workflow" }},
    schema = {{
        type = "object",
        required = {{ "prompt" }},
        properties = {{
            prompt = {{ type = "string" }},
            model_tier = {{ type = "string" }},
        }},
    }},
    handler = function(input) return "{TASK_PREFIX}" .. input.prompt end,
}})
maki.api.register_tool({{
    name = "interp_echo",
    description = "interpreter fixture",
    audiences = {{ "main", "interpreter" }},
    schema = {{
        type = "object",
        required = {{ "msg" }},
        properties = {{
            msg = {{ type = "string" }},
            count = {{ type = "integer" }},
            flag = {{ type = "boolean" }},
            items = {{ type = "array", items = {{ type = "string" }} }},
            raw = {{ description = "no type, maps to any" }},
        }},
    }},
    handler = function(input) return "{ECHO_PREFIX}" .. input.msg end,
}})
maki.api.register_tool({{
    name = "interp_fail",
    description = "failing interpreter fixture",
    audiences = {{ "main", "interpreter" }},
    schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
    handler = function() return {{ llm_output = "{FAIL_MSG}", is_error = true }} end,
}})
maki.api.register_tool({{
    name = "{GATED_WIRE}",
    description = "registry tool wearing an MCP wire name",
    audiences = {{ "main", "interpreter" }},
    schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
    handler = function() return "" end,
}})
maki.api.register_tool({{
    name = "{TOOLS_MCP_PROBE}",
    description = "returns the code_execution description agent.tools built",
    audiences = {{ "main" }},
    schema = {{
        type = "object",
        required = {{ "mcp" }},
        properties = {{ mcp = {{ type = "boolean" }} }},
        additionalProperties = false,
    }},
    handler = function(input, ctx)
        local defs, err = maki.agent.tools(ctx, {{ audience = "main", mcp = input.mcp }})
        if err then return {{ llm_output = err, is_error = true }} end
        for _, d in ipairs(defs) do
            if d.name == "{CODE_EXECUTION}" then return d.description end
        end
        return {{ llm_output = "{CODE_EXECUTION} missing", is_error = true }}
    end,
}})
maki.api.register_tool({{
    name = "{MAIN_ONLY_NAME}",
    description = "registry tool a client tool may shadow",
    audiences = {{ "main" }},
    schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
    handler = function() return "" end,
}})
maki.api.register_tool({{
    name = "sub_tool",
    description = "subagent fixture",
    audiences = {{ "general_sub", "interpreter" }},
    schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
    handler = function() return "" end,
}})
"#
    )
}

fn setup_with(reg: Arc<ToolRegistry>) -> (Arc<ToolRegistry>, PluginHost) {
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("code_execution", CODE_EXECUTION_SRC)
        .expect("real plugin should load");
    host.load_source("policy_fixtures", &fixture_plugin())
        .expect("fixture plugin should load");
    (reg, host)
}

fn setup() -> (Arc<ToolRegistry>, PluginHost) {
    setup_with(Arc::new(ToolRegistry::new()))
}

/// The plugin lists callable tools through `maki.api.get_tools()`, which reads
/// the global registry. Safe: nextest runs each test in its own process.
fn setup_native() -> (Arc<ToolRegistry>, PluginHost) {
    setup_with(Arc::clone(ToolRegistry::global_arc()))
}

fn describe(
    reg: &ToolRegistry,
    filter: &ToolFilter,
    audience: ToolAudience,
    workflow: bool,
) -> String {
    reg.get(CODE_EXECUTION)
        .expect("code_execution registered")
        .tool
        .description(&DescriptionContext {
            filter,
            audience,
            workflow,
            mcp: false,
        })
        .into_owned()
}

fn run_tool(
    reg: &ToolRegistry,
    ctx: &ToolContext,
    name: &str,
    input: serde_json::Value,
) -> Result<String, String> {
    let entry = reg.get(name).unwrap_or_else(|| panic!("{name} registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    smol::block_on(async { inv.execute(ctx).await })
        .output
        .map(|out| match out {
            maki_agent::ToolOutput::Plain(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

fn exec_code(reg: &ToolRegistry, ctx: &ToolContext, code: &str) -> Result<String, String> {
    run_tool(
        reg,
        ctx,
        CODE_EXECUTION,
        serde_json::json!({ "code": code, "timeout": 10 }),
    )
}

fn run_code_in(code: &str, workflow: bool) -> Result<String, String> {
    let (reg, _host) = setup_native();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(&reg);
    ctx.workflow = workflow;
    exec_code(&reg, &ctx, code)
}

fn run_code(code: &str) -> Result<String, String> {
    run_code_in(code, false)
}

#[test]
fn describe_main_hides_workflow_and_sub_tools() {
    let (reg, _host) = setup();
    let desc = describe(&reg, &ToolFilter::All, ToolAudience::MAIN, false);
    assert!(
        desc.lines().any(|l| l == INTERP_ECHO_SIG),
        "expected exact line {INTERP_ECHO_SIG:?} in: {desc}"
    );
    assert!(!desc.contains("wf_task"), "got: {desc}");
    assert!(!desc.contains("sub_tool"), "got: {desc}");
    assert!(!desc.contains(WORKFLOW_NOTE_SUBSTR), "got: {desc}");
}

#[test]
fn describe_workflow_adds_workflow_tools_and_note() {
    let (reg, _host) = setup();
    let desc = describe(&reg, &ToolFilter::All, ToolAudience::MAIN, true);
    assert!(desc.contains(WF_TASK_SIG), "got: {desc}");
    assert!(desc.contains(WORKFLOW_NOTE_SUBSTR), "got: {desc}");
    assert!(!desc.contains("sub_tool"), "got: {desc}");
}

#[test]
fn describe_general_sub_scopes_to_sub_audience() {
    let (reg, _host) = setup();
    let desc = describe(&reg, &ToolFilter::All, ToolAudience::GENERAL_SUB, false);
    assert!(desc.contains(SUB_TOOL_SIG), "got: {desc}");
    assert!(!desc.contains("interp_echo"), "got: {desc}");
    assert!(!desc.contains("wf_task"), "got: {desc}");
}

#[test]
fn except_filter_removes_tool_from_description() {
    let (reg, _host) = setup();
    let filter = ToolFilter::AllExcept(vec!["interp_echo".to_owned()]);
    let desc = describe(&reg, &filter, ToolAudience::MAIN, false);
    assert!(!desc.contains("interp_echo"), "got: {desc}");
}

#[test]
fn interpreter_calls_advertised_tool_end_to_end() {
    let out = run_code("result = await interp_echo(msg='hi')\nprint(result)")
        .expect("advertised tool must be callable");
    assert!(out.contains(&format!("{ECHO_PREFIX}hi")), "got: {out}");
}

/// The list form covers a model that forgets the `*`. It must run rather than
/// cost a TypeError round-trip.
#[test_case::test_case("gather(interp_echo(msg='hi'), interp_fail())" ; "varargs")]
#[test_case::test_case("gather([interp_echo(msg='hi'), interp_fail()])" ; "single_list")]
fn gather_keeps_sibling_results_when_one_call_fails(call: &str) {
    let out = run_code(&format!("ok, bad = await {call}\nprint(ok)\nprint(bad)"))
        .expect("a failed call must not fail the script");
    assert!(out.contains(&format!("{ECHO_PREFIX}hi")), "got: {out}");
    assert!(
        out.contains(&format!("{ERROR_PREFIX}interp_fail: {FAIL_MSG}")),
        "failed call must name the tool and its error: {out}"
    );
}

/// Only a failed tool call belongs in the results. The script's own mistake
/// must stop the run instead of hiding as an `[ERROR]` entry.
#[test]
fn gather_lets_script_errors_through() {
    let err = run_code("await gather(interp_echo(msg='hi'), 'not a call')")
        .expect_err("awaiting a non-call must raise");
    assert!(!err.contains(ERROR_PREFIX), "got: {err}");
}

/// `asyncio.gather` still cancels its siblings, so its error is the one place
/// worth pointing at the wrapper. A bare await is fail-fast on purpose and
/// must not nag.
#[test_case::test_case("await interp_fail()", false ; "plain_await_stays_fail_fast")]
#[test_case::test_case("await asyncio.gather(interp_echo(msg='hi'), interp_fail())", true ; "asyncio_gather_points_at_the_wrapper")]
fn failed_call_error_names_the_tool_and_hints_only_for_asyncio_gather(code: &str, hint: bool) {
    let err = run_code(code).expect_err("a failed call must raise");
    assert!(
        err.contains("interp_fail") && err.contains(FAIL_MSG),
        "got: {err}"
    );
    assert_eq!(err.contains(GATHER_HINT_SUBSTR), hint, "got: {err}");
}

/// The model counts lines in the code it wrote, so the preamble it never sees
/// must not shift them. Guards the plugin passing the preamble as its own
/// option instead of pasting it onto the code.
#[test]
fn traceback_lines_are_numbered_from_the_users_first_line() {
    let err = run_code("x = 1\nprint(boom_undefined)").expect_err("undefined name must error");
    assert!(err.contains("line 2, in <module>"), "got: {err}");
}

#[test]
fn workflow_tool_not_callable_when_workflow_false() {
    let err = run_code("await wf_task(prompt='x')")
        .expect_err("workflow tool must not be in the fn-map when workflow=false");
    assert!(err.contains("wf_task"), "got: {err}");
}

/// Regression guard: the old `ctx:agent_context()` take() used to reset
/// audience/workflow reads, leaving workflow tools uncallable.
#[test]
fn workflow_tool_callable_when_workflow_true() {
    let out = run_code_in("result = await wf_task(prompt='x')\nprint(result)", true)
        .expect("workflow tool must be callable when workflow=true");
    assert!(out.contains(&format!("{TASK_PREFIX}x")), "got: {out}");
}

// --- context tools (MCP, client tools) ---

fn run_code_with_mcp(
    code: &str,
    mcp: McpSession,
    audience: ToolAudience,
) -> Result<String, String> {
    let (reg, _host) = setup_native();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(&reg);
    ctx.audience = audience;
    ctx.mcp = Some(mcp);
    exec_code(&reg, &ctx, code)
}

/// A caller that drops MCP for a subagent must not hand it a description
/// promising MCP tools that subagent cannot call.
#[test_case::test_case(true ; "session_keeps_mcp")]
#[test_case::test_case(false ; "session_drops_mcp")]
fn agent_tools_describes_mcp_only_when_the_session_keeps_it(enabled: bool) {
    let (reg, _host) = setup_native();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(&reg);
    ctx.mcp = Some(stub_session(&[(MCP_TOOL_QUALIFIED, "")]));
    let desc = run_tool(
        &reg,
        &ctx,
        TOOLS_MCP_PROBE,
        serde_json::json!({ "mcp": enabled }),
    )
    .expect("probe must describe code_execution");
    assert_eq!(
        desc.matches(MCP_NOTE_SUBSTR).count(),
        usize::from(enabled),
        "got: {desc}"
    );
}

/// The stub leaves the tool deferred, so binding it means reading the MCP index
/// rather than the request's tool array.
#[test]
fn deferred_mcp_tool_is_callable_from_the_sandbox() {
    let err = run_code_with_mcp(
        &format!("await {MCP_TOOL_WIRE}()"),
        stub_session(&[(MCP_TOOL_QUALIFIED, "")]),
        ToolAudience::MAIN,
    )
    .expect_err("the stub transport fails every call");
    assert!(err.contains(MCP_REACHED_ERR), "got: {err}");
    assert!(!err.contains(NAME_ERROR), "got: {err}");
}

fn run_code_with_client_tool(code: &str, name: &str) -> Result<String, String> {
    let (reg, _host) = setup_native();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(&reg);
    ctx.local_tools = Arc::new(HashMap::from([(
        name.to_owned(),
        local_tool(|_, _| Box::pin(async { Ok(CLIENT_TOOL_OUT.to_owned()) })),
    )]));
    exec_code(&reg, &ctx, code)
}

/// A nested call resolves names exactly like the model's own, so a client tool
/// the registry never heard of is callable. One that shadows a registry entry is
/// not: that entry is `main`-only, and precedence must not launder its audience
/// into the interpreter.
#[test]
fn client_tool_binds_unless_the_registry_holds_the_name() {
    let out = run_code_with_client_tool(&format!("print(await {CLIENT_TOOL}())"), CLIENT_TOOL)
        .expect("a client tool must be callable");
    assert!(out.contains(CLIENT_TOOL_OUT), "got: {out}");

    let err = run_code_with_client_tool(&format!("await {MAIN_ONLY_NAME}()"), MAIN_ONLY_NAME)
        .expect_err("a shadowed registry name must not be bound at all");
    assert!(err.contains(NAME_ERROR), "got: {err}");
}

/// A registry tool wears the same wire name as an MCP tool, and its audience
/// keeps it out of this sandbox. Binding the name would hand the script the tool
/// anyway, through MCP's back door.
#[test]
fn mcp_name_never_reaches_an_audience_gated_registry_tool() {
    let err = run_code_with_mcp(
        &format!("await {GATED_WIRE}()"),
        stub_session(&[(GATED_QUALIFIED, "")]),
        ToolAudience::GENERAL_SUB,
    )
    .expect_err("a gated name must not be bound at all");
    assert!(err.contains(NAME_ERROR), "got: {err}");
}

// --- script rendering ---

const SCRIPT_TOOL_ID: &str = "ce-script-1";
const MAX_SCRIPT_LINES: usize = 2000;
const EXPAND_NOTICE: &str = "click to expand";
const DIVIDER_LINE: &str = "──────";

fn event_ctx(reg: &Arc<ToolRegistry>) -> (ToolContext, flume::Receiver<maki_agent::Envelope>) {
    let (tx, rx) = flume::unbounded::<maki_agent::Envelope>();
    let event_tx = maki_agent::EventSender::new(tx, 0);
    let mut ctx = maki_agent::tools::test_support::stub_ctx_with(
        &AgentMode::Build,
        Some(&event_tx),
        Some(SCRIPT_TOOL_ID),
    );
    ctx.registry = Arc::clone(reg);
    (ctx, rx)
}

fn parse_code(reg: &ToolRegistry, code: &str) -> Box<dyn maki_agent::tools::ToolInvocation> {
    reg.get("code_execution")
        .expect("code_execution registered")
        .tool
        .parse(&serde_json::json!({ "code": code, "timeout": 10 }))
        .expect("parse failed")
}

fn start_preview_text(code: &str) -> String {
    let (reg, _host) = setup();
    let inv = parse_code(&reg, code);
    let (ctx, rx) = event_ctx(&reg);
    smol::block_on(inv.start(&ctx));
    let body = rx
        .drain()
        .find_map(|env| match env.event {
            maki_agent::AgentEvent::LiveToolBuf { id, body } if id == SCRIPT_TOOL_ID => Some(body),
            _ => None,
        })
        .expect("start must publish a preview buf");
    body.take().text()
}

#[test]
fn start_preview_contains_numbered_script_lines() {
    let text = start_preview_text("print('a')\nprint('b')");
    assert!(text.contains("1 print('a')"), "got: {text}");
    assert!(text.contains("2 print('b')"), "got: {text}");
}

#[test_case::test_case(MAX_SCRIPT_LINES, false ; "at_cap_shows_all_lines_without_notice")]
#[test_case::test_case(MAX_SCRIPT_LINES + 1, true ; "over_cap_hides_excess_with_notice")]
fn start_preview_caps_script_at_max_lines(total: usize, truncated: bool) {
    let code: String = (1..=total)
        .map(|i| format!("print({i})"))
        .collect::<Vec<_>>()
        .join("\n");
    let text = start_preview_text(&code);
    assert!(
        text.contains(&format!("print({MAX_SCRIPT_LINES})")),
        "line at the cap must be visible"
    );
    assert!(
        !text.contains(&format!("print({})", MAX_SCRIPT_LINES + 1)),
        "line beyond the cap must be hidden"
    );
    assert_eq!(
        text.contains(EXPAND_NOTICE),
        truncated,
        "expand notice must appear iff truncated, tail: {}",
        &text[text.len().saturating_sub(200)..]
    );
}

/// The async highlight task may snapshot mid-run, but the reply's
/// `LiveToolBuf` and final `ToolSnapshot` always come after it, so the last
/// body event holds the final content whatever the highlight timing.
fn final_body_text(rx: &flume::Receiver<maki_agent::Envelope>) -> String {
    rx.drain()
        .filter_map(|env| match env.event {
            maki_agent::AgentEvent::ToolSnapshot { id, snapshot, .. } if id == SCRIPT_TOOL_ID => {
                Some(snapshot.text())
            }
            maki_agent::AgentEvent::LiveToolBuf { id, body } if id == SCRIPT_TOOL_ID => {
                Some(body.take().text())
            }
            _ => None,
        })
        .last()
        .expect("handler must publish a body")
}

/// Some call paths skip `start`, so `handler` must render the script itself.
#[test]
fn handler_renders_script_when_start_never_ran() {
    let (reg, _host) = setup_native();
    let inv = parse_code(&reg, "print('hi')");
    let (ctx, rx) = event_ctx(&reg);
    smol::block_on(inv.execute(&ctx))
        .output
        .expect("execute ok");
    let text = final_body_text(&rx);
    assert!(text.contains("1 print('hi')"), "script section: {text}");
    assert!(text.contains("\nhi"), "interpreter output below: {text}");
}

/// A regression here shows phantom numbered lines, or crashes `start`, which
/// silently swallows the preview since start errors are only logged.
#[test_case::test_case("print('a')\n\n\n" ; "trailing_newlines")]
#[test_case::test_case("" ; "empty_code")]
fn start_preview_renders_single_line(code: &str) {
    let text = start_preview_text(code);
    assert_eq!(
        text.trim_end().lines().count(),
        2,
        "one script line + divider, no phantom lines: {text:?}"
    );
    assert_eq!(text.trim_end().lines().last(), Some(DIVIDER_LINE));
}

#[test]
fn handler_error_keeps_script_and_drops_waiting_notice() {
    let (reg, _host) = setup_native();
    let inv = parse_code(&reg, "print(boom_undefined)");
    let (ctx, rx) = event_ctx(&reg);
    let err = smol::block_on(inv.execute(&ctx))
        .output
        .expect_err("undefined name must error");
    let text = final_body_text(&rx);

    assert!(
        text.contains("1 print(boom_undefined)"),
        "script header must survive the error path: {text}"
    );
    assert!(
        text.contains(err.trim_end()),
        "error must render below the script, err: {err:?}, body: {text}"
    );
    assert!(
        !text.contains("Waiting for output"),
        "placeholder must be cleared on error: {text}"
    );
}

fn restore_lines_with(code: &str, output: &str, is_error: bool, clicks: Vec<usize>) -> Vec<String> {
    let (_reg, host) = setup();
    let eh = host.event_handle();
    let (tx, rx) = flume::unbounded::<maki_agent::Envelope>();
    eh.request_restore(
        maki_lua::RestoreItem {
            tool: Arc::from("code_execution"),
            tool_use_id: SCRIPT_TOOL_ID.into(),
            output: output.into(),
            input: serde_json::json!({ "code": code }),
            is_error,
            tool_output_lines: maki_config::ToolOutputLines::default(),
            theme_gen: None,
            clicks,
            state: None,
        },
        maki_agent::EventSender::new(tx, 0),
    );
    let snapshot = loop {
        let env = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("restore must emit a snapshot");
        if let maki_agent::AgentEvent::ToolSnapshot { id, snapshot, .. } = env.event
            && id == SCRIPT_TOOL_ID
        {
            break snapshot;
        }
    };
    snapshot.text().lines().map(str::to_owned).collect()
}

fn restore_lines(output: &str, is_error: bool) -> Vec<String> {
    restore_lines_with("print('x')", output, is_error, Vec::new())
}

#[test_case::test_case("out1\nout2", false, &["out1", "out2"] ; "output_lines_below_divider")]
#[test_case::test_case("boom", true, &["boom"] ; "error_output_below_divider")]
#[test_case::test_case("(no output)", false, &["No output"] ; "no_output_marker_renders_label")]
fn restore_body_is_script_divider_output(output: &str, is_error: bool, tail: &[&str]) {
    let mut expected = vec!["1 print('x')".to_owned(), DIVIDER_LINE.to_owned()];
    expected.extend(tail.iter().map(|s| (*s).to_owned()));
    assert_eq!(restore_lines(output, is_error), expected);
}

/// Restoring a session saved with the script expanded replays the buf's
/// click handler, so the header must rebuild past `MAX_SCRIPT_LINES`.
#[test]
fn restore_expanded_shows_full_script_beyond_cap() {
    let over = MAX_SCRIPT_LINES + 1;
    let code: String = (1..=over)
        .map(|i| format!("print({i})"))
        .collect::<Vec<_>>()
        .join("\n");
    let lines = restore_lines_with(&code, "out1", false, vec![0]);
    let text = lines.join("\n");
    assert!(
        text.contains(&format!("print({over})")),
        "expanded restore must show lines beyond the cap"
    );
    assert!(
        !text.contains(EXPAND_NOTICE),
        "no truncation notice when expanded"
    );
    assert_eq!(
        lines.last().map(String::as_str),
        Some("out1"),
        "output must stay below the expanded script"
    );
}
