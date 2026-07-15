//! Tests the batch plugin's policy end-to-end: real plugin source, real
//! `maki.async.gather`, with tool dispatch replaced by a scriptable Lua stub.

use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::{stub_ctx, stub_ctx_with};
use maki_agent::{AgentEvent, AgentMode, BufferSnapshot, EventSender, SpanStyle, ToolOutput};
use maki_config::ToolOutputLines;
use maki_lua::PluginHost;
use serde_json::{Value, json};

const BATCH_PLUGIN_SRC: &str = include_str!("../../plugins/batch/init.lua");

// Mirrors of the plugin's format contracts.
const MAX_BATCH_SIZE: usize = 25;
const ERROR_PREFIX: &str = "[ERROR] ";
const EMPTY_ERROR: &str = "provide at least one tool call";
const NESTED_ERROR: &str = "cannot nest batch inside batch";
const DISCARDED_ERROR: &str = "maximum of 25 tools per batch";
const SUMMARY_ALL_OK_FMT: &str = "All {} tools executed successfully.";
const SUMMARY_MIXED_FMT: &str = "Executed {}/{} successfully. {} failed.";

const BATCH_TOOL: &str = "batch";
const PROBE_TOOL: &str = "probe";
const BOOM_ERR: &str = "stub tool exploded";

/// `maki.agent.call_tool` is stubbed; `maki.async.gather` and the semaphore
/// stay real, so the park/release pair proves children genuinely overlap.
const STUB_PRELUDE: &str = r#"
recorder = { calls = {} }
local sem = maki.async.semaphore(1)
local held = sem:acquire()

maki.agent.call_tool = function(ctx, name, input, opts)
  recorder.calls[#recorder.calls + 1] = { tool = name, params = input }
  if name == "ok" then
    return "ok:" .. tostring(input.tag or "?")
  elseif name == "annotated" then
    if opts and opts.on_annotation then
      opts.on_annotation("model-x")
      opts.on_annotation("5 lines")
    end
    return "annotated_done"
  elseif name == "park" then
    -- Deadlocks unless a sibling runs concurrently and releases.
    local p = sem:acquire()
    p:release()
    return "parked_done"
  elseif name == "release" then
    held:release()
    return "released_done"
  elseif name == "boom" then
    return nil, "@BOOM_ERR@"
  end
  return nil, "unknown tool: " .. name
end

maki.api.register_tool({
  name = "probe",
  description = "recorder snapshot",
  schema = { type = "object", properties = {}, additionalProperties = false },
  audiences = { "main" },
  handler = function(input, ctx)
    return (maki.json.encode(recorder))
  end,
})

maki.api.register_tool({
  name = "hdrtool",
  description = "child with custom header",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  header = function(input)
    return "H:" .. tostring(input.x)
  end,
  handler = function() return "unused" end,
})

maki.api.register_tool({
  name = "badhdr",
  description = "child whose header throws",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  header = function(input)
    error("header kaboom")
  end,
  handler = function() return "unused" end,
})

maki.api.register_tool({
  name = "badrestore",
  description = "child whose restore throws",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  restore = function() error("restore kaboom") end,
  handler = function() return "unused" end,
})

local ToolView = require("maki.tool_view")
maki.api.register_tool({
  name = "viewer",
  description = "child with a truncating ToolView restore",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  restore = function(input, output, is_error, rctx)
    return ToolView.restore(output, { max_lines = 2, keep = "head" })
  end,
  handler = function() return "unused" end,
})
"#;

fn load_batch_host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    let prelude = STUB_PRELUDE.replace("@BOOM_ERR@", BOOM_ERR);
    host.load_source("batch_policy", &format!("{prelude}\n{BATCH_PLUGIN_SRC}"))
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
            ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

fn run_batch(reg: &ToolRegistry, tool_calls: Value) -> Result<String, String> {
    exec_tool(reg, BATCH_TOOL, json!({ "tool_calls": tool_calls }))
}

fn run_batch_state(reg: &ToolRegistry, tool_calls: Value) -> Value {
    let entry = reg.get(BATCH_TOOL).expect("batch registered");
    let input = json!({ "tool_calls": tool_calls });
    let inv = entry.tool.parse(&input).expect("parse failed");
    let ctx = stub_ctx(&AgentMode::Build);
    smol::block_on(async { inv.execute(&ctx).await })
        .output
        .expect("batch failed")
        .state()
        .cloned()
        .expect("no state on batch output")
}

fn recorded_calls(reg: &ToolRegistry) -> Vec<Value> {
    let out = exec_tool(reg, PROBE_TOOL, json!({})).expect("probe failed");
    let snap: Value = serde_json::from_str(&out).expect("probe returned invalid json");
    snap["calls"].as_array().cloned().unwrap_or_default()
}

fn section(tool: &str, body: &str) -> String {
    format!("## {tool}\n{body}\n\n")
}

fn summary_all_ok(total: usize) -> String {
    SUMMARY_ALL_OK_FMT.replacen("{}", &total.to_string(), 1)
}

fn summary_mixed(ok: usize, total: usize, failed: usize) -> String {
    SUMMARY_MIXED_FMT
        .replacen("{}", &ok.to_string(), 1)
        .replacen("{}", &total.to_string(), 1)
        .replacen("{}", &failed.to_string(), 1)
}

#[test]
fn all_success_exact_llm_output() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "ok", "parameters": { "tag": "a" } },
            { "tool": "ok", "parameters": { "tag": "b" } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("ok", "ok:a"),
        section("ok", "ok:b"),
        summary_all_ok(2)
    );
    assert_eq!(out, expected);
}

