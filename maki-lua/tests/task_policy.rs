//! Tests the task plugin's delegation policy end-to-end: real plugin
//! source, real `maki.json` / `maki.async`, with model I/O replaced by
//! scriptable Lua stubs.

use std::sync::Arc;

use futures_lite::future::poll_once;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{AgentMode, CancelToken, ToolOutput};
use maki_lua::{PluginHost, SessionEndReason};
use maki_storage::id::SessionRef;
use serde_json::{Value, json};

const TASK_PLUGIN_SRC: &str = include_str!("../../plugins/task/init.lua");
const TASK_RUNNER_SRC: &str = include_str!("../../plugins/task/runner.lua");
const TASK_BACKGROUND_SRC: &str = include_str!("../../plugins/task/background.lua");

// Mirrors of the plugin's error contracts and policy numbers.
const STRUCTURED_OUTPUT_TOOL: &str = "structured_output";
const MAX_STRUCTURED_RETRIES: usize = 2;
const MAX_SCHEMA_ERRORS: usize = 3;
const SCHEMA_COMPILE_ERROR: &str = "invalid output_schema";
const SCHEMA_ROOT_ERROR: &str = "output_schema must have type object";
const STRUCTURED_MISSING_ERROR: &str = "subagent finished without calling structured_output";
const STRUCTURED_INVALID_ERROR: &str = "subagent result does not match output_schema";
const SUMMARY_MISSING_ERROR: &str = "subagent finished without providing a summary";
const UNKNOWN_SUBAGENT_ERR: &str = "unknown subagent type: bogus";
const SUB_AGENT_ERROR_PREFIX: &str = "sub-agent error: ";

const TASK_TOOL: &str = "task";
const PROBE_TOOL: &str = "probe";
const WAIT_TOOL: &str = "wait";
const RELEASE_TOOL: &str = "release";
const TASK_PROMPT: &str = "do the thing";
const PLAIN_TEXT: &str = "plain text result";
const RECOVERED_TEXT: &str = "summary after nudge";
/// Mirrors the task plugin's `NUDGE_SUMMARY` wording.
const SUMMARY_NUDGE_FRAGMENT: &str = "concise summary";
const PROMPT_ERR_MSG: &str = "model exploded";
const RAISE_MSG: &str = "stub prompt kaboom";
const PARTIAL_TEXT: &str = "half a transcript";
const CANCELLED_ERR: &str = "cancelled";
const BACKGROUND_STARTED: &str = "Background task started";
const BACKGROUND_CANCELLED_ERR: &str = "background task cancelled";
const SESSION_OPEN_ERR: &str = "session admission failed";
const FINISHED: &str = "finished";
const FAILED: &str = "failed";
/// Mirrors the task plugin's `max_concurrent` default.
const TASK_DEFAULT_MAX_CONCURRENT: u64 = 8;

const SCENARIO_PLAIN: &str = "plain";
const SCENARIO_HAPPY: &str = "happy";
const SCENARIO_INVALID_THEN_VALID: &str = "invalid_then_valid";
const SCENARIO_NEVER_STRUCTURED: &str = "never_structured";
const SCENARIO_INVALID_ONLY: &str = "invalid_only";
const SCENARIO_PROMPT_ERROR: &str = "prompt_error";
const SCENARIO_PARTIAL_ERROR: &str = "partial_error";
const SCENARIO_RAISE: &str = "raise";
const SCENARIO_NO_SUMMARY: &str = "no_summary";
const SCENARIO_NO_SUMMARY_THEN_RECOVER: &str = "no_summary_then_recover";
const SCENARIO_PARKED: &str = "parked";
const SCENARIO_PARKED_SECOND: &str = "parked_second";
const SCENARIO_SESSION_ERROR: &str = "session_error";
const SCENARIO_OPEN_PARKED: &str = "open_parked";

/// Stubs keyed by `opts.name` (the task's `description`). `maki.json` and
/// `maki.async` stay real so schema validation and semaphore behavior are tested.
const STUB_PRELUDE: &str = r#"
recorder = {
  prompts = {}, closed = 0, sessions = 0, acquired = 0, released = 0,
  notices = {}, acquire_attempts = 0,
}
local sessions = {}
local parked = {}
local waiters = {}

