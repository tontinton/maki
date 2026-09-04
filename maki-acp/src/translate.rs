use std::path::{Path, PathBuf};

use agent_client_protocol_schema::{
    Content, ContentBlock, ContentChunk, Cost, Diff, ImageContent, SessionUpdate, StopReason,
    TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use maki_agent::DoneReason;
use maki_agent::tools::ToolRegistry;
use maki_agent::types::{ToolDoneEvent, ToolOutput, ToolStartEvent, TurnCompleteEvent};
use maki_providers::{ContentBlock as MsgBlock, ImageMediaType, Message, Role as MsgRole};

const MIN_FENCE_LEN: usize = 3;
/// Model pricing is quoted in US dollars, so that is the reported currency.
const CURRENCY: &str = "USD";

/// File-level tools report a location so the client can follow along. Directory
/// scoped tools (glob, grep, list) and commands (bash) target no single file.
const FILE_TOOLS: &[&str] = &[
    "read",
    "write",
    "edit",
    "multiedit",
    "edit_lines",
    "insert_lines",
    "index",
    "view_image",
];

/// Zed renders tool output as markdown, so bare text loses its newlines.
/// We wrap it in a backtick fence (longer than any run inside the text)
/// to keep the original formatting.
fn fenced(text: &str) -> String {
    let longest_backtick_run = text
        .split(|c: char| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(MIN_FENCE_LEN.max(longest_backtick_run + 1));
    format!("{fence}\n{text}\n{fence}")
}

pub fn tool_kind(name: &str) -> ToolKind {
    let entry = match ToolRegistry::global().get(name) {
        Some(e) => e,
        None => return ToolKind::Other,
    };
    entry
        .tool
        .tool_kind()
        .map(parse_tool_kind)
        .unwrap_or(ToolKind::Other)
}

fn parse_tool_kind(s: &str) -> ToolKind {
    match s {
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" => ToolKind::Move,
        "search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "think" => ToolKind::Think,
        "fetch" => ToolKind::Fetch,
        "switch_mode" => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

pub fn text_delta(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text.to_string(),
    ))))
}

pub fn thinking_delta(text: &str) -> SessionUpdate {
    SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text.to_string(),
    ))))
}

pub fn tool_pending(id: &str, name: &str) -> SessionUpdate {
    let kind = tool_kind(name);
    SessionUpdate::ToolCall(
        ToolCall::new(ToolCallId::from(id.to_string()), name.to_string())
            .kind(kind)
            .status(ToolCallStatus::Pending),
    )
}

pub fn tool_start(event: &ToolStartEvent, cwd: &Path, home: Option<&Path>) -> SessionUpdate {
    let mut fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::InProgress)
        .title(event.summary.clone());

    if let Some(raw) = &event.raw_input {
        fields = fields.raw_input(raw.clone());
    }

    let locations = tool_locations(&event.tool, event.raw_input.as_ref(), cwd, home);
    if !locations.is_empty() {
        fields = fields.locations(locations);
    }

    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(event.id.clone()),
        fields,
    ))
}

/// File locations the tool call touches, per ACP "Following the Agent". The
/// client expects absolute paths, so `~` and relative paths are resolved
/// against the session's home and cwd.
fn tool_locations(
    tool: &str,
    raw_input: Option<&serde_json::Value>,
    cwd: &Path,
    home: Option<&Path>,
) -> Vec<ToolCallLocation> {
    if !FILE_TOOLS.contains(&tool) {
        return Vec::new();
    }
    let Some(raw) = raw_input else {
        return Vec::new();
    };
    let Some(path) = input_path(raw) else {
        return Vec::new();
    };
    let Some(resolved) = resolve_path(path, cwd, home) else {
        return Vec::new();
    };
    vec![location(resolved, input_line(tool, raw))]
}

