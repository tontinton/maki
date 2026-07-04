//! Tests the task plugin's structured-output policy end-to-end: real plugin
//! source, real `maki.json` / `maki.async`, with model I/O replaced by
//! scriptable Lua stubs.

use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::{AgentMode, ToolOutput};
use maki_lua::PluginHost;
use serde_json::{Value, json};

const TASK_PLUGIN_SRC: &str = include_str!("../../plugins/task/init.lua");

// Mirrors of the plugin's error contracts and policy numbers.
const STRUCTURED_OUTPUT_TOOL: &str = "structured_output";
const MAX_STRUCTURED_RETRIES: usize = 2;
const MAX_SCHEMA_ERRORS: usize = 3;
const SCHEMA_COMPILE_ERROR: &str = "invalid output_schema";
const STRUCTURED_MISSING_ERROR: &str = "subagent finished without calling structured_output";
const STRUCTURED_INVALID_ERROR: &str = "subagent result does not match output_schema";
const UNKNOWN_SUBAGENT_ERR: &str = "unknown subagent type: bogus";
const SUB_AGENT_ERROR_PREFIX: &str = "sub-agent error: ";

const TASK_TOOL: &str = "task";
const PROBE_TOOL: &str = "probe";
const TASK_PROMPT: &str = "do the thing";
const PLAIN_TEXT: &str = "plain text result";
const PROMPT_ERR_MSG: &str = "model exploded";
const RAISE_MSG: &str = "stub prompt kaboom";

const SCENARIO_PLAIN: &str = "plain";
const SCENARIO_HAPPY: &str = "happy";
const SCENARIO_INVALID_THEN_VALID: &str = "invalid_then_valid";
const SCENARIO_NEVER_STRUCTURED: &str = "never_structured";
const SCENARIO_INVALID_ONLY: &str = "invalid_only";
const SCENARIO_PROMPT_ERROR: &str = "prompt_error";
const SCENARIO_RAISE: &str = "raise";

/// Stubs keyed by `opts.name` (the task's `description`). `maki.json` and
/// `maki.async` stay real so schema validation and semaphore behavior are tested.
const STUB_PRELUDE: &str = r#"
recorder = { prompts = {}, closed = 0, sessions = 0, acquired = 0, released = 0 }

-- Spy wrapper: the real semaphore does the work, counters track that every
-- permit is explicitly released (gc would silently hide a leak).
local real_semaphore = maki.async.semaphore
maki.async.semaphore = function(n)
  recorder.sem_size = n
  local sem = real_semaphore(n)
  return {
    acquire = function(self)
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

behaviors.raise = function(sess, msg)
  error("@RAISE_MSG@")
end

maki.agent.session = function(ctx, opts)
  recorder.sessions = recorder.sessions + 1
  recorder.has_local_tools = opts.local_tools ~= nil
  local sess = { opts = opts }
  function sess:prompt(msg)
    recorder.prompts[#recorder.prompts + 1] = msg
    return behaviors[opts.name](self, msg)
  end
  function sess:close()
    recorder.closed = recorder.closed + 1
  end
  return sess
end

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
      first_ack = recorder.first_ack,
      first_err = recorder.first_err,
      second_ack = recorder.second_ack,
      second_err = recorder.second_err,
      task_cap = ctx:config().task_max_concurrent,
      acquired = recorder.acquired,
      released = recorder.released,
      sem_size = recorder.sem_size,
    }
    if #recorder.prompts > 0 then
      snap.prompts = recorder.prompts
    end
    return (maki.json.encode(snap))
  end,
})
"#;

fn load_task_host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let prelude = STUB_PRELUDE
        .replace("@PLAIN_TEXT@", PLAIN_TEXT)
        .replace("@PROMPT_ERR@", PROMPT_ERR_MSG)
        .replace("@RAISE_MSG@", RAISE_MSG);
    host.load_source("task_policy", &format!("{prelude}\n{TASK_PLUGIN_SRC}"))
        .unwrap();
    (reg, host)
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: Value) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let ctx = stub_ctx(&AgentMode::Build);
    smol::block_on(async { inv.execute(&ctx).await })
        .output
        .map(|out| match out {
            ToolOutput::Plain(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
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
#[test_case::test_case(json!({"output_schema": {"type": 42}}), SCHEMA_COMPILE_ERROR ; "invalid_output_schema")]
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

#[test]
fn structured_happy_path_returns_validated_json() {
    let (reg, _host) = load_task_host();
    let out = exec_tool(
        &reg,
        TASK_TOOL,
        task_input(SCENARIO_HAPPY, Some(answer_schema())),
    )
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

/// Spy counters catch a leaked permit even when gc would silently reclaim it.
#[test]
fn raising_prompt_does_not_leak_semaphore_permit() {
    let (reg, _host) = load_task_host();
    let err = exec_tool(&reg, TASK_TOOL, task_input(SCENARIO_RAISE, None)).unwrap_err();
    assert!(err.contains(RAISE_MSG), "got: {err}");

    let snap = probe(&reg);
    let cap = snap["task_cap"].as_u64().expect("task_cap missing");
    assert_eq!(
        snap["sem_size"],
        json!(cap),
        "semaphore not sized from config"
    );
    assert_eq!(snap["acquired"], json!(1));
    assert_eq!(snap["released"], json!(1), "permit not explicitly released");

    // Pool is full again (released == acquired), so this cannot block.
    let out = exec_tool(&reg, TASK_TOOL, task_input(SCENARIO_PLAIN, None)).unwrap();
    assert_eq!(out, PLAIN_TEXT);
}
