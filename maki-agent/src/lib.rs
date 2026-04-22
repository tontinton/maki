//! Async agent loop with tools.

pub mod agent;
pub mod cancel;
pub mod child_guard;
pub use child_guard::ChildGuard;
pub mod mcp;
pub use mcp::config::McpServerInfo;
pub use mcp::config::McpServerStatus;
pub use mcp::protocol::PromptRole;
pub use mcp::{McpCommand, McpHandle, McpPromptArg, McpPromptInfo, McpSnapshot, McpSnapshotReader};
pub(crate) mod task_set;
pub use agent::{
    Agent, AgentParams, AgentRunParams, History, Instructions, LoadedInstructions, RunOutcome,
};
pub use cancel::{CancelToken, CancelTrigger};
pub use maki_config::{AgentConfig, PermissionsConfig, UiConfig};
pub mod command;
pub mod diff;
pub mod permissions;
pub(crate) mod prompt;
pub mod skill;
pub mod template;
pub mod tools;
pub use tools::ToolFilter;
pub mod types;

use std::collections::HashMap;
use std::path::PathBuf;

pub use maki_providers::AgentError;
use maki_providers::Message;
pub use maki_providers::{ImageMediaType, ImageSource, ThinkingConfig};
pub use types::{
    AgentEvent, BatchProgressEvent, BatchToolEntry, BatchToolStatus, BufferSnapshot, Envelope,
    EventSender, GrepFileEntry, GrepLine, GrepMatchGroup, InstructionBlock, NO_FILES_FOUND,
    QuestionAnswer, QuestionInfo, QuestionOption, RawRenderHints, SnapshotLine, SnapshotSpan,
    SpanStyle, SubagentInfo, TodoItem, TodoPriority, TodoStatus, ToolDoneEvent, ToolInput,
    ToolOutput, ToolStartEvent, TurnCompleteEvent,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AgentMode {
    #[default]
    Build,
    Plan(PathBuf),
}

pub enum ExtractedCommand {
    Interrupt(AgentInput, u64),
    Compact(u64),
}

pub trait InterruptSource: Send + Sync {
    fn poll(&self) -> Option<ExtractedCommand>;
}

#[derive(Clone)]
pub struct McpPromptRef {
    pub qualified_name: String,
    pub arguments: HashMap<String, String>,
}

#[derive(Default)]
pub struct AgentInput {
    pub message: String,
    pub mode: AgentMode,
    pub images: Vec<ImageSource>,
    pub preamble: Vec<Message>,
    pub thinking: ThinkingConfig,
    pub prompt: Option<Box<McpPromptRef>>,
}
