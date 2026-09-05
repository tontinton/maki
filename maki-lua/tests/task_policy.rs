//! Tests the task plugin's structured-output policy end-to-end: real plugin
//! source, real `maki.json` / `maki.async`, with model I/O replaced by
//! scriptable Lua stubs.

use std::sync::Arc;
use std::time::Duration;

use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::{AgentMode, ToolOutput};
use maki_lua::PluginHost;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};

const TASK_PLUGIN_SRC: &str = include_str!("../../plugins/task/init.lua");

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
const TASK_TOOL_RESULT: &str = "task_result";
const TASK_TOOL_WAIT: &str = "task_wait";
const PROBE_TOOL: &str = "probe";
const BG_FLASH: &str = "bg-done";
const BG_UNKNOWN_PREFIX: &str = "unknown task id: ";
const BG_WAIT_MIN_ERR: &str = "timeout_ms must be >= 1";
/// Generous vs the background work, which finishes in well under a second.
const BG_TEST_WAIT: Duration = Duration::from_secs(5);
const TASK_PROMPT: &str = "do the thing";
const PLAIN_TEXT: &str = "plain text result";
const RECOVERED_TEXT: &str = "summary after nudge";
/// Mirrors the task plugin's `NUDGE_SUMMARY` wording.
const SUMMARY_NUDGE_FRAGMENT: &str = "concise summary";
const PROMPT_ERR_MSG: &str = "model exploded";
const RAISE_MSG: &str = "stub prompt kaboom";
const PARTIAL_TEXT: &str = "half a transcript";
const CANCELLED_ERR: &str = "cancelled";
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

/// Stubs keyed by `opts.name` (the task's `description`). `maki.json` and
/// `maki.async` stay real so schema validation and semaphore behavior are tested.
const STUB_PRELUDE: &str = r#"
recorder = { prompts = {}, closed = 0, sessions = 0, acquired = 0, released = 0, notifies = {} }

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

behaviors.bg_slow = function(sess, msg)
  maki.async.sleep(400)
  return { text = "@PLAIN_TEXT@" }
end

-- The real notify touches a live mailbox, which the stub ctx lacks. Record
-- instead, and flash so tests get a deterministic completion signal on the
-- UI action channel.
maki.session.notify = function(text, opts)
  recorder.notifies[#recorder.notifies + 1] = { text = text, opts = opts }
  maki.ui.flash("@BG_FLASH@")
end

maki.agent.session = function(ctx, opts)
  recorder.sessions = recorder.sessions + 1
  recorder.has_local_tools = opts.local_tools ~= nil
  local sess = { opts = opts }
  function sess:prompt(msg)
    recorder.prompts[#recorder.prompts + 1] = msg
    return behaviors[opts.name](self, msg)
  end
  function sess:close(is_err)
    recorder.closed = recorder.closed + 1
    recorder.close_failed = is_err
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
      acquired = recorder.acquired,
      released = recorder.released,
      sem_size = recorder.sem_size,
    }
    if recorder.resolve_opts then
      snap.resolve_opts = recorder.resolve_opts
    end
    if #recorder.prompts > 0 then
      snap.prompts = recorder.prompts
    end
    if recorder.close_failed ~= nil then
      snap.close_failed = recorder.close_failed
    end
    if #recorder.notifies > 0 then
      snap.notifies = recorder.notifies
    end
    return (maki.json.encode(snap))
  end,
})
"#;

fn load_task_host() -> (Arc<ToolRegistry>, PluginHost) {
    load_task_host_with_opts(serde_json::Map::new())
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
        .replace("@BG_FLASH@", BG_FLASH);
    host.load_source_with_opts(
        "task_policy",
        &format!("{prelude}\n{TASK_PLUGIN_SRC}"),
        opts,
    )
    .unwrap();
    (reg, host)
}

fn exec_tool(reg: &ToolRegistry, name: &str, input: Value) -> Result<String, String> {
    exec_tool_with_session(reg, name, input, None)
}

