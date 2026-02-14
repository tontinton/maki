pub mod agent;
pub(crate) mod anthropic;
pub mod auth;
pub mod model;
pub(crate) mod prompt;
pub mod provider;
pub mod tool;
pub(crate) mod zai;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use model::{Model, ModelError, ModelPricing, TokenUsage};

const DATA_DIR_NAME: &str = ".maki";
pub const PLANS_DIR: &str = "plans";
const SCRUB_MAX_LINES: usize = 1000;
const SCRUB_TIERS: &[(usize, usize)] = &[(1000, 2), (500, 3), (100, 5)];

pub fn data_dir() -> Result<PathBuf, AgentError> {
    let home = env::var("HOME").map_err(|_| AgentError::Api {
        status: 0,
        message: "HOME not set".into(),
    })?;
    let dir = PathBuf::from(home).join(DATA_DIR_NAME);
    fs::create_dir_all(&dir).map_err(AgentError::Io)?;
    Ok(dir)
}

pub fn new_plan_path() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let plan_dir = data_dir()
        .map(|d| d.join(PLANS_DIR))
        .unwrap_or_else(|_| PLANS_DIR.into());
    format!("{}/{ts}.md", plan_dir.display())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AgentMode {
    #[default]
    Build,
    Plan(String),
}

pub struct AgentInput {
    pub message: String,
    pub mode: AgentMode,
    pub pending_plan: Option<String>,
}

