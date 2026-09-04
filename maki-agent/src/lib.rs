//! Async agent loop with tools.

pub mod agent;
pub mod cancel;
pub mod child_guard;
pub use child_guard::ChildGuard;
pub mod headless;
pub mod mailbox;
pub mod mcp;
pub use mcp::config::{McpConfigError, McpConfigErrors, McpServerInfo, McpServerStatus};
pub use mcp::protocol::PromptRole;
pub use mcp::{
    McpCommand, McpHandle, McpPromptArg, McpPromptInfo, McpSession, McpSnapshot, McpSnapshotReader,
};
pub(crate) mod task_set;
pub use agent::{
    Agent, AgentParams, AgentRunParams, History, HistorySnapshot, Instructions, LoadedInstructions,
    SharedMessages, UNAVAILABLE_RESULT, close_dangling_tool_calls, find_subdirectory_instructions,
    is_instruction_file,
};
pub use cancel::{CancelMap, CancelToken, CancelTrigger};
pub use mailbox::{MailboxError, SessionMailbox};
pub use maki_config::{AgentConfig, PermissionsConfig, ToolOutputLines};
pub mod command;
pub mod diff;
pub mod permissions;
pub mod prompt;
pub mod reviewers;
pub mod template;
pub mod tools;
pub use tools::ToolFilter;
pub mod types;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use maki_providers::AgentError;
use maki_providers::Message;
pub use maki_providers::{EMPTY_RESPONSE_MARKER, ImageMediaType, ImageSource, ThinkingConfig};
pub use types::{
    AgentEvent, BufferSnapshot, DoneReason, Envelope, EventSender, GrepFileEntry, GrepLine,
    GrepMatchGroup, InstructionBlock, NO_FILES_FOUND, ReviewerVerdictEvent, RunLedger, RunTotals,
    SessionEndReason, SharedBuf, SnapshotLine, SnapshotSpan, SpanColor, SpanStyle, SubagentInfo,
    TextOutput, ToolDoneEvent, ToolInput, ToolOutput, ToolStartEvent, TurnCompleteEvent,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AgentMode {
    #[default]
    Build,
    Plan(PathBuf),
}

impl AgentMode {
    pub fn plan_path(&self) -> Option<&Path> {
        match self {
            Self::Plan(p) => Some(p),
            Self::Build => None,
        }
    }
}

pub enum ExtractedCommand {
    /// Every message the user queued back to back, so one turn answers them
    /// all. A source must stop there and hand anything else over on its own,
    /// since a command like `/compact` rewrites the history the later messages
    /// land in.
    Interrupt(Vec<AgentInput>),
    /// Carries the guidance typed as `/compact <instructions>`, for this one
    /// summary.
    Compact(Option<String>),
}

pub trait InterruptSource: Send + Sync {
    fn poll(&self) -> Option<ExtractedCommand>;
}

#[derive(Clone)]
pub struct McpPromptRef {
    pub qualified_name: String,
    pub arguments: HashMap<String, String>,
}

pub struct AgentInput {
    pub message: String,
    pub mode: AgentMode,
    pub images: Vec<ImageSource>,
    pub preamble: Vec<Message>,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    /// No `Default` on this struct so adding a field forces every call site to update.
    pub workflow: bool,
    pub prompt: Option<Box<McpPromptRef>>,
}
