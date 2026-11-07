mod compaction;
mod display_order;
mod frame;
mod history;
pub mod hook;
mod instructions;
mod run;
mod streaming;
pub mod tool_dispatch;
mod transcript;

pub use compaction::compact;
pub use display_order::{DisplayItem, display_order, observation_detail, observation_line};
pub use frame::{HostPrompt, RunContext, RunContextBuilder, plan_mode_update};
pub use history::{
    Cutter, History, HistorySnapshot, SharedHeader, SharedMessages, UNAVAILABLE_RESULT,
    USER_CANCEL_MARKER, close_dangling_tool_calls, live_header, live_history, live_snapshot,
    publish_live_history, publish_live_session, unbind_thinking,
};
pub use hook::{AgentCall, AgentHook, AgentHooks, AgentSlot};
pub use instructions::{
    CallInstructions, Instructions, LoadedInstructions, build_system_prompt,
    default_instruction_files, find_subdirectory_instructions, is_instruction_file,
    load_instruction_text, load_instructions,
};
#[cfg(test)]
pub(crate) use run::testing;
pub use run::{Agent, AgentParams, AgentRunParams, ModelSlot, resolve_compaction_model};
pub use transcript::{NEWER_FORMAT, SessionExport, Transcript, TranscriptError};
