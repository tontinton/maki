//! Async agent loop with tools.
//!
//! `AgentMode::Build` executes tools freely; `Plan(path)` restricts writes to the plan file only.
//! `ExtractedCommand` injects control signals (interrupt, cancel, compact) into a running agent.
//! `AgentInput::effective_message` prepends plan context to the prompt when a pending plan exists in Build mode.

pub mod agent;
pub mod cancel;
pub mod mcp;
pub use mcp::config::McpServerInfo;
pub use mcp::config::McpServerStatus;
pub(crate) mod task_set;
pub use agent::{Agent, AgentParams, AgentRunParams, History, LoadedInstructions, RunOutcome};
pub use cancel::{CancelToken, CancelTrigger};
pub use maki_config::{AgentConfig, PermissionsConfig, UiConfig};
pub mod permissions;
pub(crate) mod prompt;
pub mod skill;
pub mod template;
pub mod tools;
pub mod types;

use std::path::PathBuf;

pub use maki_providers::AgentError;
use maki_providers::Message;
pub use maki_providers::{ImageMediaType, ImageSource};
pub use types::{
    AgentEvent, BatchProgressEvent, BatchToolEntry, BatchToolStatus, DiffHunk, DiffLine, DiffSpan,
    Envelope, EventSender, GrepFileEntry, GrepMatch, InstructionBlock, NO_FILES_FOUND,
    QuestionAnswer, QuestionInfo, QuestionOption, SubagentInfo, TodoItem, TodoPriority, TodoStatus,
    ToolDoneEvent, ToolInput, ToolOutput, ToolStartEvent, TurnCompleteEvent,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AgentMode {
    #[default]
    Build,
    Plan(PathBuf),
}

pub enum ExtractedCommand {
    Interrupt(AgentInput, u64),
    Cancel,
    Compact(u64),
    Ignore,
}

#[derive(Default)]
pub struct AgentInput {
    pub message: String,
    pub mode: AgentMode,
    pub pending_plan: Option<PathBuf>,
    pub images: Vec<ImageSource>,
    pub preamble: Vec<Message>,
}

impl AgentInput {
    pub fn effective_message(&self) -> String {
        match &self.pending_plan {
            Some(path) if self.mode == AgentMode::Build && path.exists() => {
                format!(
                    "A plan was written to {}. Follow the plan.\n\n{}",
                    path.display(),
                    self.message
                )
            }
            _ => self.message.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn effective_message_no_plan() {
        let input = AgentInput {
            message: "do stuff".into(),
            mode: AgentMode::Build,
            ..Default::default()
        };
        assert_eq!(input.effective_message(), "do stuff");
    }

    #[test]
    fn effective_message_with_existing_plan() {
        let dir = TempDir::new().unwrap();
        let plan_path = dir.path().join("plan.md");
        fs::write(&plan_path, "the plan").unwrap();
        let input = AgentInput {
            message: "go".into(),
            mode: AgentMode::Build,
            pending_plan: Some(plan_path.clone()),
            ..Default::default()
        };
        let msg = input.effective_message();
        assert!(msg.contains(plan_path.to_str().unwrap()));
        assert!(msg.contains("go"));
    }

    #[test]
    fn effective_message_skips_missing_plan() {
        let input = AgentInput {
            message: "go".into(),
            mode: AgentMode::Build,
            pending_plan: Some(PathBuf::from("/nonexistent/plan.md")),
            ..Default::default()
        };
        assert_eq!(input.effective_message(), "go");
    }

    #[test]
    fn effective_message_plan_mode_ignores_pending() {
        let input = AgentInput {
            message: "plan this".into(),
            mode: AgentMode::Plan(PathBuf::from("/tmp/p.md")),
            pending_plan: Some(PathBuf::from("/tmp/p.md")),
            ..Default::default()
        };
        assert_eq!(input.effective_message(), "plan this");
    }
}