#[test]
fn flat_nested_and_merged_params_normalize_identically() {
    let (reg, _host) = load_batch_host();
    run_batch(
        &reg,
        json!([
            { "tool": "ok", "tag": "flat", "n": 1 },
            { "tool": "ok", "parameters": { "tag": "nested", "n": 1 } },
            { "tool": "ok", "parameters": { "tag": "merged" }, "n": 1 },
        ]),
    )
    .expect("batch failed");

    let mut calls = recorded_calls(&reg);
    assert_eq!(calls.len(), 3);
    calls.sort_by_key(|c| c["params"]["tag"].as_str().unwrap_or("").to_owned());
    assert_eq!(calls[0]["params"], json!({ "tag": "flat", "n": 1 }));
    assert_eq!(calls[1]["params"], json!({ "tag": "merged", "n": 1 }));
    assert_eq!(calls[2]["params"], json!({ "tag": "nested", "n": 1 }));
}

#[test_case::test_case(json!([]), EMPTY_ERROR ; "empty_list")]
#[test_case::test_case(json!([{ "tool": "ok", "parameters": { "tag": "x" }, "tag": "y" }]), "duplicate parameter 'tag'" ; "duplicate_key")]
fn invalid_input_errors_without_dispatch(tool_calls: Value, expected_err: &str) {
    let (reg, _host) = load_batch_host();
    let err = run_batch(&reg, tool_calls).unwrap_err();
    assert!(err.contains(expected_err), "got: {err}");
    assert!(
        recorded_calls(&reg).is_empty(),
        "nothing must be dispatched"
    );
}

#[test]
fn nested_batch_rejected_without_dispatch() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "batch", "parameters": { "tool_calls": [] } },
            { "tool": "ok", "parameters": { "tag": "a" } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("batch", &format!("{ERROR_PREFIX}{NESTED_ERROR}")),
        section("ok", "ok:a"),
        summary_mixed(1, 2, 1)
    );
    assert_eq!(out, expected);
    let calls = recorded_calls(&reg);
    assert_eq!(calls.len(), 1, "nested batch must not be dispatched");
    assert_eq!(calls[0]["tool"], json!("ok"));
}

#[test]
fn overflow_entries_discarded_with_section() {
    let (reg, _host) = load_batch_host();
    let entries: Vec<Value> = (0..MAX_BATCH_SIZE + 1)
        .map(|i| json!({ "tool": "ok", "parameters": { "tag": i.to_string() } }))
        .collect();
    let out = run_batch(&reg, json!(entries)).expect("batch failed");
    assert!(
        out.contains(&format!("{ERROR_PREFIX}{DISCARDED_ERROR}")),
        "got: {out}"
    );
    assert!(
        out.ends_with(&summary_mixed(MAX_BATCH_SIZE, MAX_BATCH_SIZE + 1, 1)),
        "got: {out}"
    );
    assert_eq!(
        recorded_calls(&reg).len(),
        MAX_BATCH_SIZE,
        "only the first {MAX_BATCH_SIZE} entries may dispatch"
    );
}

