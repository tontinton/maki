//! Async agent loop with tools.

pub mod agent;
pub mod cancel;
pub mod child_env;
pub mod child_guard;
pub use child_guard::ChildGuard;
pub mod file_index;
pub use file_index::{
    FILE_MATCH_CONFIG, FileIndex, FileMatch, FileQuery, FileReader, Ranked, WalkEnd,
    byte_highlights, cancel_walks, file_haystack, file_haystack_owned, file_index, file_pattern,
    invalidate_for, on_walk_end, resolved_index,
};
pub mod headless;
pub mod mailbox;
pub mod mcp;
pub use mcp::config::{McpConfigError, McpConfigErrors, McpServerInfo, McpServerStatus};
pub use mcp::protocol::PromptRole;
pub use mcp::{
    McpCommand, McpHandle, McpPromptArg, McpPromptInfo, McpSession, McpSnapshot, McpSnapshotReader,
};
pub mod session;
pub(crate) mod task_set;
pub use agent::{
    Agent, AgentParams, AgentRunParams, History, HistorySnapshot, Instructions, LoadedInstructions,
    ModelSlot, RunContext, RunContextBuilder, SharedMessages, UNAVAILABLE_RESULT,
    close_dangling_tool_calls, find_subdirectory_instructions, is_instruction_file,
};
pub use cancel::{CancelMap, CancelToken, CancelTrigger};
pub use mailbox::{MailboxError, SessionMailbox};
pub use maki_config::{AgentConfig, PermissionsConfig, SessionDefaults, ToolOutputLines};
pub mod command;
pub mod diff;
pub mod permissions;
pub mod prompt;
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
    AgentEvent, BufferSnapshot, CallRecord, DoneReason, Envelope, EventSender, EventStreamGuard,
    GrepFileEntry, GrepLine, GrepMatchGroup, InstructionBlock, NO_FILES_FOUND, RunLedger,
    RunTotals, SessionEndReason, SessionEvents, SharedBuf, SnapshotLine, SnapshotSpan, SpanColor,
    SpanStyle, SteerKind, SubagentInfo, TextOutput, ToolDoneEvent, ToolInput, ToolOutput,
    ToolStartEvent, TurnCompleteEvent, UiWaker, event_stream,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputSource {
    Tui,
    Acp,
    /// `maki -p` and sdk mode.
    Headless,
    /// A plugin prompting a session of its own, `task` subagents included.
    Plugin,
}

impl InputSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Acp => "acp",
            Self::Headless => "headless",
            Self::Plugin => "plugin",
        }
    }
}

/// A message queued ahead of the one that drives a run. It stays apart from
/// the preamble so `agent.user_message` gets to judge it like any other.
pub struct EarlierInput {
    pub message: String,
    pub images: Vec<ImageSource>,
    pub preamble: Vec<Message>,
}

pub struct AgentInput {
    pub message: String,
    pub mode: AgentMode,
    pub images: Vec<ImageSource>,
    pub preamble: Vec<Message>,
    /// The rest of a burst of queued messages, oldest first.
    pub earlier: Vec<EarlierInput>,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    /// No `Default` on this struct so adding a field forces every call site to update.
    pub workflow: bool,
    pub prompt: Option<Box<McpPromptRef>>,
    pub source: InputSource,
}

impl AgentInput {
    /// What a host with no toggle UI sends. `-p`, the SDK and ACP know nothing
    /// about the toggles beyond what config says, so they all build their input
    /// here and a knob added to [`SessionDefaults`] reaches every one of them.
    pub fn from_defaults(
        message: String,
        mode: AgentMode,
        images: Vec<ImageSource>,
        defaults: SessionDefaults,
        source: InputSource,
    ) -> Self {
        Self {
            message,
            mode,
            images,
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking: defaults.thinking.into(),
            fast: defaults.fast,
            workflow: defaults.workflow,
            prompt: None,
            source,
        }
    }
}