/// The target file: `path`, or its schema alias `file_path` when the model
/// used that spelling.
fn input_path(raw_input: &serde_json::Value) -> Option<&str> {
    raw_input
        .get("path")
        .or_else(|| raw_input.get("file_path"))?
        .as_str()
        .filter(|s| !s.is_empty())
}

/// `~`, `~/x`, and relative paths become absolute; other `~` spellings
/// (`~user`) have no expansion. ACP clients require absolute paths in
/// locations, so anything unresolvable yields None instead of a bogus path.
fn resolve_path(raw: &str, cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if raw.starts_with('~') {
        if raw == "~" {
            return home.map(Path::to_path_buf);
        }
        let rest = raw.strip_prefix("~/")?;
        return home.map(|h| h.join(rest));
    }
    let p = Path::new(raw);
    Some(if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    })
}

/// The line a tool call focuses on, when its input says so. ACP `line` is
/// 0-based (Zed uses it as a buffer row), but tool inputs are 1-based.
fn input_line(tool: &str, raw_input: &serde_json::Value) -> Option<u32> {
    let key = match tool {
        "read" => "offset",
        "edit_lines" => "start",
        // insert_lines writes *after* `line`, so the new text's 0-based row
        // is the raw value itself, and 0 (insert at the top) is valid.
        "insert_lines" => return raw_input.get("line").and_then(as_number),
        _ => return None,
    };
    raw_input.get(key).and_then(as_line).map(|l| l - 1)
}

fn as_line(v: &serde_json::Value) -> Option<u32> {
    as_number(v).filter(|&l| l >= 1)
}

/// raw_input is pre-validation, so models sometimes send numbers as strings.
fn as_number(v: &serde_json::Value) -> Option<u32> {
    let n = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))?;
    u32::try_from(n).ok()
}

fn location(path: PathBuf, line: Option<u32>) -> ToolCallLocation {
    let mut loc = ToolCallLocation::new(path);
    if let Some(l) = line {
        loc = loc.line(l);
    }
    loc
}

/// A finished call's location comes from what it wrote. File tools already
/// reported that path at start, sometimes with a line, and an update replaces
/// locations instead of merging them, so re-reporting would drop the line the
/// client is following. Paths are already absolute (the plugins abspath them),
/// so resolve_path is a no-op here; it only guards against a relative slip.
fn done_locations(event: &ToolDoneEvent, cwd: &Path, home: Option<&Path>) -> Vec<ToolCallLocation> {
    if event.is_error || FILE_TOOLS.contains(&&*event.tool) {
        return Vec::new();
    }
    let Some(path) = event.written_path() else {
        return Vec::new();
    };
    let Some(resolved) = resolve_path(path, cwd, home) else {
        return Vec::new();
    };
    vec![location(resolved, None)]
}

pub fn tool_output(id: &str, content: &str) -> SessionUpdate {
    let fields = ToolCallUpdateFields::new().content(vec![ToolCallContent::Content(Content::new(
        ContentBlock::Text(TextContent::new(fenced(content))),
    ))]);
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(id.to_string()),
        fields,
    ))
}

pub fn tool_done(event: &ToolDoneEvent, cwd: &Path, home: Option<&Path>) -> SessionUpdate {
    let status = if event.is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };

    let content = match &event.output {
        ToolOutput::Diff {
            path,
            before,
            after,
            ..
        } => {
            let diff = if before.is_empty() {
                Diff::new(path.as_str(), after.clone())
            } else {
                Diff::new(path.as_str(), after.clone()).old_text(before.clone())
            };
            vec![ToolCallContent::Diff(diff)]
        }
        _ => {
            let text = event.output.as_text();
            if text.is_empty() {
                vec![]
            } else {
                vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
                    TextContent::new(fenced(&text)),
                )))]
            }
        }
    };

    let raw_text = event.output.as_text();
    let mut fields = ToolCallUpdateFields::new().status(status).content(content);
    if !raw_text.is_empty() {
        fields = fields.raw_output(serde_json::Value::String(raw_text));
    }

    let locations = done_locations(event, cwd, home);
    if !locations.is_empty() {
        fields = fields.locations(locations);
    }

    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(event.id.clone()),
        fields,
    ))
}