impl AgentInput {
    pub fn effective_message(&self) -> String {
        match &self.pending_plan {
            Some(path) if self.mode == AgentMode::Build => {
                format!(
                    "A plan was written to {path}. Follow the plan.\n\n{}",
                    self.message
                )
            }
            _ => self.message.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text }],
        }
    }

    pub fn tool_results(results: Vec<(String, ToolDoneEvent)>) -> Self {
        Self {
            role: Role::User,
            content: results
                .into_iter()
                .map(|(id, output)| ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: output.content,
                    is_error: output.is_error,
                })
                .collect(),
        }
    }

    /// Replace the stale tool contents with a short summary.
    /// The model's `ToolUse` block stores the entire written file in `input.content`,
    /// and because we resend the full conversation history on every API call,
    /// a single 500-line `write`, for example, permanently adds ~5-10k tokens to every request.
    /// The tool result already confirms success, so the model doesn't need the content again.
    pub fn scrub_tool_use_inputs(&mut self, successful_ids: &[&str]) {
        for block in &mut self.content {
            if let ContentBlock::ToolUse { id, name, input } = block
                && successful_ids.contains(&id.as_str())
            {
                match name.as_str() {
                    tool::ToolCall::WRITE => {
                        if let Some(content) = input.get("content").and_then(|v| v.as_str()) {
                            let lines = content.lines().count();
                            let bytes = content.len();
                            input["content"] =
                                Value::String(format!("[{lines} lines, {bytes} bytes]"));
                        }
                    }
                    tool::ToolCall::EDIT => {
                        for key in &["old_string", "new_string"] {
                            if let Some(v) = input.get(*key).and_then(|v| v.as_str()) {
                                input[*key] = Value::String(format!("[{} bytes]", v.len()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn scrub_target_name<'a>(msg: &'a Message, tool_use_id: &str) -> Option<&'a str> {
    msg.content.iter().find_map(|b| match b {
        ContentBlock::ToolUse { id, name, .. }
            if id == tool_use_id
                && matches!(
                    name.as_str(),
                    tool::ToolCall::READ | tool::ToolCall::GREP | tool::ToolCall::GLOB
                ) =>
        {
            Some(name.as_str())
        }
        _ => None,
    })
}

fn scrub_summary(name: &str, content: &str) -> String {
    match name {
        tool::ToolCall::READ => format!("[read: {} lines]", content.lines().count()),
        tool::ToolCall::GREP => {
            let matches = content.lines().filter(|l| l.starts_with("  ")).count();
            format!("[grep: {matches} matches]")
        }
        tool::ToolCall::GLOB => format!("[glob: {} files]", content.lines().count()),
        _ => format!("[{name}: scrubbed]"),
    }
}

fn truncate_to_lines(content: &str, max: usize) -> String {
    let total = content.lines().count();
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        if i >= max {
            out.push_str(&format!("\n[truncated, showing {max} of {total} lines]"));
            break;
        }
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

fn assistant_turns_after(history: &[Message], from: usize) -> usize {
    history[from + 1..]
        .iter()
        .filter(|m| matches!(m.role, Role::Assistant))
        .count()
}

pub fn scrub_stale_tool_results(history: &mut [Message]) {
    for i in 1..history.len() {
        let turns_ago = assistant_turns_after(history, i);
        let (before, current) = history.split_at_mut(i);

        for block in &mut current[0].content {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
            else {
                continue;
            };
            if *is_error || content.starts_with('[') {
                continue;
            }
            let Some(name) = scrub_target_name(&before[i - 1], tool_use_id) else {
                continue;
            };

            let line_count = content.lines().count();
            let should_scrub = SCRUB_TIERS
                .iter()
                .any(|&(min_lines, min_turns)| line_count >= min_lines && turns_ago >= min_turns);

            if should_scrub {
                *content = scrub_summary(name, content);
            } else if line_count > SCRUB_MAX_LINES {
                *content = truncate_to_lines(content, SCRUB_MAX_LINES);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStartEvent {
    pub tool: &'static str,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDoneEvent {
    pub tool: &'static str,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    TextDelta {
        text: String,
    },
    ToolStart(ToolStartEvent),
    ToolDone(ToolDoneEvent),
    TurnComplete {
        message: Message,
        usage: TokenUsage,
        model: String,
    },
    ToolResultsSubmitted {
        message: Message,
    },
    Done {
        usage: TokenUsage,
        num_turns: u32,
        stop_reason: Option<String>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("tool error in {tool}: {message}")]
    Tool { tool: String, message: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] ureq::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("channel send failed")]
    Channel,
}

impl AgentError {
    pub fn from_response(response: ureq::http::Response<ureq::Body>) -> Self {
        let status = response.status().as_u16();
        let message = response
            .into_body()
            .read_to_string()
            .unwrap_or_else(|_| "unable to read error body".into());
        Self::Api { status, message }
    }
}

impl From<mpsc::SendError<AgentEvent>> for AgentError {
    fn from(_: mpsc::SendError<AgentEvent>) -> Self {
        Self::Channel
    }
}

pub struct PendingToolCall {
    pub id: String,
    pub call: tool::ToolCall,
}

pub struct StreamResponse {
    pub message: Message,
    pub tool_calls: Vec<PendingToolCall>,
    pub usage: TokenUsage,
    pub stop_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_message_with_and_without_plan() {
        let no_plan = AgentInput {
            message: "do stuff".into(),
            mode: AgentMode::Build,
            pending_plan: None,
        };
        assert_eq!(no_plan.effective_message(), "do stuff");

        let with_plan = AgentInput {
            message: "go".into(),
            mode: AgentMode::Build,
            pending_plan: Some("/tmp/plan.md".into()),
        };
        let msg = with_plan.effective_message();
        assert!(msg.contains("/tmp/plan.md"));
        assert!(msg.contains("go"));

        let plan_mode = AgentInput {
            message: "plan this".into(),
            mode: AgentMode::Plan("/tmp/p.md".into()),
            pending_plan: Some("/tmp/p.md".into()),
        };
        assert_eq!(plan_mode.effective_message(), "plan this");
    }

    fn tool_use_msg(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({}),
            }],
        }
    }

    fn tool_result_msg(tool_use_id: &str, content: &str, is_error: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
                is_error,
            }],
        }
    }

    fn result_content(history: &[Message], idx: usize) -> &str {
        match &history[idx].content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            _ => panic!("expected ToolResult"),
        }
    }

    fn make_lines(n: usize) -> String {
        (1..=n)
            .map(|i| format!("{i}: line"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn add_filler_turns(history: &mut Vec<Message>, n: usize) {
        for i in 0..n {
            let id = format!("filler_{i}");
            history.push(tool_use_msg(&id, "bash"));
            history.push(tool_result_msg(&id, "ok", false));
        }
    }

    #[test]
    fn scrub_ignores_small_results_and_non_targets() {
        let small = "1: fn main() {}";
        let mut history = vec![
            tool_use_msg("r1", "read"),
            tool_result_msg("r1", small, false),
            tool_use_msg("b1", "bash"),
            tool_result_msg("b1", &make_lines(200), false),
        ];
        add_filler_turns(&mut history, 10);
        scrub_stale_tool_results(&mut history);

        assert_eq!(result_content(&history, 1), small);
        assert!(!result_content(&history, 3).starts_with('['));
    }

    #[test]
    fn scrub_100_lines_after_5_turns() {
        let content = make_lines(150);
        let mut history = vec![
            tool_use_msg("r1", "read"),
            tool_result_msg("r1", &content, false),
        ];

        add_filler_turns(&mut history, 4);
        scrub_stale_tool_results(&mut history);
        assert!(!result_content(&history, 1).starts_with('['));

        add_filler_turns(&mut history, 1);
        scrub_stale_tool_results(&mut history);
        assert!(result_content(&history, 1).starts_with("[read:"));
    }

    #[test]
    fn scrub_500_lines_after_3_turns() {
        let content = make_lines(500);
        let mut history = vec![
            tool_use_msg("r1", "read"),
            tool_result_msg("r1", &content, false),
        ];

        add_filler_turns(&mut history, 2);
        scrub_stale_tool_results(&mut history);
        assert!(!result_content(&history, 1).starts_with('['));

        add_filler_turns(&mut history, 1);
        scrub_stale_tool_results(&mut history);
        assert!(result_content(&history, 1).starts_with("[read:"));
    }

    #[test]
    fn scrub_1000_lines_after_2_turns() {
        let content = make_lines(1000);
        let mut history = vec![
            tool_use_msg("g1", "grep"),
            tool_result_msg("g1", &content, false),
        ];

        add_filler_turns(&mut history, 1);
        scrub_stale_tool_results(&mut history);
        assert!(!result_content(&history, 1).starts_with('['));

        add_filler_turns(&mut history, 1);
        scrub_stale_tool_results(&mut history);
        assert!(result_content(&history, 1).starts_with("[grep:"));
    }

    #[test]
    fn truncate_to_max_lines_immediately() {
        let content = make_lines(1500);
        let mut history = vec![
            tool_use_msg("r1", "read"),
            tool_result_msg("r1", &content, false),
        ];
        scrub_stale_tool_results(&mut history);
        let result = result_content(&history, 1);
        assert!(!result.starts_with('['));
        assert!(result.contains("[truncated, showing 1000 of 1500 lines]"));
        assert_eq!(result.lines().count(), SCRUB_MAX_LINES + 1);
    }
}
