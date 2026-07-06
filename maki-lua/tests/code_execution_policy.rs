//! Tests the code_execution plugin's interpreter visibility: one predicate
//! gates both `describe` text and the handler's fn-map, so what the model
//! sees is exactly what the interpreter can call.

use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{DescriptionContext, ToolAudience, ToolContext, ToolFilter, ToolRegistry};
use maki_lua::PluginHost;

const CODE_EXECUTION_SRC: &str = include_str!("../../plugins/code_execution/init.lua");

const ECHO_PREFIX: &str = "echo:";
const TASK_PREFIX: &str = "task:";
const WORKFLOW_NOTE_SUBSTR: &str = "Workflow mode: orchestrate subagents";
const INTERP_ECHO_SIG: &str = "- interp_echo(msg: str, count: int = None, flag: bool = None, items: list = None, raw: any = None) -> str";
const WF_TASK_SIG: &str = "- wf_task(prompt: str, model_tier: str = None) -> str";
const SUB_TOOL_SIG: &str = "- sub_tool() -> str";

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
    name = "sub_tool",
    description = "subagent fixture",
    audiences = {{ "general_sub", "interpreter" }},
    schema = {{ type = "object", properties = {{}}, additionalProperties = false }},
    handler = function() return "" end,
}})
"#
    )
}

fn setup() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("code_execution", CODE_EXECUTION_SRC)
        .expect("real plugin should load");
    host.load_source("policy_fixtures", &fixture_plugin())
        .expect("fixture plugin should load");
    (reg, host)
}

/// Uses the global native registry because `interpreter_bridge::dispatch` does.
/// Safe: nextest runs each test in its own process.
fn setup_native() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::clone(ToolRegistry::native_arc());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("code_execution", CODE_EXECUTION_SRC)
        .expect("real plugin should load");
    host.load_source("policy_fixtures", &fixture_plugin())
        .expect("fixture plugin should load");
    (reg, host)
}

fn describe(
    reg: &ToolRegistry,
    filter: &ToolFilter,
    audience: ToolAudience,
    workflow: bool,
) -> String {
    reg.get("code_execution")
        .expect("code_execution registered")
        .tool
        .description(&DescriptionContext {
            filter,
            audience,
            workflow,
        })
        .into_owned()
}

fn exec_code(reg: &ToolRegistry, ctx: &ToolContext, code: &str) -> Result<String, String> {
    let entry = reg
        .get("code_execution")
        .expect("code_execution registered");
    let inv = entry
        .tool
        .parse(&serde_json::json!({ "code": code, "timeout": 10 }))
        .expect("parse failed");
    smol::block_on(async { inv.execute(ctx).await })
        .output
        .map(|out| match out {
            maki_agent::ToolOutput::Plain(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

fn stub_ctx_for(reg: &Arc<ToolRegistry>, mode: &AgentMode) -> ToolContext {
    let mut ctx = stub_ctx(mode);
    ctx.registry = Arc::clone(reg);
    ctx
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
    let (reg, _host) = setup_native();
    let ctx = stub_ctx_for(&reg, &AgentMode::Build);
    let out = exec_code(
        &reg,
        &ctx,
        "result = await interp_echo(msg='hi')\nprint(result)",
    )
    .expect("advertised tool must be callable");
    assert!(out.contains(&format!("{ECHO_PREFIX}hi")), "got: {out}");
}

#[test]
fn workflow_tool_not_callable_when_workflow_false() {
    let (reg, _host) = setup_native();
    let ctx = stub_ctx_for(&reg, &AgentMode::Build);
    let err = exec_code(&reg, &ctx, "await wf_task(prompt='x')")
        .expect_err("workflow tool must not be in the fn-map when workflow=false");
    assert!(err.contains("wf_task"), "got: {err}");
}

/// Regression guard: the old `ctx:agent_context()` take() used to reset
/// audience/workflow reads. That accessor is gone now, this makes sure
/// workflow tools stay callable when `ctx.workflow = true`.
#[test]
fn workflow_tool_callable_when_workflow_true() {
    let (reg, _host) = setup_native();
    let mut ctx = stub_ctx_for(&reg, &AgentMode::Build);
    ctx.workflow = true;
    let out = exec_code(
        &reg,
        &ctx,
        "result = await wf_task(prompt='x')\nprint(result)",
    )
    .expect("workflow tool must be callable when workflow=true");
    assert!(out.contains(&format!("{TASK_PREFIX}x")), "got: {out}");
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

fn restore_lines_with(code: &str, output: &str, is_error: bool, expanded: bool) -> Vec<String> {
    let (_reg, host) = setup();
    let eh = host.event_handle().expect("event handle");
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
            expanded,
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
    restore_lines_with("print('x')", output, is_error, false)
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
    let lines = restore_lines_with(&code, "out1", false, true);
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