fn exec_tool_with_session(
    reg: &ToolRegistry,
    name: &str,
    input: Value,
    session: Option<SessionRef>,
) -> Result<String, String> {
    let entry = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} not registered"));
    let inv = entry.tool.parse(&input).expect("parse failed");
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.session_id = session;
    smol::block_on(async { inv.execute(&ctx).await })
        .output
        .map(|out| match out {
            ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
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

fn bg_input(scenario: &str) -> Value {
    let mut input = task_input(scenario, None);
    input["background"] = json!(true);
    input
}

fn bg_task_id(receipt: &str) -> String {
    receipt
        .split("Background task ")
        .nth(1)
        .and_then(|rest| rest.split(" started").next())
        .expect("receipt must contain a task id")
        .to_string()
}

fn wait_flash(host: &PluginHost) -> maki_lua::UiAction {
    host.ui_action_rx()
        .recv_timeout(BG_TEST_WAIT)
        .expect("on_finish must flash the UI")
}

const FULL_MODEL_SPEC: &str = "aperture/ollama/glm-5.2";

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
#[test_case::test_case(json!({"output_schema": "not an object"}), SCHEMA_ROOT_ERROR ; "non_table_output_schema")]
#[test_case::test_case(json!({"output_schema": {"description": "missing type"}}), SCHEMA_ROOT_ERROR ; "output_schema_missing_type")]
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

// --- background tasks ---

/// The receipt must come back before the subagent finishes, and the outcome
/// must reach the session mailbox with wake set, carrying the full result.
#[test]
fn background_returns_receipt_and_delivers_by_notify() {
    let (reg, host) = load_task_host();
    let receipt =
        exec_tool(&reg, TASK_TOOL, bg_input(SCENARIO_PLAIN)).expect("background task failed");
    let id = bg_task_id(&receipt);
    assert!(
        receipt.contains(&format!("Background task {id} started ({SCENARIO_PLAIN})")),
        "got: {receipt}"
    );

    assert!(
        matches!(wait_flash(&host), maki_lua::UiAction::Flash(msg) if msg == BG_FLASH),
        "flash must carry the completion marker"
    );

    let snap = probe(&reg);
    assert_eq!(snap["closed"], json!(1));
    assert_eq!(
        snap["released"],
        json!(1),
        "permit not released in on_finish"
    );
    assert!(
        snap.get("close_failed").is_none_or(Value::is_null),
        "success must close without a verdict, got {:?}",
        snap.get("close_failed")
    );
    let notifies = snap["notifies"].as_array().expect("notify must fire");
    assert_eq!(notifies.len(), 1);
    let text = notifies[0]["text"].as_str().expect("notify text");
    assert!(
        text.contains(&format!("Background task {id} ({SCENARIO_PLAIN}) finished"))
            && text.contains(PLAIN_TEXT),
        "got: {text}"
    );
    assert_eq!(notifies[0]["opts"]["wake"], json!(true));

    let out =
        exec_tool(&reg, TASK_TOOL_RESULT, json!({ "task_id": id })).expect("task_result failed");
    assert_eq!(out, PLAIN_TEXT);
}

/// A background subagent must not be a child of the call that spawned it, or
/// it dies the moment that call's turn ends instead of outliving it. With a
/// real session id this now spawns `scope = { session = sid }`; confirm that
/// still reaches a receipt instead of tripping the scope validation.
#[test]
fn background_with_a_real_session_detaches_and_still_delivers() {
    let (reg, host) = load_task_host();
    let session: SessionRef = "01965087-4c71-7f00-8000-000000000000"
        .parse()
        .expect("valid session id");
    let receipt = exec_tool_with_session(
        &reg,
        TASK_TOOL,
        bg_input(SCENARIO_PLAIN),
        Some(session.clone()),
    )
    .expect("background task failed");
    let id = bg_task_id(&receipt);

    assert!(
        matches!(wait_flash(&host), maki_lua::UiAction::Flash(msg) if msg == BG_FLASH),
        "flash must carry the completion marker"
    );

    let out = exec_tool_with_session(
        &reg,
        TASK_TOOL_RESULT,
        json!({ "task_id": id }),
        Some(session),
    )
    .expect("task_result failed");
    assert_eq!(out, PLAIN_TEXT);
}

#[test]
fn background_error_lands_in_result_and_releases_the_permit() {
    let (reg, host) = load_task_host();
    let receipt =
        exec_tool(&reg, TASK_TOOL, bg_input(SCENARIO_PROMPT_ERROR)).expect("receipt failed");
    let id = bg_task_id(&receipt);
    wait_flash(&host);

    let snap = probe(&reg);
    assert_eq!(snap["released"], json!(1), "error path must still release");
    assert_eq!(
        snap["close_failed"],
        json!(format!("{SUB_AGENT_ERROR_PREFIX}{PROMPT_ERR_MSG}")),
        "failure must hand close the verdict"
    );
    let err = exec_tool(&reg, TASK_TOOL_RESULT, json!({ "task_id": id })).unwrap_err();
    assert_eq!(err, format!("{SUB_AGENT_ERROR_PREFIX}{PROMPT_ERR_MSG}"));
}

#[test]
fn task_result_unknown_id_errors() {
    let (reg, _host) = load_task_host();
    let err = exec_tool(&reg, TASK_TOOL_RESULT, json!({ "task_id": "nope" })).unwrap_err();
    assert!(err.starts_with(BG_UNKNOWN_PREFIX), "got: {err}");
}

#[test]
fn task_wait_returns_the_result_once_the_task_finishes() {
    let (reg, _host) = load_task_host();
    let receipt = exec_tool(&reg, TASK_TOOL, bg_input(SCENARIO_PLAIN)).expect("receipt failed");
    let id = bg_task_id(&receipt);
    let out = exec_tool(
        &reg,
        TASK_TOOL_WAIT,
        json!({ "task_id": id, "timeout_ms": BG_TEST_WAIT.as_millis() as u64 }),
    )
    .expect("task_wait failed");
    assert_eq!(out, PLAIN_TEXT);
}

/// The wait must give up at the timeout with a still-working note, and the
/// task must still complete afterwards.
#[test]
fn task_wait_times_out_while_working_then_the_result_lands() {
    let (reg, host) = load_task_host();
    let receipt = exec_tool(&reg, TASK_TOOL, bg_input("bg_slow")).expect("receipt failed");
    let id = bg_task_id(&receipt);

    let out = exec_tool(
        &reg,
        TASK_TOOL_WAIT,
        json!({ "task_id": id, "timeout_ms": 50 }),
    )
    .expect("task_wait failed");
    assert!(out.contains("still working after 50ms"), "got: {out}");

    wait_flash(&host);
    let result =
        exec_tool(&reg, TASK_TOOL_RESULT, json!({ "task_id": id })).expect("task_result failed");
    assert_eq!(result, PLAIN_TEXT);
}

#[test]
fn task_wait_rejects_non_positive_timeout() {
    let (reg, _host) = load_task_host();
    let receipt = exec_tool(&reg, TASK_TOOL, bg_input(SCENARIO_PLAIN)).expect("receipt failed");
    let id = bg_task_id(&receipt);
    let err = exec_tool(
        &reg,
        TASK_TOOL_WAIT,
        json!({ "task_id": id, "timeout_ms": 0 }),
    )
    .unwrap_err();
    assert_eq!(err, BG_WAIT_MIN_ERR);
}