-- Every `wait` call rechecks its condition after any recorded step.
local function changed()
  local woken = waiters
  waiters = {}
  for _, done in ipairs(woken) do
    done()
  end
end

-- Parks the caller until the `release` tool resumes it by name.
local function park(name)
  return maki.async.await(1, function(done)
    parked[name] = done
    changed()
  end)
end

-- Spy wrapper: the real semaphore does the work, counters track that every
-- permit is explicitly released (gc would silently hide a leak).
local real_semaphore = maki.async.semaphore
maki.async.semaphore = function(n)
  recorder.sem_size = n
  local sem = real_semaphore(n)
  return {
    acquire = function(self)
      recorder.acquire_attempts = recorder.acquire_attempts + 1
      changed()
      local permit = sem:acquire()
      recorder.acquired = recorder.acquired + 1
      return {
        release = function(p)
          recorder.released = recorder.released + 1
          return permit:release()
        end,
      }
    end,
  }
end

maki.agent.resolve_model = function(ctx, opts)
  recorder.resolve_opts = opts
  return { spec = "test/model" }
end

maki.agent.system_prompt = function(ctx, opts)
  return "sys"
end

maki.agent.tools = function(ctx, opts)
  return {}
end

local behaviors = {}

behaviors.plain = function(sess, msg)
  return { text = "@PLAIN_TEXT@" }
end
behaviors.open_parked = behaviors.plain

behaviors.happy = function(sess, msg)
  local h = sess.opts.local_tools.structured_output.handler
  recorder.first_ack, recorder.first_err = h({ answer = "42" })
  return { text = "raw text ignored" }
end

behaviors.invalid_then_valid = function(sess, msg)
  local h = sess.opts.local_tools.structured_output.handler
  recorder.first_ack, recorder.first_err = h({ answer = 42 })
  recorder.second_ack, recorder.second_err = h({ answer = "42" })
  return { text = "raw text ignored" }
end

behaviors.never_structured = function(sess, msg)
  return { text = "no structured call" }
end

behaviors.invalid_only = function(sess, msg)
  local h = sess.opts.local_tools.structured_output.handler
  recorder.first_ack, recorder.first_err = h({ a = 1, b = 2, c = 3, d = 4 })
  return { text = "still invalid" }
end

behaviors.prompt_error = function(sess, msg)
  return nil, "@PROMPT_ERR@"
end

behaviors.partial_error = function(sess, msg)
  return { text = "@PARTIAL_TEXT@" }, "@CANCELLED_ERR@"
end

behaviors.no_summary = function(sess, msg)
  return { text = "" }
end

behaviors.no_summary_then_recover = function(sess, msg)
  if #recorder.prompts == 1 then
    return { text = "" }
  end
  return { text = "@RECOVERED_TEXT@" }
end

behaviors.raise = function(sess, msg)
  error("@RAISE_MSG@")
end

behaviors.parked = function(sess, msg)
  return park(sess.opts.name)
end
behaviors.parked_second = behaviors.parked