/// One exact-string assertion pins the llm output contract: success
/// sections, error sections (child failure and unknown tool), input order,
/// and the mixed summary line.
#[test]
fn mixed_success_and_error_keeps_input_order() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "ok", "parameters": { "tag": "a" } },
            { "tool": "boom", "parameters": {} },
            { "tool": "nope", "parameters": {} },
            { "tool": "ok", "parameters": { "tag": "b" } },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}{}{}",
        section("ok", "ok:a"),
        section("boom", &format!("{ERROR_PREFIX}{BOOM_ERR}")),
        section("nope", &format!("{ERROR_PREFIX}unknown tool: nope")),
        section("ok", "ok:b"),
        summary_mixed(2, 4, 2)
    );
    assert_eq!(out, expected);
}

/// `park` blocks until `release` runs: completion order is release-first,
/// yet sections must come out in input order. Also proves real overlap
/// (serial execution would deadlock this test).
#[test]
fn children_overlap_and_output_keeps_input_order() {
    let (reg, _host) = load_batch_host();
    let out = run_batch(
        &reg,
        json!([
            { "tool": "park", "parameters": {} },
            { "tool": "release", "parameters": {} },
        ]),
    )
    .expect("batch failed");
    let expected = format!(
        "{}{}{}",
        section("park", "parked_done"),
        section("release", "released_done"),
        summary_all_ok(2)
    );
    assert_eq!(out, expected);
}

fn restore_snapshot_lines(
    host: &PluginHost,
    input: Value,
    output: &str,
    state: Option<Value>,
) -> Vec<Vec<(String, SpanStyle)>> {
    restore_snapshot_lines_opts(host, input, output, state, Vec::new())
}

fn restore_snapshot_lines_opts(
    host: &PluginHost,
    input: Value,
    output: &str,
    state: Option<Value>,
    clicks: Vec<usize>,
) -> Vec<Vec<(String, SpanStyle)>> {
    let handle = host.event_handle().expect("event handle available");
    let (tx, rx) = flume::unbounded();
    handle.request_restore(
        maki_lua::RestoreItem {
            tool: Arc::from(BATCH_TOOL),
            tool_use_id: "restore_id".to_owned(),
            output: output.to_owned(),
            input,
            is_error: false,
            tool_output_lines: ToolOutputLines::default(),
            theme_gen: None,
            clicks,
            state,
        },
        EventSender::new(tx, 0),
    );
    // Strong barrier: LoadSource drains the async gate, so spawned tasks
    // (highlight rewrites etc.) have finished before we read snapshots.
    host.load_source("barrier", "").unwrap();

    let mut lines = Vec::new();
    for env in rx.drain() {
        if let AgentEvent::ToolSnapshot { snapshot, .. } = env.event {
            lines = snapshot
                .lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| (s.text.clone(), s.style.clone()))
                        .collect()
                })
                .collect();
        }
    }
    lines
}

fn lines_text(lines: &[Vec<(String, SpanStyle)>]) -> String {
    lines
        .iter()
        .map(|l| l.iter().map(|(t, _)| t.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pins the child header contract: indicator span, `{tool}> ` prefix in
/// `tool_prefix` style, the child's own header spans, then the persisted
/// annotation like standalone (`push_header` in tool_display.rs).
#[test]
fn restore_with_state_renders_child_header_contract() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [{ "tool": "hdrtool", "parameters": { "x": "A" } }] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "hdrtool", "status": "success", "output": "line one\nline two", "annotation": "12 lines" }
        ] })),
    );
    let header = &lines[0];
    assert_eq!(
        header[0],
        ("● ".to_owned(), SpanStyle::Named("tool_success".to_owned()))
    );
    assert_eq!(
        header[1],
        (
            "hdrtool> ".to_owned(),
            SpanStyle::Named("tool_prefix".to_owned())
        )
    );
    assert_eq!(header[2].0, "H:A", "child's own header spans must follow");
    assert_eq!(
        header.last().unwrap(),
        &(
            " (12 lines)".to_owned(),
            SpanStyle::Named("tool_annotation".to_owned())
        ),
        "persisted annotation ends the header"
    );

    let text = lines_text(&lines);
    assert!(text.contains("line one"), "body from state: {text}");
    assert!(text.contains("line two"), "body from state: {text}");
}