pub fn map_done_reason(reason: DoneReason) -> StopReason {
    match reason {
        DoneReason::EndTurn => StopReason::EndTurn,
        DoneReason::MaxTokens => StopReason::MaxTokens,
        DoneReason::MaxTurns => StopReason::MaxTurnRequests,
        DoneReason::Cancelled => StopReason::Cancelled,
        // Manual `/compact` isn't a turn boundary; ACP has no dedicated
        // stop reason for housekeeping, so surface it as EndTurn.
        DoneReason::Compact => StopReason::EndTurn,
    }
}

/// Per ACP "Session Usage Updates": the current context gauge plus the
/// session's cumulative cost. Each turn's event only carries its own turn's
/// share, so the caller tracks the running total across turns.
pub fn usage_update(event: &TurnCompleteEvent, cost_total: Option<f64>) -> SessionUpdate {
    let used = event
        .context_size
        .unwrap_or_else(|| event.usage.context_tokens()) as u64;
    let mut update = UsageUpdate::new(used, u64::from(event.context_window));
    if let Some(cost) = cost_total {
        update = update.cost(Cost::new(cost, CURRENCY));
    }
    SessionUpdate::UsageUpdate(update)
}

pub fn replay_history(messages: &[Message], cwd: &Path, home: Option<&Path>) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    for msg in messages {
        match msg.role {
            MsgRole::User => replay_user(msg, &mut updates),
            MsgRole::Assistant => replay_assistant(msg, &mut updates, cwd, home),
        }
    }
    updates
}

fn replay_user(msg: &Message, updates: &mut Vec<SessionUpdate>) {
    if msg.is_observation() {
        return;
    }
    if let Some(text) = msg.user_text() {
        updates.push(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.to_string())),
        )));
    }
    for block in &msg.content {
        match block {
            MsgBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => updates.push(replay_tool_result(tool_use_id, content, *is_error)),
            MsgBlock::Image { source } => {
                updates.push(SessionUpdate::UserMessageChunk(ContentChunk::new(
                    ContentBlock::Image(ImageContent::new(
                        source.data.to_string(),
                        mime_type(&source.media_type),
                    )),
                )));
            }
            _ => {}
        }
    }
}

fn replay_assistant(
    msg: &Message,
    updates: &mut Vec<SessionUpdate>,
    cwd: &Path,
    home: Option<&Path>,
) {
    for block in &msg.content {
        match block {
            MsgBlock::Text { text } => updates.push(text_delta(text)),
            MsgBlock::Thinking { thinking, .. } => updates.push(thinking_delta(thinking)),
            MsgBlock::ToolUse {
                id, name, input, ..
            } => {
                updates.push(replay_tool_call(id, name, input, cwd, home));
            }
            _ => {}
        }
    }
}

fn replay_tool_call(
    id: &str,
    name: &str,
    input: &serde_json::Value,
    cwd: &Path,
    home: Option<&Path>,
) -> SessionUpdate {
    let call = ToolCall::new(ToolCallId::from(id.to_string()), name.to_string())
        .kind(tool_kind(name))
        .status(ToolCallStatus::Pending)
        .raw_input(input.clone())
        .locations(tool_locations(name, Some(input), cwd, home));
    SessionUpdate::ToolCall(call)
}

fn replay_tool_result(id: &str, content: &str, is_error: bool) -> SessionUpdate {
    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    let mut fields = ToolCallUpdateFields::new().status(status);
    if !content.is_empty() {
        fields = fields.content(vec![ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new(fenced(content))),
        ))]);
    }
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(id.to_string()),
        fields,
    ))
}

