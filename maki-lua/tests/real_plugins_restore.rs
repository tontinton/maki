//! Exercises real plugins (bash, grep, batch) through `request_restore`.
//! A broken restore silently falls back to raw LLM output, so we assert
//! things only the real views produce (gutters, command headers, truncation).

use std::env::current_dir;
use std::path::Path;
use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_agent::{AgentEvent, EventSender, SpanStyle};
use maki_config::ToolOutputLines;
use maki_highlight::highlight_code;
use maki_lua::test_support::runtime;
use maki_lua::{PluginHost, RestoreReason, Split, UiAction};
use maki_storage::id::{MakiId, SessionRef};
use serde_json::{Value, json};
use test_case::test_case;

const BASH_SRC: &str = include_str!("../../plugins/bash/init.lua");
const GREP_SRC: &str = include_str!("../../plugins/grep/init.lua");
/// The bash restore cases all run `echo`, and their output `hi` does not contain it.
const COMMAND_WORD: &str = "echo";
const BATCH_SRC: &str = include_str!("../../plugins/batch/init.lua");
const TODO_SRC: &str = include_str!("../../plugins/todo_write/init.lua");
const READ_SRC: &str = include_str!("../../plugins/read/init.lua");
/// Deeper than the 200 lines of context the read plugin highlights.
const READ_PADDING_LINES: usize = 300;
/// Kept short, so the context stays far below the highlighter's 16 KiB
/// prefix cut, which `maki-highlight` tests on its own. Long lines made a
/// loaded CI runner miss the 10 s restore deadline and lose the colors.
const READ_PADDING_BYTES: usize = 8;
const READ_COMMENT: &str = "/* comment";
const READ_EXCERPT: &str = "let value = 1;";
const READ_LANG: &str = "rust";

/// Only the real ToolView emits this when collapsed.
const EXPAND_HINT: &str = "click to expand";
/// Fixed caps so truncation tests don't depend on the product defaults. The
/// index and read caps differ so a body rendered through the wrong view is
/// visibly different.
const VIEW_CAP: usize = 3;
const INDEX_VIEW_CAP: usize = 2;
const READ_VIEW_CAP: usize = 5;
const TODO_TOOL: &str = "todo_write";
const TODO_ITEM: &str = "wire the panel";
const SUBAGENT_TASK_ID: &str = "toolu_sub";
const SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";

fn session_ref() -> SessionRef {
    SESSION_ID.parse().expect("valid session id")
}

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

fn restore_item(
    tool: &str,
    input: Value,
    output: &str,
    state: Option<Value>,
    clicks: Vec<usize>,
) -> maki_lua::RestoreItem {
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
        session_id: None,
        task_id: None,
        reason: RestoreReason::default(),
    }
}

/// Stamped the way the UI stamps a subagent chat's calls while its session loads.
fn in_subagent_load(mut item: maki_lua::RestoreItem) -> maki_lua::RestoreItem {
    item.session_id = Some(session_ref());
    item.task_id = Some(Arc::from(SUBAGENT_TASK_ID));
    item.reason = RestoreReason::Load;
    item
}

fn restore(
    host: &PluginHost,
    tool: &str,
    input: Value,
    output: &str,
    state: Option<Value>,
    clicks: Vec<usize>,
) -> Restored {
    run_restore(host, restore_item(tool, input, output, state, clicks))
}

