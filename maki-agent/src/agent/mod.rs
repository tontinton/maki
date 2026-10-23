mod compaction;
mod frame;
mod history;
pub mod hook;
mod instructions;
mod run;
mod streaming;
pub mod tool_dispatch;

pub use compaction::compact;
pub use frame::{RunContext, RunContextBuilder, plan_mode_update};
pub use history::{
    History, HistorySnapshot, SharedMessages, UNAVAILABLE_RESULT, close_dangling_tool_calls,
    live_history, publish_live_history,
};
pub use hook::{AgentCall, AgentHook, AgentHooks, AgentSlot};
pub use instructions::{
    CallInstructions, Instructions, LoadedInstructions, build_system_prompt,
    default_instruction_files, find_subdirectory_instructions, is_instruction_file,
    load_instruction_text, load_instructions,
};
pub use run::{Agent, AgentParams, AgentRunParams, ModelSlot, resolve_compaction_model};