maki.agent.session = function(ctx, opts)
  if opts == nil then
    opts = ctx
    recorder.scope = opts.scope
    recorder.owner = opts.session
  end
  recorder.sessions = recorder.sessions + 1
  if opts.name == "@SESSION_ERROR_SCENARIO@" then
    return nil, "@SESSION_OPEN_ERR@"
  end
  recorder.has_local_tools = opts.local_tools ~= nil
  recorder.thinking = opts.thinking
  local sess = { opts = opts }
  sessions[opts.name] = sess
  function sess:id() return opts.name end
  function sess:prompt(msg)
    recorder.prompts[#recorder.prompts + 1] = msg
    return behaviors[opts.name](self, msg)
  end
  function sess:close()
    if self.closed then return end
    self.closed = true
    recorder.closed = recorder.closed + 1
  end
  if opts.name == "@OPEN_PARKED_SCENARIO@" then
    park(opts.name)
  end
  return sess
end

maki.agent.list = function(opts)
  local out = {}
  for name, session in pairs(sessions) do
    if session.opts.session == opts.session and not session.closed then
      out[#out + 1] = { id = name }
    end
  end
  return out
end

maki.session.notify = function(text, opts)
  recorder.notices[#recorder.notices + 1] = {
    text = text, session = opts.session, wake = opts.wake,
  }
  changed()
  return true
end

maki.api.register_tool({
  name = "wait",
  description = "wait until a step is parked and enough acquires and notices happened",
  schema = {
    type = "object",
    properties = {
      parked = { type = "string" },
      acquires = { type = "integer" },
      notices = { type = "integer" },
    },
  },
  audiences = { "main" },
  handler = function(input)
    local function ready()
      return (not input.parked or parked[input.parked] ~= nil)
        and recorder.acquire_attempts >= (input.acquires or 0)
        and #recorder.notices >= (input.notices or 0)
    end
    while not ready() do
      maki.async.await(1, function(done)
        waiters[#waiters + 1] = done
      end)
    end
    return "ready"
  end,
})

maki.api.register_tool({
  name = "release",
  description = "resume a parked prompt or session open",
  schema = { type = "object", properties = { name = { type = "string" } } },
  audiences = { "main" },
  handler = function(input)
    local done = assert(parked[input.name], "nothing parked")
    parked[input.name] = nil
    done({ text = "@PLAIN_TEXT@" })
    return "released"
  end,
})

maki.api.register_tool({
  name = "probe",
  description = "recorder snapshot",
  schema = { type = "object", properties = {}, additionalProperties = false },
  audiences = { "main" },
  handler = function(input, ctx)
    local snap = {
      sessions = recorder.sessions,
      closed = recorder.closed,
      prompt_count = #recorder.prompts,
      has_local_tools = recorder.has_local_tools,
      thinking = recorder.thinking,
      first_ack = recorder.first_ack,
      first_err = recorder.first_err,
      second_ack = recorder.second_ack,
      second_err = recorder.second_err,
      acquired = recorder.acquired,
      released = recorder.released,
      sem_size = recorder.sem_size,
      notices = recorder.notices,
      notice_count = #recorder.notices,
      scope = recorder.scope,
      owner = recorder.owner,
    }
    if recorder.resolve_opts then
      snap.resolve_opts = recorder.resolve_opts
    end
    if #recorder.prompts > 0 then
      snap.prompts = recorder.prompts
    end
    return (maki.json.encode(snap))
  end,
})
"#;

fn load_task_host() -> (Arc<ToolRegistry>, PluginHost) {
    load_task_host_with_opts(serde_json::Map::new())
}

fn load_background_host() -> (Arc<ToolRegistry>, PluginHost) {
    let mut opts = serde_json::Map::new();
    opts.insert("allow_background".into(), json!(true));
    load_task_host_with_opts(opts)
}

fn load_task_host_with_opts(
    opts: serde_json::Map<String, serde_json::Value>,
) -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let prelude = STUB_PRELUDE
        .replace("@PLAIN_TEXT@", PLAIN_TEXT)
        .replace("@RECOVERED_TEXT@", RECOVERED_TEXT)
        .replace("@PROMPT_ERR@", PROMPT_ERR_MSG)
        .replace("@RAISE_MSG@", RAISE_MSG)
        .replace("@PARTIAL_TEXT@", PARTIAL_TEXT)
        .replace("@CANCELLED_ERR@", CANCELLED_ERR)
        .replace("@SESSION_ERROR_SCENARIO@", SCENARIO_SESSION_ERROR)
        .replace("@OPEN_PARKED_SCENARIO@", SCENARIO_OPEN_PARKED)
        .replace("@SESSION_OPEN_ERR@", SESSION_OPEN_ERR);
    let source = format!(
        r#"{prelude}
local builtin_require = require
local runner
local background
local function require(name)
  if name == "runner" then
    if runner == nil then
      runner = (function()
{TASK_RUNNER_SRC}
      end)()
    end
    return runner
  end
  if name == "background" then
    if background == nil then
      background = (function()
{TASK_BACKGROUND_SRC}
      end)()
    end
    return background
  end
  return builtin_require(name)
end
{TASK_PLUGIN_SRC}
"#
    );
    host.load_source_with_opts("task_policy", &source, opts)
        .unwrap();
    (reg, host)
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: Value) -> Result<String, String> {
    exec_tool_with_ctx(reg, name, input, &stub_ctx(&AgentMode::Build))
}

fn exec_tool_with_ctx(
    reg: &ToolRegistry,
    name: &str,
    input: Value,
    ctx: &ToolContext,
) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    smol::block_on(inv.execute(ctx))
        .output
        .map(|out| match out {
            ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

fn session_ctx(session: &SessionRef) -> ToolContext {
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.session_id = Some(session.clone());
    ctx
}

fn background_input(scenario: &str, output_schema: Option<Value>) -> Value {
    let mut input = task_input(scenario, output_schema);
    input["background"] = json!(true);
    input
}

fn start_background(
    reg: &ToolRegistry,
    ctx: &ToolContext,
    scenario: &str,
    output_schema: Option<Value>,
) {
    let out = exec_tool_with_ctx(
        reg,
        TASK_TOOL,
        background_input(scenario, output_schema),
        ctx,
    )
    .expect("background task admission failed");
    assert!(out.starts_with(BACKGROUND_STARTED), "{out}");
}

/// Blocks until every condition holds: `parked` (a scenario name),
/// `acquires` and `notices` (minimum counts).
fn wait(reg: &ToolRegistry, conditions: Value) {
    exec_tool(reg, WAIT_TOOL, conditions).expect("wait failed");
}

fn release(reg: &ToolRegistry, scenario: &str) {
    exec_tool(reg, RELEASE_TOOL, json!({ "name": scenario })).expect("release failed");
}

fn probe(reg: &ToolRegistry) -> Value {
    let out = exec_tool(reg, PROBE_TOOL, json!({})).expect("probe failed");
    serde_json::from_str(&out).expect("probe returned invalid json")
}

fn task_input(scenario: &str, output_schema: Option<Value>) -> Value {
    let mut input = json!({ "description": scenario, "prompt": TASK_PROMPT });
    if let Some(schema) = output_schema {
        input["output_schema"] = schema;
    }
    input
}

const FULL_MODEL_SPEC: &str = "aperture/ollama/glm-5.2";

/// Advertised to the model, so the wording lives in the docs, not here: what
/// matters is that the property exists and that whatever the model writes
/// reaches the session verbatim, capping being the session's job.
#[test_case::test_case(Some(json!("high")) ; "effort")]
#[test_case::test_case(Some(json!(4096)) ; "token_budget")]
#[test_case::test_case(None ; "inherit_parent_when_omitted")]
fn thinking_forwards_to_session(thinking: Option<Value>) {
    let (reg, _host) = load_task_host();
    let entry = reg.get(TASK_TOOL).expect("task tool missing");
    assert!(entry.tool.schema()["properties"]["thinking"].is_object());
    let mut input = task_input(SCENARIO_PLAIN, None);
    if let Some(thinking) = &thinking {
        input["thinking"] = thinking.clone();
    }

    exec_tool(&reg, TASK_TOOL, input).expect("task failed");
    let snap = probe(&reg);
    assert_eq!(snap.get("thinking"), thinking.as_ref());
}

#[test]
fn model_spec_forwards_full_spec_to_resolve_model() {
    let mut opts = serde_json::Map::new();
    opts.insert("allow_model".into(), json!(true));
    let (reg, _host) = load_task_host_with_opts(opts);
    let mut input = task_input(SCENARIO_PLAIN, None);
    input["model"] = json!(FULL_MODEL_SPEC);
    let out = exec_tool(&reg, TASK_TOOL, input).expect("task with model spec failed");
    assert_eq!(out, PLAIN_TEXT);

    let snap = probe(&reg);
    let opts = snap["resolve_opts"]
        .as_object()
        .expect("resolve_opts missing");
    assert_eq!(opts["spec"], json!(FULL_MODEL_SPEC));
    assert!(
        opts.get("tier").is_none_or(Value::is_null),
        "tier should be unset when only model spec is given"
    );
}

#[test]
fn model_spec_ignored_when_allow_model_off() {
    let (reg, _host) = load_task_host();
    let mut input = task_input(SCENARIO_PLAIN, None);
    input["model"] = json!(FULL_MODEL_SPEC);
    let out = exec_tool(&reg, TASK_TOOL, input).expect("task with model spec failed");
    assert_eq!(out, PLAIN_TEXT);

    let snap = probe(&reg);
    let opts = snap["resolve_opts"]
        .as_object()
        .expect("resolve_opts missing");
    assert!(
        opts.get("spec").is_none_or(Value::is_null),
        "spec should not be forwarded when allow_model is off"
    );
}

fn answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"],
        "additionalProperties": false,
    })
}

/// Four wrong-typed properties, one more than MAX_SCHEMA_ERRORS, so
/// truncation in `bounded_errors` is observable.
fn multi_error_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "a": { "type": "string" },
            "b": { "type": "string" },
            "c": { "type": "string" },
            "d": { "type": "string" },
        },
        "required": ["a", "b", "c", "d"],
    })
}