fn run_restore(host: &PluginHost, item: maki_lua::RestoreItem) -> Restored {
    let handle = host.event_handle();
    let (tx, rx) = flume::unbounded();
    handle.request_restore(item, maki_agent::EventSender::new(tx, 0));
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

#[test_case::test_case("\n"; "lf")]
#[test_case::test_case("\r\n"; "crlf")]
fn read_restore_keeps_nearby_comment_colors_with_large_context(ending: &str) {
    let dir = tempfile::TempDir::new().unwrap();
    let shallow = dir.path().join("shallow.rs");
    let deep = dir.path().join("deep.rs");
    let source = format!("{READ_COMMENT}{ending}{READ_EXCERPT}{ending}");
    let padding = format!("// {}{ending}", "x".repeat(READ_PADDING_BYTES));
    let deep_source = format!("{}{source}", padding.repeat(READ_PADDING_LINES));
    std::fs::write(&shallow, &source).unwrap();
    std::fs::write(&deep, &deep_source).unwrap();
    // Restore tasks get 10s, and compiling rust regexes in a cold debug build can take longer.
    highlight_code(READ_LANG, &deep_source, "");

    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source("read", READ_SRC).unwrap();
    let render = |path: &Path, offset: usize| {
        let handle = host.event_handle();
        let (tx, rx) = flume::unbounded();
        handle.request_restore(
            restore_item(
                "read",
                json!({ "path": path, "offset": offset, "limit": 1 }),
                &format!("{offset}: {READ_EXCERPT}"),
                None,
                Vec::new(),
            ),
            EventSender::new(tx, 0),
        );
        handle.wait_restore_complete_for_test();
        host.load_source("barrier", "").unwrap();
        let snapshot = rx
            .drain()
            .filter_map(|env| match env.event {
                AgentEvent::ToolSnapshot { snapshot, .. } => Some(snapshot),
                _ => None,
            })
            .last()
            .expect("no restored read snapshot");
        assert_eq!(snapshot.text(), format!("{offset} {READ_EXCERPT}"));
        let spans = snapshot.lines[0].spans[1..].to_vec();
        assert!(
            spans
                .iter()
                .any(|span| matches!(span.style, SpanStyle::Inline(_)))
        );
        spans
    };
    assert_eq!(render(&deep, READ_PADDING_LINES + 2), render(&shallow, 2));
}

/// The body shows the highlighted command, so the title only carries what
/// the model says the command does. A description that is blank or just the
/// command says nothing. The directory of a leading `cd` stays, since the
/// body leaves the `cd` out.
#[test_case(Some("print hi"), "echo hi", "print hi" ; "described")]
#[test_case(None, "echo hi", "" ; "undescribed")]
#[test_case(Some("echo hi"), "echo hi", "" ; "described_by_its_command")]
#[test_case(Some(" "), "echo hi", "" ; "blank_description")]
#[test_case(None, "cd src && echo hi", "in src" ; "undescribed_past_a_cd")]
#[test_case(Some("print hi"), "cd src && echo hi", "print hi in src" ; "described_past_a_cd")]
#[test_case(Some("echo hi"), "cd src && echo hi", "in src" ; "described_by_its_command_past_a_cd")]
fn bash_restore_titles_a_call_and_shows_its_command(
    description: Option<&str>,
    command: &str,
    header: &str,
) {
    let host = load_host();
    let r = restore(
        &host,
        "bash",
        json!({ "command": command, "description": description }),
        "hi",
        None,
        Vec::new(),
    );
    assert_eq!(r.header, header);
    assert!(r.body.contains(COMMAND_WORD), "{}", r.body);
}

/// The session's own directory tells the reader nothing.
#[test]
fn bash_title_leaves_out_the_session_directory() {
    let cwd = current_dir().unwrap();
    let host = load_host();
    let r = restore(
        &host,
        "bash",
        json!({ "command": "echo hi", "description": "print hi", "workdir": cwd }),
        "hi",
        None,
        Vec::new(),
    );
    assert_eq!(r.header, "print hi");
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
    let result = runtime().block_on(inv.execute(&ctx));
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

const SKILL_TOOL: &str = "skill";
const SKILL_NAME: &str = "release";
/// Outside any home or cwd, so `shorten_path` leaves it alone.
const SKILL_LOCATION: &str = "/srv/skills/release/SKILL.md";
const SKILL_BODY: &str = "1 # Release\n2 Tag it.";

/// Restore (session reload, batch child) must draw the skill like the live
/// call did: content behind a line-number gutter, with the location in
/// the header, not the llm's numbered text with the location on top.
#[test]
fn skill_restore_renders_content_not_llm_text() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
    let output = format!("{SKILL_LOCATION}\n   1 | # Release\n   2 | Tag it.");
    let r = restore(
        &host,
        SKILL_TOOL,
        json!({ "name": SKILL_NAME }),
        &output,
        None,
        Vec::new(),
    );
    assert_eq!(r.body, SKILL_BODY);
    assert_eq!(r.header, SKILL_LOCATION);
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
        "live dir listing is annotated with entry count"
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

/// Nothing else runs the real plugin's restore. It draws the call's list into
/// the chat from its input alone, so a failed call renders the same.
#[test]
fn todo_write_restore_renders_the_list() {
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source(TODO_TOOL, TODO_SRC).unwrap();
    let input = json!({ "todos": [{ "content": TODO_ITEM, "status": "in_progress" }] });
    let item = in_subagent_load(restore_item(TODO_TOOL, input, "", None, Vec::new()));

    let r = run_restore(&host, item);

    assert!(r.body.contains(TODO_ITEM), "list missing: {}", r.body);
}

/// A session load replays a transcript, so its restores run with the panel
/// closed. Refilling per-session state here would pop the panel open on the
/// next focus change, resurrecting a list the turn already finished.
#[test]
fn todo_load_restore_leaves_the_panel_closed() {
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source(TODO_TOOL, TODO_SRC).unwrap();
    let input = json!({ "todos": [{ "content": TODO_ITEM, "status": "in_progress" }] });
    run_restore(
        &host,
        in_subagent_load(restore_item(TODO_TOOL, input, "", None, Vec::new())),
    );

    // Focus what a restore-time store would have written: the calling chat's
    // own ids.
    let sid = session_ref().id().to_string();
    host.event_handle().fire_autocmd(
        "TaskFocusChanged",
        json!({ "session_id": sid, "id": SUBAGENT_TASK_ID }),
    );
    host.load_source("barrier", "").unwrap();

    let opened = host.ui_action_rx().drain().any(
        |action| matches!(action, UiAction::OpenWin { config, .. } if config.split == Split::Panel),
    );
    assert!(!opened, "a load restore must not refill the panel");
}

/// Each tab gets a panel of its own. The plugin used to share one window, and
/// it stayed in the tab it first opened in, so every other tab's list was
/// drawn there, out of sight.
#[test]
fn todo_panel_opens_in_the_tab_of_each_writing_session() {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(TODO_TOOL, TODO_SRC).unwrap();
    let sessions = [MakiId::generate(), MakiId::generate()];
    let input = json!({ "todos": [{ "content": TODO_ITEM, "status": "in_progress" }] });
    let tool = &reg.get(TODO_TOOL).unwrap().tool;

    for session in sessions {
        let mut ctx = maki_agent::tools::test_support::stub_ctx(&maki_agent::AgentMode::Build);
        ctx.session_id = Some(SessionRef::from_id(session));
        runtime().block_on(tool.parse(&input).unwrap().execute(&ctx));
    }

    let owners: Vec<_> = host
        .ui_action_rx()
        .drain()
        .filter_map(|action| match action {
            UiAction::OpenWin {
                session, config, ..
            } if config.split == Split::Panel => Some(session),
            _ => None,
        })
        .collect();
    assert_eq!(owners, sessions.map(Some));
}

const WHICH_TASK_SRC: &str = r#"
maki.api.register_tool({
    name = "which_task",
    description = "reports the calling task",
    schema = { type = "object", properties = {}, additionalProperties = false },
    handler = function() return "" end,
    restore = function(_input, _output, _is_error, rctx)
        local buf = maki.ui.buf()
        buf:line("task:" .. rctx:task_id())
        buf:line("session:" .. tostring(rctx:session_id()))
        buf:line("reason:" .. tostring(rctx:restore_reason()))
        return buf
    end,
})
"#;

/// Batch drives child restores through a plain table ctx, so the chat and
/// the reason have to travel with it or a batched call would pass for a
/// rerender in the main chat.
#[test]
fn batch_child_restore_sees_the_chat_and_reason() {
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    host.load_source("which_task", WHICH_TASK_SRC).unwrap();
    host.load_source("batch", BATCH_SRC).unwrap();
    let input = json!({ "tool_calls": [{ "tool": "which_task", "parameters": {} }] });
    let state =
        json!({ "children": [{ "tool": "which_task", "status": "success", "output": "" }] });

    let r = run_restore(
        &host,
        in_subagent_load(restore_item("batch", input, "", Some(state), Vec::new())),
    );

    for expected in [
        format!("task:{SUBAGENT_TASK_ID}"),
        format!("session:{}", session_ref().id()),
        "reason:load".to_owned(),
    ] {
        assert!(
            r.body.contains(&expected),
            "expected {expected:?} in: {}",
            r.body
        );
    }
}
