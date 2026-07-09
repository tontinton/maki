mod compaction;
mod history;
mod instructions;
mod run;
mod streaming;
pub mod tool_dispatch;

pub use compaction::{branch_summary, compact};
pub use history::{CutPoint, History, SharedContext, ValidContext, finalize::FinalizedPartial};
pub use instructions::{
    Instructions, LoadedInstructions, build_system_prompt, find_subdirectory_instructions,
    is_instruction_file, load_instruction_text, load_instructions,
};
pub use run::{Agent, AgentParams, AgentRunParams, resolve_compaction_model};
