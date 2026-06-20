use agent_client_protocol_schema::{
    Content, ContentBlock, ContentChunk, Diff, ImageContent, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use maki_agent::tools::ToolRegistry;
use maki_agent::types::{BatchProgressEvent, ToolDoneEvent, ToolOutput, ToolStartEvent};
use maki_providers::{ContentBlock as MsgBlock, ImageMediaType, Message, Role as MsgRole};

const MIN_FENCE_LEN: usize = 3;

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
    let entry = match ToolRegistry::native().get(name) {
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

pub fn tool_start(event: &ToolStartEvent) -> SessionUpdate {
    let mut fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::InProgress)
        .title(event.summary.clone());

    if let Some(raw) = &event.raw_input {
        fields = fields.raw_input(raw.clone());
    }

    let mut locations = Vec::new();
    if event.input.is_some()
        && let Some(path) = input_path(event.raw_input.as_ref())
    {
        locations.push(ToolCallLocation::new(path));
    }
    if !locations.is_empty() {
        fields = fields.locations(locations);
    }

    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(event.id.clone()),
        fields,
    ))
}

fn input_path(raw_input: Option<&serde_json::Value>) -> Option<std::path::PathBuf> {
    raw_input
        .and_then(|v| v.get("path"))
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
}

pub fn batch_inner_start(event: &BatchProgressEvent) -> SessionUpdate {
    let title = event.summary.clone().unwrap_or_else(|| event.tool.clone());
    SessionUpdate::ToolCall(
        ToolCall::new(
            ToolCallId::from(format!("{}__{}", event.batch_id, event.index)),
            title,
        )
        .kind(tool_kind(&event.tool))
        .status(ToolCallStatus::InProgress),
    )
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

pub fn tool_done(event: &ToolDoneEvent) -> SessionUpdate {
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

    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::from(event.id.clone()),
        fields,
    ))
}

pub fn map_stop_reason(
    sr: Option<maki_providers::StopReason>,
) -> agent_client_protocol_schema::StopReason {
    match sr {
        Some(maki_providers::StopReason::EndTurn) | None => {
            agent_client_protocol_schema::StopReason::EndTurn
        }
        Some(maki_providers::StopReason::MaxTokens) => {
            agent_client_protocol_schema::StopReason::MaxTokens
        }
        Some(maki_providers::StopReason::ToolUse) => {
            agent_client_protocol_schema::StopReason::EndTurn
        }
    }
}

pub fn replay_history(messages: &[Message]) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    for msg in messages {
        match msg.role {
            MsgRole::User => replay_user(msg, &mut updates),
            MsgRole::Assistant => replay_assistant(msg, &mut updates),
        }
    }
    updates
}

fn replay_user(msg: &Message, updates: &mut Vec<SessionUpdate>) {
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

fn replay_assistant(msg: &Message, updates: &mut Vec<SessionUpdate>) {
    for block in &msg.content {
        match block {
            MsgBlock::Text { text } => updates.push(text_delta(text)),
            MsgBlock::Thinking { thinking, .. } => updates.push(thinking_delta(thinking)),
            MsgBlock::ToolUse { id, name, input } => {
                updates.push(replay_tool_call(id, name, input));
            }
            _ => {}
        }
    }
}

fn replay_tool_call(id: &str, name: &str, input: &serde_json::Value) -> SessionUpdate {
    SessionUpdate::ToolCall(
        ToolCall::new(ToolCallId::from(id.to_string()), name.to_string())
            .kind(tool_kind(name))
            .status(ToolCallStatus::Pending)
            .raw_input(input.clone()),
    )
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
    use maki_providers::ImageSource;
    use test_case::test_case;

    use super::*;

    #[test_case("1: mod render\n2: mod segment", "```\n1: mod render\n2: mod segment\n```" ; "plain_text_gets_default_fence")]
    #[test_case("has ```rust\ncode\n``` inside", "````\nhas ```rust\ncode\n``` inside\n````" ; "fence_longer_than_inner_backticks")]
    fn fenced_wraps_in_code_block(input: &str, expected: &str) {
        assert_eq!(fenced(input), expected);
    }

    #[test_case(Some("import"), "import"; "summary_becomes_title")]
    #[test_case(None, "grep"; "title_falls_back_to_tool_name")]
    fn batch_inner_start_composite_id_and_title(summary: Option<&str>, title: &str) {
        let event = BatchProgressEvent {
            batch_id: "b1".into(),
            index: 2,
            tool: "grep".into(),
            status: maki_agent::BatchToolStatus::InProgress,
            output: None,
            summary: summary.map(Into::into),
        };
        let json = serde_json::to_value(batch_inner_start(&event)).unwrap();
        assert_eq!(json["toolCallId"], "b1__2");
        assert_eq!(json["title"], title);
    }

    #[test_case(None; "missing_stop_reason")]
    #[test_case(Some(maki_providers::StopReason::ToolUse); "tool_use")]
    fn stop_reason_without_acp_equivalent_maps_to_end_turn(sr: Option<maki_providers::StopReason>) {
        assert_eq!(
            map_stop_reason(sr),
            agent_client_protocol_schema::StopReason::EndTurn
        );
    }

    fn assistant(content: Vec<MsgBlock>) -> Message {
        Message {
            role: MsgRole::Assistant,
            content,
            display_text: None,
        }
    }

    fn updates_json(messages: &[Message]) -> Vec<serde_json::Value> {
        replay_history(messages)
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
                MsgBlock::ToolUse {
                    id: "tu-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ]),
            Message {
                role: MsgRole::User,
                content: vec![MsgBlock::ToolResult {
                    tool_use_id: "tu-1".into(),
                    content: "file.rs".into(),
                    is_error: false,
                }],
                display_text: None,
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
    fn replay_failed_tool_result_maps_to_failed_status() {
        let msg = Message {
            role: MsgRole::User,
            content: vec![MsgBlock::ToolResult {
                tool_use_id: "tu-err".into(),
                content: "boom".into(),
                is_error: true,
            }],
            display_text: None,
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
}