/// Errors render like standalone: red `●` indicator and the plain body,
/// without the llm-only `[ERROR]` prefix or a red body.
#[test]
fn restore_error_child_renders_error_style() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [{ "tool": "ok", "parameters": {} }] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "ok", "status": "error", "output": "it broke" }
        ] })),
    );
    assert_eq!(
        lines[0][0],
        ("● ".to_owned(), SpanStyle::Named("tool_error".to_owned()))
    );
    let text = lines_text(&lines);
    assert!(text.contains("it broke"), "got: {text}");
    assert!(
        !text.contains(ERROR_PREFIX),
        "UI body must not carry the llm error prefix: {text}"
    );
    let body_spans: Vec<_> = lines[1..].iter().flatten().collect();
    assert!(
        body_spans
            .iter()
            .all(|(_, style)| *style != SpanStyle::Named("tool_error".to_owned())),
        "error body must render plain, not red: {body_spans:?}"
    );
}

/// A batch child's body is the child tool's own restore view, including
/// its ToolView truncation notice.
#[test]
fn restore_child_body_equals_child_restore_view() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [{ "tool": "viewer", "parameters": {} }] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "viewer", "status": "success", "output": "l1\nl2\nl3\nl4\nl5" }
        ] })),
    );
    let text = lines_text(&lines);
    assert!(text.contains("l1"), "visible head line: {text}");
    assert!(text.contains("l2"), "visible head line: {text}");
    assert!(
        !text.contains("l3"),
        "lines beyond the child's own cap stay hidden: {text}"
    );
    assert!(
        text.contains("... (3 lines) (click to expand)"),
        "the child's own ToolView notice must render: {text}"
    );
}

/// A child can annotate more than once (a task child streams its model,
/// then its completion note arrives on the same channel); batch joins
/// them on the child header in order.
#[test]
fn annotations_append_on_child_in_order() {
    let (reg, _host) = load_batch_host();
    let state = run_batch_state(&reg, json!([{ "tool": "annotated", "parameters": {} }]));
    assert_eq!(
        state["children"][0]["annotation"],
        json!("model-x · 5 lines")
    );
}

/// Throwing child header/restore callbacks degrade that child to plain
/// rendering without failing the batch.
#[test]
fn throwing_child_callbacks_degrade_to_plain() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [
            { "tool": "badhdr", "parameters": {} },
            { "tool": "badrestore", "parameters": {} },
        ] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "badhdr", "status": "success", "output": "hdr body" },
            { "tool": "badrestore", "status": "success", "output": "restore body" },
        ] })),
    );
    let text = lines_text(&lines);
    assert!(text.contains("badhdr> "), "plain-name header: {text}");
    assert!(text.contains("hdr body"), "body still renders: {text}");
    assert!(text.contains("restore body"), "plain body fallback: {text}");
}

/// No state: parse `## tool` sections from the LLM output.
#[test]
fn restore_without_state_parses_llm_sections() {
    let (_reg, host) = load_batch_host();
    let stored = format!(
        "{}{}{}",
        section("hdrtool", "line one\nline two"),
        section("ok", &format!("{ERROR_PREFIX}{BOOM_ERR}")),
        summary_mixed(1, 2, 1)
    );
    let lines = restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [
            { "tool": "hdrtool", "parameters": { "x": "A" } },
            { "tool": "ok", "parameters": {} },
        ] }),
        &stored,
        None,
    );
    assert_eq!(lines[0][2].0, "H:A", "child header fn still applies");
    let text = lines_text(&lines);
    assert!(
        lines.iter().any(|l| l
            .iter()
            .any(|(_, s)| *s == SpanStyle::Named("tool_error".into()))),
        "[ERROR] section maps to error status: {lines:?}"
    );
    assert!(text.contains("line one"), "body from section: {text}");
    assert!(text.contains(BOOM_ERR), "error body: {text}");
    assert!(
        !text.contains(ERROR_PREFIX),
        "llm error prefix must not render: {text}"
    );
    assert!(
        !text.contains("successfully"),
        "summary line must not render: {text}"
    );
}