#[test_case::test_case(json!({"subagent_type": "bogus"}), UNKNOWN_SUBAGENT_ERR ; "unknown_subagent_type")]
#[test_case::test_case(json!({"output_schema": {"type": "object", "properties": {"x": {"type": 42}}}}), SCHEMA_COMPILE_ERROR ; "invalid_output_schema")]
#[test_case::test_case(json!({"output_schema": {"type": "array"}}), SCHEMA_ROOT_ERROR ; "non_object_output_schema")]
fn bad_input_errors_before_any_session(extra: Value, expected_prefix: &str) {
    let (reg, _host) = load_task_host();
    let mut input = task_input(SCENARIO_PLAIN, None);
    for (k, v) in extra.as_object().unwrap() {
        input[k.as_str()] = v.clone();
    }
    let err = exec_tool(&reg, TASK_TOOL, input).unwrap_err();
    assert!(err.starts_with(expected_prefix), "got: {err}");
    let snap = probe(&reg);
    assert_eq!(snap["sessions"], json!(0));
    assert_eq!(snap["prompt_count"], json!(0));
}

fn typeless_answer_schema() -> Value {
    let mut schema = answer_schema();
    schema.as_object_mut().unwrap().remove("type");
    schema
}

#[test_case::test_case(answer_schema() ; "typed_root")]
#[test_case::test_case(typeless_answer_schema() ; "missing_root_type_defaults_to_object")]
fn structured_happy_path_returns_validated_json(schema: Value) {
    let (reg, _host) = load_task_host();
    let out = exec_tool(&reg, TASK_TOOL, task_input(SCENARIO_HAPPY, Some(schema)))
        .expect("structured task failed");
    let parsed: Value = serde_json::from_str(&out).expect("result is not json");
    assert_eq!(parsed, json!({ "answer": "42" }));

    let snap = probe(&reg);
    assert_eq!(snap["sessions"], json!(1));
    assert_eq!(snap["closed"], json!(1));
    assert_eq!(snap["prompt_count"], json!(1));
    assert_eq!(snap["has_local_tools"], json!(true));
    assert!(snap["first_ack"].is_string(), "valid input must be acked");
    assert!(snap.get("first_err").is_none_or(Value::is_null));
    let prompt = snap["prompts"][0].as_str().expect("prompt missing");
    assert!(prompt.starts_with(TASK_PROMPT), "got: {prompt}");
    assert!(
        prompt.contains(STRUCTURED_OUTPUT_TOOL),
        "prompt must point at the structured_output tool: {prompt}"
    );
}

