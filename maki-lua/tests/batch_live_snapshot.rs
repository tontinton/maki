//! Regression: async tasks spawned by child restores (e.g. highlighting)
//! used to snapshot the first-created buf instead of the batch root buf.
//! That meant one child's tiny header could overwrite the full batch render.

use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx_with;
use maki_agent::{AgentEvent, AgentMode, BufferSnapshot, EventSender, SpanStyle};
use maki_lua::PluginHost;
use serde_json::json;

const BATCH_PLUGIN_SRC: &str = include_str!("../../plugins/batch/init.lua");
const BATCH_ID: &str = "batch_id";

const CHILD_SRC: &str = r#"
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

#[test]
fn async_highlight_tasks_never_shrink_and_reach_final_snapshot() {
    let snapshots = run_batch(CHILD_SRC, "hl");
    for text in snapshots.iter().map(BufferSnapshot::text) {
        assert_eq!(
            text.matches("hl> ").count(),
            2,
            "every batch snapshot must carry all children, got:\n{text}"
        );
    }
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

/// A child restore that awaits an async API inline (like bash highlighting
/// its `$ command` header) must not error out of the sync `get_tool`
/// wrapper and degrade to the plain fallback body.
#[test]
fn child_restore_awaiting_async_api_keeps_its_body() {
    let snapshots = run_batch(SYNC_HL_CHILD_SRC, "cmd");
    let last = snapshots.last().expect("at least one batch snapshot");
    let text = last.text();
    assert!(
        text.contains("echo header-marker"),
        "child restore header must survive, got:\n{text}"
    );
}

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

fn run_batch(child_src: &str, tool: &str) -> Vec<BufferSnapshot> {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("stray_repro", &format!("{child_src}\n{BATCH_PLUGIN_SRC}"))
        .unwrap();

    let (tx, rx) = flume::unbounded();
    let event_tx = EventSender::new(tx, 0);
    let mut ctx = stub_ctx_with(&AgentMode::Build, Some(&event_tx), Some(BATCH_ID));
    ctx.registry = Arc::clone(&reg);

    let input = json!({ "tool_calls": [
        { "tool": tool, "parameters": {} },
        { "tool": tool, "parameters": {} },
    ]});
    let entry = reg.get("batch").unwrap();
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