/// A body line that looks like the next section header, but is not
/// preceded by the blank line `render_llm` always emits, must stay body
/// text instead of splitting the section early.
#[test]
fn restore_without_state_keeps_header_lookalike_in_body() {
    let (_reg, host) = load_batch_host();
    let stored = format!(
        "{}{}{}",
        section("ok", "body line\n## hdrtool\nbody tail"),
        section("hdrtool", "real body"),
        summary_all_ok(2)
    );
    let lines = restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [
            { "tool": "ok", "parameters": {} },
            { "tool": "hdrtool", "parameters": { "x": "A" } },
        ] }),
        &stored,
        None,
    );
    let text = lines_text(&lines);
    assert!(text.contains("## hdrtool"), "lookalike stays body: {text}");
    assert!(text.contains("body tail"), "section 1 intact: {text}");
    assert!(text.contains("real body"), "section 2 body: {text}");
    assert!(
        !text.contains("successfully"),
        "parsed path, not legacy fallback: {text}"
    );
}

/// Unparseable output: raw text as one plain body.
#[test]
fn restore_without_state_falls_back_to_raw_output() {
    let (_reg, host) = load_batch_host();
    let lines = restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [{ "tool": "ok", "parameters": { "tag": "a" } }] }),
        "not the section format",
        None,
    );
    let text = lines_text(&lines);
    assert!(text.contains("ok> "), "header from input: {text}");
    assert!(text.contains("not the section format"), "raw body: {text}");
}

/// A replayed click row must reach exactly the child it lands on and run
/// its real toggle. Two regressions pinned here: the async `buf:click`
/// held the child userdata borrow across the handler, silently dropping
/// the click, and the `{row=0}` replay toggled every child instead of the
/// clicked one.
#[test]
fn replayed_click_expands_only_the_clicked_child() {
    let (_reg, host) = load_batch_host();
    let input = json!({ "tool_calls": [
        { "tool": "viewer", "parameters": {} },
        { "tool": "viewer", "parameters": {} },
    ] });
    let state = json!({ "children": [
        { "tool": "viewer", "status": "success", "output": "a1\na2\na3\na4\na5" },
        { "tool": "viewer", "status": "success", "output": "b1\nb2\nb3\nb4\nb5" },
    ] });

    // Rows are 1-based (row 0 = header), so snapshot line i = row i+1.
    // Find child2's notice dynamically so layout changes can't break this.
    let collapsed = restore_snapshot_lines(&host, input.clone(), "irrelevant", Some(state.clone()));
    let notice_row = 1 + collapsed
        .iter()
        .enumerate()
        .filter(|(_, l)| l.iter().any(|(t, _)| t.contains("(click to expand)")))
        .nth(1)
        .map(|(i, _)| i)
        .expect("second child's truncation notice");

    let text = lines_text(&restore_snapshot_lines_opts(
        &host,
        input.clone(),
        "irrelevant",
        Some(state.clone()),
        vec![notice_row],
    ));
    assert!(text.contains("b5"), "clicked child expands: {text}");
    assert!(!text.contains("a3"), "other child stays truncated: {text}");
    assert!(
        text.contains("(click to expand)"),
        "other child keeps its notice: {text}"
    );

    // A second click inside the now-expanded child collapses it again.
    let text = lines_text(&restore_snapshot_lines_opts(
        &host,
        input,
        "irrelevant",
        Some(state),
        vec![notice_row, notice_row],
    ));
    assert!(!text.contains("b3"), "second click collapses: {text}");
}

/// Regression: an edit child's body must show the code change (old lines
/// in `diff_old`, new lines in `diff_new`), not the llm summary. Runs the
/// real edit and batch plugins. The extensionless path outside any real
/// filesystem pins the plain diff render: no async highlight rewrite (which
/// replaces these spans; covered at text level in real_plugins_restore.rs)
/// and no line numbers read back from disk.
#[test]
fn edit_child_body_renders_diff_not_summary() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
    let lines = restore_snapshot_lines(
        &host,
        json!({ "tool_calls": [{ "tool": "edit", "parameters": {
            "path": "/nonexistent/f",
            "old_string": "let a = 1;",
            "new_string": "let a = 2;",
        } }] }),
        "irrelevant",
        Some(json!({ "children": [
            { "tool": "edit", "status": "success", "output": "edited /nonexistent/f" }
        ] })),
    );
    let spans: Vec<(String, SpanStyle)> = lines.into_iter().flatten().collect();
    let old_span = (
        "- let a = 1;".to_owned(),
        SpanStyle::Named("diff_old".into()),
    );
    let new_span = (
        "+ let a = 2;".to_owned(),
        SpanStyle::Named("diff_new".into()),
    );
    assert!(spans.contains(&old_span), "old line in diff_old: {spans:?}");
    assert!(spans.contains(&new_span), "new line in diff_new: {spans:?}");
    assert!(
        !spans
            .iter()
            .any(|(t, _)| t.contains("edited /nonexistent/f")),
        "summary must not be the body: {spans:?}"
    );
}