#[test]
fn invalid_then_valid_recovers_within_one_prompt() {
    let (reg, _host) = load_task_host();
    let out = exec_tool(
        &reg,
        TASK_TOOL,
        task_input(SCENARIO_INVALID_THEN_VALID, Some(answer_schema())),
    )
    .expect("task should succeed after inline retry");
    let parsed: Value = serde_json::from_str(&out).expect("result is not json");
    assert_eq!(parsed, json!({ "answer": "42" }));

    let snap = probe(&reg);
    assert!(snap.get("first_ack").is_none_or(Value::is_null));
    let first_err = snap["first_err"].as_str().expect("first_err missing");
    assert!(
        first_err.contains("/answer"),
        "inline error should point at the failing path: {first_err}"
    );
    assert!(snap["second_ack"].is_string(), "valid retry must be acked");
    assert!(snap.get("second_err").is_none_or(Value::is_null));
    assert_eq!(snap["prompt_count"], json!(1));
    assert_eq!(snap["closed"], json!(1));
}

#[test]
fn missing_structured_output_nudges_then_errors() {
    let (reg, _host) = load_task_host();
    let err = exec_tool(
        &reg,
        TASK_TOOL,
        task_input(SCENARIO_NEVER_STRUCTURED, Some(answer_schema())),
    )
    .unwrap_err();
    assert_eq!(err, STRUCTURED_MISSING_ERROR);

    let snap = probe(&reg);
    assert_eq!(snap["prompt_count"], json!(1 + MAX_STRUCTURED_RETRIES));
    for i in 1..=MAX_STRUCTURED_RETRIES {
        let nudge = snap["prompts"][i].as_str().expect("nudge prompt missing");
        assert!(nudge.contains(STRUCTURED_OUTPUT_TOOL), "got: {nudge}");
    }
    assert_eq!(snap["closed"], json!(1));
}