fn mime_type(media: &ImageMediaType) -> &'static str {
    match media {
        ImageMediaType::Png => "image/png",
        ImageMediaType::Jpeg => "image/jpeg",
        ImageMediaType::Gif => "image/gif",
        ImageMediaType::Webp => "image/webp",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use maki_providers::ImageSource;
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const CWD: &str = "/home/user/project";
    const HOME: &str = "/home/user";

    #[test_case("1: mod render\n2: mod segment", "```\n1: mod render\n2: mod segment\n```" ; "plain_text_gets_default_fence")]
    #[test_case("has ```rust\ncode\n``` inside", "````\nhas ```rust\ncode\n``` inside\n````" ; "fence_longer_than_inner_backticks")]
    fn fenced_wraps_in_code_block(input: &str, expected: &str) {
        assert_eq!(fenced(input), expected);
    }

    /// The only pair whose names disagree, and the one ACP clients read to tell
    /// "the model stopped" from "the agent ran out of turns".
    #[test]
    fn max_turns_maps_to_max_turn_requests() {
        assert_eq!(
            map_done_reason(DoneReason::MaxTurns),
            StopReason::MaxTurnRequests
        );
    }

    fn assistant(content: Vec<MsgBlock>) -> Message {
        Message {
            role: MsgRole::Assistant,
            content,
            display_text: None,
            ..Default::default()
        }
    }

    fn updates_json(messages: &[Message]) -> Vec<serde_json::Value> {
        replay_history(messages, Path::new(CWD), Some(Path::new(HOME)))
            .iter()
            .map(|u| serde_json::to_value(u).unwrap())
            .collect()
    }

    #[test]
    fn replay_full_conversation_in_order() {
        let messages = vec![
            Message::user("hello".into()),
            assistant(vec![
                MsgBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: None,
                },
                MsgBlock::Text {
                    text: "let me check".into(),
                },
                MsgBlock::tool_use("tu-1", "bash", serde_json::json!({"command": "ls"})),
            ]),
            Message {
                role: MsgRole::User,
                content: vec![MsgBlock::ToolResult {
                    tool_use_id: "tu-1".into(),
                    content: "file.rs".into(),
                    is_error: false,
                }],
                display_text: None,
                ..Default::default()
            },
            assistant(vec![MsgBlock::Text {
                text: "done".into(),
            }]),
        ];

        let json = updates_json(&messages);
        assert_eq!(json.len(), 6);
        assert_eq!(json[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(json[0]["content"]["text"], "hello");
        assert_eq!(json[1]["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(json[1]["content"]["text"], "hmm");
        assert_eq!(json[2]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(json[2]["content"]["text"], "let me check");
        assert_eq!(json[3]["sessionUpdate"], "tool_call");
        assert_eq!(json[3]["toolCallId"], "tu-1");
        assert!(json[3]["kind"].is_null());
        assert_eq!(json[3]["rawInput"]["command"], "ls");
        assert_eq!(json[4]["sessionUpdate"], "tool_call_update");
        assert_eq!(json[4]["toolCallId"], "tu-1");
        assert_eq!(json[4]["status"], "completed");
        assert_eq!(
            json[4]["content"][0]["content"]["text"],
            "```\nfile.rs\n```"
        );
        assert_eq!(json[5]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(json[5]["content"]["text"], "done");
    }

    #[test]
    fn replay_prefers_display_text_over_model_text() {
        let msg = Message::user_display("expanded with context".into(), "what user typed".into());
        let json = updates_json(&[msg]);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["content"]["text"], "what user typed");
    }

    #[test]
    fn replay_hides_synthetic_messages() {
        assert!(updates_json(&[Message::synthetic("injected".into())]).is_empty());
    }

    #[test]
    fn replay_never_speaks_an_observation_as_the_user() {
        let obs = Message::observation("[monitor] build failed".into());
        assert!(updates_json(&[obs]).is_empty());
    }

    #[test]
    fn replay_failed_tool_result_maps_to_failed_status() {
        let msg = Message {
            role: MsgRole::User,
            content: vec![MsgBlock::ToolResult {
                tool_use_id: "tu-err".into(),
                content: "boom".into(),
                is_error: true,
            }],
            display_text: None,
            ..Default::default()
        };
        let json = updates_json(&[msg]);
        assert_eq!(json[0]["sessionUpdate"], "tool_call_update");
        assert_eq!(json[0]["status"], "failed");
    }

    #[test]
    fn replay_user_image_keeps_mime_type() {
        let msg = Message::user_with_images(
            String::new(),
            vec![ImageSource {
                media_type: ImageMediaType::Png,
                data: std::sync::Arc::from("b64data"),
            }],
        );
        let json = updates_json(&[msg]);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["sessionUpdate"], "user_message_chunk");
        assert_eq!(json[0]["content"]["type"], "image");
        assert_eq!(json[0]["content"]["mimeType"], "image/png");
        assert_eq!(json[0]["content"]["data"], "b64data");
    }

    #[test_case("read", ToolKind::Read ; "read")]
    #[test_case("edit", ToolKind::Edit ; "edit")]
    #[test_case("delete", ToolKind::Delete ; "delete")]
    #[test_case("move", ToolKind::Move ; "move_kind")]
    #[test_case("search", ToolKind::Search ; "search")]
    #[test_case("execute", ToolKind::Execute ; "execute")]
    #[test_case("think", ToolKind::Think ; "think")]
    #[test_case("fetch", ToolKind::Fetch ; "fetch")]
    #[test_case("switch_mode", ToolKind::SwitchMode ; "switch_mode")]
    #[test_case("other", ToolKind::Other ; "other")]
    #[test_case("bogus", ToolKind::Other ; "unknown_maps_to_other")]
    fn parse_tool_kind_maps_wire_strings(input: &str, expected: ToolKind) {
        assert_eq!(parse_tool_kind(input), expected);
    }

    #[test_case("nonexistent_plugin_tool", ToolKind::Other ; "unknown_tool_is_other")]
    fn tool_kind_from_registry(name: &str, expected: ToolKind) {
        assert_eq!(tool_kind(name), expected);
    }

    fn start_event(tool: &str, raw_input: Option<serde_json::Value>) -> ToolStartEvent {
        ToolStartEvent {
            id: "t-1".into(),
            tool: Arc::from(tool),
            summary: String::new(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input,
            output: None,
        }
    }

    fn start_locations(
        tool: &str,
        raw_input: Option<serde_json::Value>,
    ) -> Option<serde_json::Value> {
        let update = tool_start(
            &start_event(tool, raw_input),
            Path::new(CWD),
            Some(Path::new(HOME)),
        );
        serde_json::to_value(update)
            .unwrap()
            .get("locations")
            .cloned()
    }

    #[test_case("read", Some(json!({"path": "/a/b.rs", "offset": 42, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 41}])) ; "read_absolute_with_offset")]
    #[test_case("read", Some(json!({"path": "/a/b.rs", "offset": 1, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 0}])) ; "offset_one_reports_line_zero")]
    #[test_case("read", Some(json!({"path": "src/lib.rs", "offset": 1, "limit": 0})), Some(json!([{"path": "/home/user/project/src/lib.rs", "line": 0}])) ; "read_relative_resolved_against_cwd")]
    #[test_case("read", Some(json!({"path": "~/notes.md", "offset": 1, "limit": 0})), Some(json!([{"path": "/home/user/notes.md", "line": 0}])) ; "read_tilde_expanded")]
    #[test_case("read", Some(json!({"path": "~", "offset": 1, "limit": 0})), Some(json!([{"path": "/home/user", "line": 0}])) ; "read_bare_tilde_expands_to_home")]
    #[test_case("read", Some(json!({"path": "~other/x", "offset": 1, "limit": 0})), None ; "read_tilde_user_prefix_unresolvable")]
    #[test_case("read", Some(json!({"file_path": "/a/b.rs", "offset": 7, "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 6}])) ; "read_file_path_alias")]
    #[test_case("read", Some(json!({"path": "/a/b.rs", "offset": "42", "limit": 5})), Some(json!([{"path": "/a/b.rs", "line": 41}])) ; "read_string_offset_coerced")]
    #[test_case("write", Some(json!({"path": "/a/b.rs", "content": "x"})), Some(json!([{"path": "/a/b.rs"}])) ; "write_no_line")]
    #[test_case("edit", Some(json!({"path": "c.rs", "old_string": "a", "new_string": "b"})), Some(json!([{"path": "/home/user/project/c.rs"}])) ; "edit_relative_no_line")]
    #[test_case("multiedit", Some(json!({"path": "c.rs", "edits": []})), Some(json!([{"path": "/home/user/project/c.rs"}])) ; "multiedit_relative_no_line")]
    #[test_case("edit_lines", Some(json!({"path": "/a", "start": 3, "end": 9, "new_string": "n"})), Some(json!([{"path": "/a", "line": 2}])) ; "edit_lines_start_becomes_line")]
    #[test_case("insert_lines", Some(json!({"path": "/a", "line": 5, "new_string": "n"})), Some(json!([{"path": "/a", "line": 5}])) ; "insert_lines_reports_the_inserted_row")]
    #[test_case("insert_lines", Some(json!({"path": "/a", "line": 0, "new_string": "n"})), Some(json!([{"path": "/a", "line": 0}])) ; "insert_lines_at_top")]
    #[test_case("index", Some(json!({"path": "/a/b.rs"})), Some(json!([{"path": "/a/b.rs"}])) ; "index_file_path")]
    #[test_case("view_image", Some(json!({"path": "img.png"})), Some(json!([{"path": "/home/user/project/img.png"}])) ; "view_image_relative")]
    #[test_case("glob", Some(json!({"pattern": "*.rs", "path": "src"})), None ; "glob_directory_path_ignored")]
    #[test_case("grep", Some(json!({"pattern": "x", "path": "src"})), None ; "grep_directory_path_ignored")]
    #[test_case("bash", Some(json!({"command": "ls", "workdir": "/tmp"})), None ; "bash_workdir_ignored")]
    #[test_case("read", None, None ; "missing_input_no_locations")]
    #[test_case("read", Some(json!({"offset": 1, "limit": 0})), None ; "missing_path_no_locations")]
    #[test_case("read", Some(json!({"path": "", "offset": 1, "limit": 0})), None ; "empty_path_no_locations")]
    fn tool_start_locations(
        tool: &str,
        input: Option<serde_json::Value>,
        expected: Option<serde_json::Value>,
    ) {
        assert_eq!(start_locations(tool, input), expected);
    }

    #[test]
    fn tool_start_with_no_raw_input_has_no_locations_field() {
        let update = tool_start(
            &start_event("read", None),
            Path::new(CWD),
            Some(Path::new(HOME)),
        );
        let json = serde_json::to_value(update).unwrap();
        assert!(
            json.get("locations").is_none(),
            "empty locations must be omitted: {json}"
        );
    }

    fn done_event(
        tool: &str,
        output: ToolOutput,
        is_error: bool,
        written: Option<&str>,
    ) -> ToolDoneEvent {
        ToolDoneEvent {
            id: "t-1".into(),
            tool: Arc::from(tool),
            output,
            is_error,
            annotation: None,
            written_path: written.map(str::to_owned),
        }
    }

    fn done_locations_of(event: &ToolDoneEvent) -> Option<serde_json::Value> {
        let update = tool_done(event, Path::new(CWD), Some(Path::new(HOME)));
        serde_json::to_value(update)
            .unwrap()
            .get("locations")
            .cloned()
    }

    #[test]
    fn done_written_path_reports_location_without_line() {
        let event = done_event(
            "memory",
            ToolOutput::Plain("wrote 3 bytes".into()),
            false,
            Some("/home/user/project/a.rs"),
        );
        assert_eq!(
            done_locations_of(&event),
            Some(json!([{"path": "/home/user/project/a.rs"}]))
        );
    }

    /// A file tool's start event already reported the path, with the line the
    /// client is following. Re-reporting it here would replace that line.
    #[test_case("edit_lines" ; "edit_lines_keeps_start_line")]
    #[test_case("insert_lines" ; "insert_lines_keeps_start_line")]
    #[test_case("write" ; "write_keeps_start_location")]
    fn done_file_tool_omits_written_path_location(tool: &str) {
        let event = done_event(
            tool,
            ToolOutput::Plain("wrote 3 bytes".into()),
            false,
            Some("/home/user/project/a.rs"),
        );
        assert_eq!(done_locations_of(&event), None);
    }

    #[test]
    fn done_error_suppresses_written_path_location() {
        let event = done_event(
            "memory",
            ToolOutput::Plain("write error: permission denied".into()),
            true,
            Some("/home/user/project/a.rs"),
        );
        assert_eq!(done_locations_of(&event), None);
    }

    #[test]
    fn done_without_file_output_has_no_locations() {
        let event = done_event("memory", ToolOutput::Plain("done".into()), false, None);
        assert_eq!(done_locations_of(&event), None);
    }

    #[test]
    fn replay_tool_use_reports_locations() {
        let msg = assistant(vec![MsgBlock::tool_use(
            "tu-1",
            "read",
            json!({"path": "src/lib.rs", "offset": 10, "limit": 0}),
        )]);
        let json = updates_json(&[msg]);
        assert_eq!(json.len(), 1);
        assert_eq!(json[0]["sessionUpdate"], "tool_call");
        assert_eq!(json[0]["toolCallId"], "tu-1");
        assert_eq!(
            json[0]["locations"],
            json!([{"path": "/home/user/project/src/lib.rs", "line": 9}])
        );
    }

    #[test]
    fn replay_non_file_tool_has_no_locations() {
        let msg = assistant(vec![MsgBlock::tool_use(
            "tu-1",
            "bash",
            json!({"command": "ls"}),
        )]);
        let json = updates_json(&[msg]);
        assert!(json[0].get("locations").is_none(), "{:?}", json[0]);
    }

    fn turn_event(
        context_size: Option<u32>,
        context_window: u32,
        cost: Option<f64>,
    ) -> TurnCompleteEvent {
        TurnCompleteEvent {
            message: Message::default(),
            usage: maki_providers::TokenUsage {
                input: 1_000,
                output: 200,
                cache_creation: 0,
                cache_read: 50_000,
                ..Default::default()
            },
            model: "test-model".into(),
            cost,
            list_cost: None,
            context_size,
            context_window,
        }
    }

    #[test]
    fn usage_update_reports_gauge_and_cumulative_cost() {
        let event = turn_event(Some(60_000), 200_000, Some(0.05));
        let json = serde_json::to_value(usage_update(&event, Some(0.125))).unwrap();
        assert_eq!(json["sessionUpdate"], "usage_update");
        assert_eq!(json["used"], 60_000);
        assert_eq!(json["size"], 200_000);
        assert_eq!(json["cost"]["amount"], 0.125);
        assert_eq!(json["cost"]["currency"], CURRENCY);
    }

    #[test]
    fn usage_update_without_cost_omits_cost() {
        let event = turn_event(Some(60_000), 200_000, None);
        let json = serde_json::to_value(usage_update(&event, None)).unwrap();
        assert_eq!(json["used"], 60_000);
        assert_eq!(json["size"], 200_000);
        assert!(json.get("cost").is_none(), "{json}");
    }

    #[test]
    fn usage_update_falls_back_to_usage_when_context_size_missing() {
        let event = turn_event(None, 200_000, None);
        let json = serde_json::to_value(usage_update(&event, None)).unwrap();
        assert_eq!(json["used"], 51_200);
    }
}