// --- Live execution snapshots (real dispatch, no call_tool stub) ---

/// Regression: child restores used to snapshot the first-created buf
/// instead of the batch root buf, letting a tiny header overwrite the
/// full batch render.
#[test]
fn async_highlight_tasks_never_shrink_and_reach_final_snapshot() {
    let snapshots = run_live_batch(HL_CHILD_SRC, "hl");
    let header_counts: Vec<usize> = snapshots
        .iter()
        .map(|s| s.text().matches("hl> ").count())
        .collect();
    assert!(
        header_counts.windows(2).all(|w| w[0] <= w[1]),
        "a later snapshot must never lose children: {header_counts:?}"
    );
    assert_eq!(
        header_counts.last(),
        Some(&2),
        "final snapshot must carry all children"
    );
    let last = snapshots.last().expect("at least one batch snapshot");
    let has_inline = last
        .lines
        .iter()
        .flat_map(|l| &l.spans)
        .any(|s| matches!(s.style, SpanStyle::Inline(_)));
    assert!(
        has_inline,
        "final snapshot must contain highlighted spans, got:\n{}",
        last.text()
    );
}

/// Restores that await async APIs (like bash highlighting its header)
/// must not throw out of the `get_tool` wrapper.
#[test]
fn child_restore_awaiting_async_api_keeps_its_body() {
    let snapshots = run_live_batch(SYNC_HL_CHILD_SRC, "cmd");
    let last = snapshots.last().expect("at least one batch snapshot");
    let text = last.text();
    assert!(
        text.contains("echo header-marker"),
        "child restore header must survive, got:\n{text}"
    );
}

const BATCH_ID: &str = "batch_id";

const HL_CHILD_SRC: &str = r#"
local ToolView = require("maki.tool_view")
maki.api.register_tool({
  name = "hl",
  description = "styled header + async-highlight restore",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  header = function(input)
    local b = maki.ui.buf()
    b:set_lines({ { { "hl-header", "tool" } } })
    return b
  end,
  restore = function(input, output, is_error, rctx)
    local buf = maki.ui.buf()
    local view = ToolView.new(buf, { max_lines = 10, keep = "head" })
    view:set_highlight(output, "lua")
    view:finish()
    return buf
  end,
  handler = function() return "local x = 1" end,
})
"#;

const SYNC_HL_CHILD_SRC: &str = r#"
local ToolView = require("maki.tool_view")
maki.api.register_tool({
  name = "cmd",
  description = "restore awaits maki.ui.highlight inline",
  schema = { type = "object", properties = {} },
  audiences = { "main" },
  restore = function(input, output, is_error, rctx)
    local buf = maki.ui.buf()
    local view = ToolView.new(buf, { max_lines = 10, keep = "tail" })
    local header = maki.ui.highlight("echo header-marker", "bash") or { { { "echo header-marker" } } }
    view:set_header(header)
    view:append(output)
    view:finish()
    return buf
  end,
  handler = function() return "cmd-output" end,
})
"#;

fn run_live_batch(child_src: &str, tool: &str) -> Vec<BufferSnapshot> {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("live_batch", &format!("{child_src}\n{BATCH_PLUGIN_SRC}"))
        .unwrap();

    let (tx, rx) = flume::unbounded();
    let event_tx = EventSender::new(tx, 0);
    let mut ctx = stub_ctx_with(&AgentMode::Build, Some(&event_tx), Some(BATCH_ID));
    ctx.registry = Arc::clone(&reg);

    let input = json!({ "tool_calls": [
        { "tool": tool, "parameters": {} },
        { "tool": tool, "parameters": {} },
    ]});
    let entry = reg.get(BATCH_TOOL).unwrap();
    let inv = entry.tool.parse(&input).unwrap();
    let done = smol::block_on(async { inv.execute(&ctx).await });
    assert!(done.output.is_ok(), "batch failed: {:?}", done.output);

    host.load_source("barrier", "").unwrap();

    let mut snapshots = Vec::new();
    for env in rx.drain() {
        if let AgentEvent::ToolSnapshot { id, snapshot, .. } = env.event {
            assert_eq!(id, BATCH_ID);
            snapshots.push(snapshot);
        }
    }
    snapshots
}