#[test]
fn invalid_only_errors_with_bounded_schema_errors() {
    let (reg, _host) = load_task_host();
    let err = exec_tool(
        &reg,
        TASK_TOOL,
        task_input(SCENARIO_INVALID_ONLY, Some(multi_error_schema())),
    )
    .unwrap_err();
    assert!(err.starts_with(STRUCTURED_INVALID_ERROR), "got: {err}");
    assert_eq!(err.lines().count(), 1 + MAX_SCHEMA_ERRORS, "got: {err}");

    let snap = probe(&reg);
    assert_eq!(snap["prompt_count"], json!(1 + MAX_STRUCTURED_RETRIES));
    let first_err = snap["first_err"].as_str().expect("first_err missing");
    assert_eq!(
        first_err.lines().count(),
        1 + MAX_SCHEMA_ERRORS,
        "inline error must carry at most MAX_SCHEMA_ERRORS validation lines: {first_err}"
    );
}

#[test]
fn prompt_error_maps_to_sub_agent_error() {
    let (reg, _host) = load_task_host();
    let err = exec_tool(&reg, TASK_TOOL, task_input(SCENARIO_PROMPT_ERROR, None)).unwrap_err();
    assert_eq!(err, format!("{SUB_AGENT_ERROR_PREFIX}{PROMPT_ERR_MSG}"));
    let snap = probe(&reg);
    assert_eq!(snap["closed"], json!(1));
}

/// Esc during a sub-agent run: the prompt hands back both an error and
/// whatever the sub-agent managed to say, and half a transcript is worth
/// more to the model than a bare "cancelled".
#[test]
fn interrupted_prompt_reports_the_partial_transcript() {
    let (reg, _host) = load_task_host();
    let err = exec_tool(&reg, TASK_TOOL, task_input(SCENARIO_PARTIAL_ERROR, None)).unwrap_err();
    assert_eq!(
        err,
        format!("sub-agent interrupted ({CANCELLED_ERR}). Partial output:\n{PARTIAL_TEXT}")
    );
}

#[test]
fn plain_path_returns_text_without_local_tools() {
    let (reg, _host) = load_task_host();
    let out = exec_tool(&reg, TASK_TOOL, task_input(SCENARIO_PLAIN, None)).unwrap();
    assert_eq!(out, PLAIN_TEXT);

    let snap = probe(&reg);
    assert_eq!(snap["has_local_tools"], json!(false));
    assert_eq!(snap["prompt_count"], json!(1));
    assert_eq!(snap["prompts"][0], json!(TASK_PROMPT));
    assert_eq!(snap["closed"], json!(1));
}

#[test]
fn no_summary_nudges_then_recovers() {
    let (reg, _host) = load_task_host();
    let out = exec_tool(
        &reg,
        TASK_TOOL,
        task_input(SCENARIO_NO_SUMMARY_THEN_RECOVER, None),
    )
    .unwrap();
    assert_eq!(out, RECOVERED_TEXT);

    let snap = probe(&reg);
    assert_eq!(snap["prompt_count"], json!(2));
    let nudge = snap["prompts"][1].as_str().expect("nudge prompt missing");
    assert!(nudge.contains(SUMMARY_NUDGE_FRAGMENT), "got: {nudge}");
    assert_eq!(snap["closed"], json!(1));
}

