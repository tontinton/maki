//! Exercises real plugins (bash, grep, batch) through `request_restore`.
//! A broken restore silently falls back to raw LLM output, so we assert
//! things only the real views produce (gutters, command headers, truncation).

use std::sync::Arc;

use maki_agent::AgentEvent;
use maki_agent::tools::ToolRegistry;
use maki_config::ToolOutputLines;
use maki_lua::PluginHost;
use serde_json::{Value, json};

const BASH_SRC: &str = include_str!("../../plugins/bash/init.lua");
const GREP_SRC: &str = include_str!("../../plugins/grep/init.lua");
const BATCH_SRC: &str = include_str!("../../plugins/batch/init.lua");

/// Only the real ToolView emits this when collapsed.
const EXPAND_HINT: &str = "click to expand";
/// Fixed caps so truncation tests don't depend on the product defaults. The
/// index and read caps differ so a body rendered through the wrong view is
/// visibly different.
const VIEW_CAP: usize = 3;
const INDEX_VIEW_CAP: usize = 2;
const READ_VIEW_CAP: usize = 5;

fn view_lines() -> ToolOutputLines {
    ToolOutputLines {
        other: VIEW_CAP,
        index: INDEX_VIEW_CAP,
        read: READ_VIEW_CAP,
        ..ToolOutputLines::DEFAULT
    }
}

const GREP_OUT: &str =
    "src/a.rs:\n  1: fn main() {}\n  2: fn helper() {}\n\nsrc/b.rs:\n  10: fn other() {}";

const BATCH_INPUT_GREP_BASH: &str = r#"{ "tool_calls": [
    { "tool": "grep", "parameters": { "pattern": "fn" } },
    { "tool": "bash", "parameters": { "command": "echo hello-from-bash" } }
]}"#;

fn load_host() -> PluginHost {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("bash", BASH_SRC).unwrap();
    host.load_source("grep", GREP_SRC).unwrap();
    host.load_source("batch", BATCH_SRC).unwrap();
    host
}

fn batch_state() -> Value {
    json!({ "children": [
        { "tool": "grep", "status": "success", "output": GREP_OUT },
        { "tool": "bash", "status": "success", "output": "hello-from-bash" },
    ]})
}

struct Restored {
    body: String,
    header: String,
}

fn restore(
    host: &PluginHost,
    tool: &str,
    input: Value,
    output: &str,
    state: Option<Value>,
    clicks: Vec<usize>,
) -> Restored {
    let handle = host.event_handle();
    let (tx, rx) = flume::unbounded();
    handle.request_restore(
        maki_lua::RestoreItem {
            tool: Arc::from(tool),
            tool_use_id: "restore_id".to_owned(),
            output: output.to_owned(),
            input,
            is_error: false,
            tool_output_lines: view_lines(),
            theme_gen: None,
            clicks,
            state,
        },
        maki_agent::EventSender::new(tx, 0),
    );
    handle.wait_restore_complete_for_test();
    // The empty LoadSource drains the async gate, so spawned highlight tasks
    // finish before we inspect the buffers.
    host.load_source("barrier", "").unwrap();
    let mut out = Restored {
        body: String::new(),
        header: String::new(),
    };
    for env in rx.drain() {
        match env.event {
            AgentEvent::ToolSnapshot { snapshot, .. } => out.body = snapshot.text(),
            AgentEvent::ToolHeaderSnapshot { snapshot, .. } => out.header = snapshot.text(),
            _ => {}
        }
    }
    out
}

#[test]
fn bash_restore_renders_real_view() {
    let host = load_host();
    let r = restore(
        &host,
        "bash",
        json!({ "command": "echo hi", "description": "print hi" }),
        "hi",
        None,
        Vec::new(),
    );
    assert!(
        r.body.contains("echo hi"),
        "real view renders the command header; the fallback body is raw output only: {}",
        r.body
    );
    assert!(r.header.contains("print hi"), "header: {}", r.header);
}

/// Phase 1: children render through their own real views (grep gutter,
/// bash command header), not the raw-llm fallback. Phase 2: a replayed
/// click inside grep's range reaches its real toggle and expands only it.
#[test]
fn batch_restore_renders_real_children_and_click_expands_grep() {
    let host = load_host();
    let input: Value = serde_json::from_str(BATCH_INPUT_GREP_BASH).unwrap();
    let collapsed = restore(
        &host,
        "batch",
        input.clone(),
        "whatever",
        Some(batch_state()),
        Vec::new(),
    );
    let text = &collapsed.body;
    assert!(text.contains("grep> "), "grep child header: {text}");
    assert!(text.contains("bash> "), "bash child header: {text}");
    // grep's real view reformats `nr:` into gutter lines.
    assert!(text.contains(" 1 fn main() {}"), "grep gutter: {text}");
    assert!(
        !text.contains("1: fn main"),
        "raw llm text means the child restore degraded to fallback: {text}"
    );
    assert!(
        text.contains(EXPAND_HINT),
        "grep view collapsed past its cap: {text}"
    );
    assert!(
        text.contains("echo hello-from-bash"),
        "bash child rendered its real view (command header): {text}"
    );
    assert!(
        text.lines().any(|l| l.trim() == "hello-from-bash"),
        "bash output line: {text}"
    );

    // Rows are 1-based (row 0 = header), so snapshot line i = row i+1.
    let notice_row = 1 + collapsed
        .body
        .lines()
        .position(|l| l.contains(EXPAND_HINT))
        .expect("grep truncation notice in collapsed render");
    let clicked = restore(
        &host,
        "batch",
        input,
        "whatever",
        Some(batch_state()),
        vec![notice_row],
    );
    let text = &clicked.body;
    assert!(
        text.contains("10 fn other() {}"),
        "expanded grep tail visible: {text}"
    );
    assert!(
        !text.contains(EXPAND_HINT),
        "grep no longer collapsed: {text}"
    );
    assert!(
        text.contains("hello-from-bash"),
        "bash child untouched: {text}"
    );
}

