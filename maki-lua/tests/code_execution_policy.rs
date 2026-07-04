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