#[test]
fn no_summary_errors_after_nudges() {
    let (reg, _host) = load_task_host();
    let err = exec_tool(&reg, TASK_TOOL, task_input(SCENARIO_NO_SUMMARY, None)).unwrap_err();
    assert_eq!(err, SUMMARY_MISSING_ERROR);

    let snap = probe(&reg);
    assert_eq!(snap["prompt_count"], json!(1 + MAX_STRUCTURED_RETRIES));
    assert_eq!(snap["closed"], json!(1));
}

/// Spy counters catch a leaked permit even when gc would silently reclaim it.
#[test]
fn raising_prompt_does_not_leak_semaphore_permit() {
    let (reg, _host) = load_task_host();
    let err = exec_tool(&reg, TASK_TOOL, task_input(SCENARIO_RAISE, None)).unwrap_err();
    assert!(err.contains(RAISE_MSG), "got: {err}");

    let snap = probe(&reg);
    assert_eq!(
        snap["sem_size"],
        json!(TASK_DEFAULT_MAX_CONCURRENT),
        "semaphore not sized from the default max_concurrent option"
    );
    assert_eq!(snap["acquired"], json!(1));
    assert_eq!(snap["released"], json!(1), "permit not explicitly released");

    // Pool is full again (released == acquired), so this cannot block.
    let out = exec_tool(&reg, TASK_TOOL, task_input(SCENARIO_PLAIN, None)).unwrap();
    assert_eq!(out, PLAIN_TEXT);
}

#[test]
fn background_input_is_opt_in() {
    let advertised = |reg: &ToolRegistry| {
        reg.get(TASK_TOOL).unwrap().tool.schema()["properties"]["background"].is_object()
    };
    assert!(!advertised(&load_task_host().0));
    assert!(advertised(&load_background_host().0));
}

#[test_case::test_case(SCENARIO_PLAIN, None, PLAIN_TEXT, FINISHED ; "plain")]
#[test_case::test_case(SCENARIO_HAPPY, Some(answer_schema()), "{\"answer\":\"42\"}", FINISHED ; "structured")]
#[test_case::test_case(SCENARIO_PROMPT_ERROR, None, PROMPT_ERR_MSG, FAILED ; "model_error")]
#[test_case::test_case(SCENARIO_RAISE, None, RAISE_MSG, FAILED ; "exception")]
fn background_delivers_the_result_in_one_waking_notice(
    scenario: &str,
    schema: Option<Value>,
    expected: &str,
    status: &str,
) {
    let (reg, _host) = load_background_host();
    let session = SessionRef::generate();
    start_background(&reg, &session_ctx(&session), scenario, schema);
    wait(&reg, json!({ "notices": 1 }));
    let snap = probe(&reg);
    assert_eq!(snap["scope"], json!("session"));
    assert_eq!(snap["owner"], json!(session.as_str()));
    assert_eq!(snap["closed"], json!(1));
    assert_eq!(snap["released"], json!(1));
    let notices = snap["notices"].as_array().unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["session"], json!(session.as_str()));
    assert_eq!(notices[0]["wake"], json!(true));
    let text = notices[0]["text"].as_str().unwrap();
    for part in [scenario, status, expected] {
        assert!(text.contains(part), "{part} missing from {text}");
    }
}

#[test]
fn background_open_error_returns_inline() {
    let (reg, _host) = load_background_host();
    let ctx = session_ctx(&SessionRef::generate());
    let input = background_input(SCENARIO_SESSION_ERROR, None);
    assert_eq!(
        exec_tool_with_ctx(&reg, TASK_TOOL, input, &ctx).unwrap_err(),
        SESSION_OPEN_ERR
    );
    let snap = probe(&reg);
    assert_eq!(snap["acquired"], json!(0));
    assert_eq!(snap["notice_count"], json!(0));
}