/// Header fn that yields (e.g. highlight) must work, not fall back.
#[test]
fn restore_header_fn_may_await_async_apis() {
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source(
        "hdr",
        r#"maki.api.register_tool({
            name = "hdr_await",
            description = "t",
            schema = { type = "object", properties = {} },
            handler = function() return "ok" end,
            header = function(input)
                local hl = maki.ui.highlight("echo marker", "bash") or { { { "echo marker" } } }
                local buf = maki.ui.buf()
                buf:set_lines(hl)
                return buf
            end,
            restore = function(input, output)
                local buf = maki.ui.buf()
                buf:line("body")
                return buf
            end,
        })"#,
    )
    .unwrap();
    let r = restore(&host, "hdr_await", json!({}), "ok", None, Vec::new());
    assert_eq!(r.body.trim(), "body");
    assert!(
        r.header.contains("echo marker"),
        "awaiting header fn must survive: {}",
        r.header
    );
}

/// Standalone edit diffs never truncate (Rust hardcodes it), so batch
/// children must match: whole diff, `-` lines numbered by finding the new
/// text in the edited file, `+` lines with a blank gutter.
#[test]
fn multiedit_batch_child_shows_full_numbered_diff() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "top\nzzz\nn1\nn2\nn3\nn4\nn5\nbottom\n").unwrap();

    let host = PluginHost::with_all_builtins(Arc::new(ToolRegistry::new())).unwrap();
    let input = json!({ "tool_calls": [{ "tool": "multiedit", "parameters": {
        "path": path.to_str().unwrap(),
        "edits": [{ "old_string": "old1\nold2\nold3\nold4\nold5", "new_string": "n1\nn2\nn3\nn4\nn5" }],
    }}]});
    let state = json!({ "children": [
        { "tool": "multiedit", "status": "success", "output": "applied 1 edit" },
    ]});
    let r = restore(&host, "batch", input, "whatever", Some(state), Vec::new());

    let text = &r.body;
    // keep = "head" truncation would cut the tail, so the last added line
    // present plus no collapse notice proves the 10-line diff is whole.
    assert!(
        text.contains("+ n5") && !text.contains(EXPAND_HINT),
        "edit diffs must never truncate: {text}"
    );
    assert!(
        text.contains("3 - old1") && text.contains("7 - old5"),
        "removed lines numbered from the new text's file position: {text}"
    );
    assert!(
        !text.contains("3 + n1"),
        "added lines get a blank gutter: {text}"
    );
}

const INDEX_TOOL: &str = "index";
const LIVE_TOOL_USE_ID: &str = "live_id";
/// More than the index view cap, exactly the read view cap, so a listing
/// rendered through the index view is visibly truncated.
const DIR_ENTRIES: [&str; READ_VIEW_CAP] = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
const ENTRIES_SUFFIX: &str = " entries";

struct Live {
    body: String,
    output: String,
    annotation: Option<String>,
}

fn exec_live(host: &PluginHost, reg: &ToolRegistry, tool: &str, input: Value) -> Live {
    let (tx, rx) = flume::unbounded();
    let event_tx = maki_agent::EventSender::new(tx, 0);
    let mut ctx = maki_agent::tools::test_support::stub_ctx_with(
        &maki_agent::AgentMode::Build,
        Some(&event_tx),
        Some(LIVE_TOOL_USE_ID),
    );
    ctx.tool_output_lines = view_lines();
    let inv = reg
        .get(tool)
        .unwrap_or_else(|| panic!("tool {tool} not registered"))
        .tool
        .parse(&input)
        .expect("parse failed");
    let result = smol::block_on(async { inv.execute(&ctx).await });
    host.load_source("live_barrier", "").unwrap();
    let mut body = String::new();
    for env in rx.drain() {
        if let AgentEvent::ToolSnapshot { snapshot, .. } = env.event {
            body = snapshot.text();
        }
    }
    let output = match result.output.expect("tool failed") {
        maki_agent::ToolOutput::Plain(s) | maki_agent::ToolOutput::Markdown(s) => s.text,
        other => panic!("unexpected output: {other:?}"),
    };
    Live {
        body,
        output,
        annotation: result.annotation,
    }
}

/// A directory has no skeleton, so index shows the plain listing. Restore must
/// rebuild that same listing view instead of the index skeleton view, which
/// would truncate to the index cap and highlight the entries as code.
#[test]
fn index_dir_renders_identically_live_and_restored() {
    let dir = tempfile::tempdir().unwrap();
    for name in DIR_ENTRIES {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
    let input = json!({ "path": dir.path().to_str().unwrap() });

    let live = exec_live(&host, &reg, INDEX_TOOL, input.clone());
    let restored = restore(&host, INDEX_TOOL, input, &live.output, None, Vec::new());

    let expected_annotation = format!("{}{ENTRIES_SUFFIX}", DIR_ENTRIES.len());
    assert_eq!(
        live.annotation.as_deref(),
        Some(expected_annotation.as_str()),
        "live dir listing is annotated like read's"
    );
    for name in DIR_ENTRIES {
        assert!(
            live.body.contains(name),
            "entry {name} missing: {}",
            live.body
        );
    }
    assert!(
        !live.body.contains(EXPAND_HINT),
        "listing fits the read view cap: {}",
        live.body
    );
    assert_eq!(
        restored.body, live.body,
        "restored dir listing must match the live one"
    );
}
