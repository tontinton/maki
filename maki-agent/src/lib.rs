pub mod agent;
pub(crate) mod anthropic;
pub mod auth;
pub mod model;
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
    fn tool_results_from_done_events() {
        let events = vec![
            (
                "id1".into(),
                ToolDoneEvent {
                    tool: "write",
                    content: "wrote 5 bytes".into(),
                    is_error: false,
                },
            ),
            (
                "id2".into(),
                ToolDoneEvent {
                    tool: "glob",
                    content: "err".into(),
                    is_error: true,
                },
            ),
        ];
        let msg = Message::tool_results(events);
        assert_eq!(msg.content.len(), 2);
        assert!(matches!(
            &msg.content[0],
            ContentBlock::ToolResult { tool_use_id, content, is_error: false }
            if tool_use_id == "id1" && content == "wrote 5 bytes"
        ));
        assert!(matches!(
            &msg.content[1],
            ContentBlock::ToolResult { tool_use_id, content, is_error: true }
            if tool_use_id == "id2" && content == "err"
        ));
    }

    #[test]
    fn agent_event_type_tags() {
        let cases: Vec<(AgentEvent, &str)> = vec![
            (AgentEvent::TextDelta { text: "x".into() }, "text_delta"),
            (
                AgentEvent::ToolStart(ToolStartEvent {
                    tool: "bash",
                    summary: "s".into(),
                }),
                "tool_start",
            ),
            (
                AgentEvent::ToolDone(ToolDoneEvent {
                    tool: "read",
                    content: "c".into(),
                    is_error: false,
                }),
                "tool_done",
            ),
            (
                AgentEvent::Done {
                    usage: TokenUsage::default(),
                    num_turns: 1,
                    stop_reason: None,
                },
                "done",
            ),
            (
                AgentEvent::Error {
                    message: "e".into(),
                },
                "error",
            ),
        ];
        for (event, expected_type) in cases {
            let json: Value = serde_json::to_value(&event).unwrap();
            assert_eq!(json["type"], expected_type);
        }
    }

    #[test]
    fn effective_message_without_plan() {
        let input = AgentInput {
            message: "do stuff".into(),
            mode: AgentMode::Build,
            pending_plan: None,
        };
        assert_eq!(input.effective_message(), "do stuff");
    }

    #[test]
    fn effective_message_injects_plan_in_build_mode() {
        let input = AgentInput {
            message: "go".into(),
            mode: AgentMode::Build,
            pending_plan: Some("/tmp/plan.md".into()),
        };
        let msg = input.effective_message();
        assert!(msg.contains("/tmp/plan.md"));
        assert!(msg.contains("go"));
    }

    #[test]
    fn effective_message_ignores_plan_in_plan_mode() {
        let input = AgentInput {
            message: "plan this".into(),
            mode: AgentMode::Plan("/tmp/p.md".into()),
            pending_plan: Some("/tmp/p.md".into()),
        };
        assert_eq!(input.effective_message(), "plan this");
    }
}