#[test]
fn background_without_a_chat_session_runs_blocking() {
    let (reg, _host) = load_background_host();
    let out = exec_tool(&reg, TASK_TOOL, background_input(SCENARIO_PLAIN, None)).unwrap();
    assert_eq!(out, PLAIN_TEXT);
    let snap = probe(&reg);
    assert_eq!(snap["scope"], Value::Null);
    assert_eq!(snap["released"], json!(1));
    assert_eq!(snap["notice_count"], json!(0));
}

#[test]
fn background_and_blocking_tasks_share_one_concurrency_limit() {
    let mut opts = serde_json::Map::new();
    opts.insert("max_concurrent".into(), json!(1));
    opts.insert("allow_background".into(), json!(true));
    let (reg, _host) = load_task_host_with_opts(opts);
    let ctx = session_ctx(&SessionRef::generate());
    start_background(&reg, &ctx, SCENARIO_PARKED, None);
    wait(&reg, json!({ "parked": SCENARIO_PARKED }));
    let invocation = reg
        .get(TASK_TOOL)
        .unwrap()
        .tool
        .parse(&task_input(SCENARIO_PARKED_SECOND, None))
        .unwrap();
    smol::block_on(async {
        let mut blocking = invocation.execute(&ctx);
        assert!(poll_once(&mut blocking).await.is_none());
        wait(&reg, json!({ "acquires": 2 }));
        assert_eq!(probe(&reg)["acquired"], json!(1));
        release(&reg, SCENARIO_PARKED);
        wait(&reg, json!({ "parked": SCENARIO_PARKED_SECOND }));
        release(&reg, SCENARIO_PARKED_SECOND);
        assert!(blocking.await.output.is_ok());
    });
    wait(&reg, json!({ "notices": 1 }));
    assert_eq!(probe(&reg)["released"], json!(2));
}

#[test]
fn session_end_stops_only_that_chats_work() {
    let (reg, host) = load_background_host();
    let ending = SessionRef::generate();
    start_background(&reg, &session_ctx(&ending), SCENARIO_PARKED, None);
    let kept = SessionRef::generate();
    start_background(&reg, &session_ctx(&kept), SCENARIO_PARKED_SECOND, None);
    wait(&reg, json!({ "parked": SCENARIO_PARKED }));
    wait(&reg, json!({ "parked": SCENARIO_PARKED_SECOND }));
    host.event_handle()
        .end_sessions_blocking([ending.id()], SessionEndReason::Delete);
    let snap = probe(&reg);
    assert_eq!(snap["closed"], json!(1));
    assert_eq!(snap["released"], json!(1));
    release(&reg, SCENARIO_PARKED_SECOND);
    wait(&reg, json!({ "notices": 1 }));
    let snap = probe(&reg);
    assert_eq!(snap["notice_count"], json!(1));
    assert_eq!(snap["notices"][0]["session"], json!(kept.as_str()));
}

#[test_case::test_case(true ; "session_end")]
#[test_case::test_case(false ; "foreground_cancel")]
fn cancellation_during_background_admission_closes_the_session(session_end: bool) {
    let (reg, host) = load_background_host();
    let session = SessionRef::generate();
    let mut ctx = session_ctx(&session);
    let (cancel, token) = CancelToken::new();
    ctx.cancel = token;
    let entry = reg.get(TASK_TOOL).unwrap();
    let invocation = entry
        .tool
        .parse(&background_input(SCENARIO_OPEN_PARKED, None))
        .unwrap();
    smol::block_on(async {
        let mut admission = invocation.execute(&ctx);
        assert!(poll_once(&mut admission).await.is_none());
        wait(&reg, json!({ "parked": SCENARIO_OPEN_PARKED }));
        if session_end {
            host.event_handle()
                .end_sessions_blocking([session.id()], SessionEndReason::Delete);
        } else {
            cancel.cancel();
        }
        release(&reg, SCENARIO_OPEN_PARKED);
        assert_eq!(
            admission.await.output.unwrap_err(),
            BACKGROUND_CANCELLED_ERR
        );
    });
    let snap = probe(&reg);
    assert_eq!(snap["sessions"], json!(1));
    assert_eq!(snap["closed"], json!(1));
    assert_eq!(snap["prompt_count"], json!(0));
    assert_eq!(snap["acquired"], json!(0));
    assert_eq!(snap["released"], json!(0));
    assert_eq!(snap["notice_count"], json!(0));
}
