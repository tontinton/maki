use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::Result;
use color_eyre::eyre::bail;

use maki_agent::tools::{all_builtin_tool_names, is_builtin_tool};

use crate::print::OutputFormat;

#[derive(Clone, ValueEnum, Default)]
pub enum InputFormat {
    #[default]
    Text,
    StreamJson,
}

#[derive(Parser)]
#[command(name = "maki", version, about = "AI coding agent for the terminal")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Non-interactive mode. Runs the prompt and exits. Compatible with Claude Code's --print flag
    #[arg(short, long)]
    pub print: bool,

    /// Model spec (provider/model-id). Defaults to last used model, or claude-opus-4-6
    #[arg(short, long)]
    pub model: Option<String>,

    /// Include full turn-by-turn messages in --print output
    #[arg(long)]
    pub verbose: bool,

    /// Resume the most recent session in this directory
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a specific session by its ID
    #[arg(short = 's', long, alias = "resume")]
    pub session: Option<String>,

    /// Output format for --print mode
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output_format: OutputFormat,

    /// Input format (text or stream-json for SDK mode)
    #[arg(long, value_enum, default_value_t = InputFormat::Text)]
    pub input_format: InputFormat,

    /// Skip loading custom commands from .maki/commands, .claude/commands, etc.
    #[arg(long)]
    pub no_commands: bool,

    /// Disable rtk command rewriting
    #[arg(long)]
    pub no_rtk: bool,

    /// Disable the Lua plugin system
    #[arg(long)]
    pub no_plugins: bool,

    /// Skip all permission prompts (allow everything)
    #[arg(long, alias = "dangerously-skip-permissions")]
    pub yolo: bool,

    /// Exit after the agent completes (for automation workflows)
    #[arg(long)]
    pub exit_on_done: bool,

    /// Pre-approve tools (comma-separated). Accepts PascalCase (Claude Code) or snake_case.
    #[arg(long, value_delimiter = ',', visible_alias = "allowedTools")]
    pub allowed_tools: Vec<String>,

    /// Disallowed tools (comma-separated).
    #[arg(long, value_delimiter = ',', visible_alias = "disallowedTools")]
    pub disallowed_tools: Vec<String>,

    /// Session ID for SDK mode
    #[arg(long)]
    pub session_id: Option<String>,

    /// Fork the loaded session under a new ID
    #[arg(long)]
    pub fork_session: bool,

    /// Maximum number of agent turns
    #[arg(long)]
    pub max_turns: Option<u32>,

    /// System prompt override
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// Append to system prompt
    #[arg(long)]
    pub append_system_prompt: Option<String>,

    /// Permission mode for SDK
    #[arg(long)]
    pub permission_mode: Option<String>,

    /// Include partial streaming messages in SDK output
    #[arg(long)]
    pub include_partial_messages: bool,

    /// Permission prompt tool (accepted for compat, used in SDK mode)
    #[arg(long, hide = true)]
    pub permission_prompt_tool: Option<String>,

    // Accepted but ignored, so Claude Code SDK callers don't break.
    #[arg(long, hide = true)]
    pub fallback_model: Option<String>,
    #[arg(long, hide = true)]
    pub settings: Option<String>,
    #[arg(long, hide = true)]
    pub setting_sources: Option<String>,
    #[arg(long, hide = true)]
    pub add_dir: Option<String>,
    #[arg(long, hide = true)]
    pub strict_mcp_config: bool,
    #[arg(long, hide = true)]
    pub include_hook_events: bool,
    #[arg(long, hide = true)]
    pub mcp_config: Option<String>,
    #[arg(long, hide = true)]
    pub tools: Option<String>,
    #[arg(long, hide = true)]
    pub betas: Option<String>,
    #[arg(long, hide = true)]
    pub max_thinking_tokens: Option<String>,
    #[arg(long, hide = true)]
    pub effort: Option<String>,
    #[arg(long, hide = true)]
    pub json_schema: Option<String>,
    #[arg(long, hide = true)]
    pub max_budget_usd: Option<String>,
    #[arg(long, hide = true)]
    pub thinking: Option<String>,
    #[arg(long, hide = true)]
    pub thinking_display: Option<String>,

    /// Initial prompt (reads stdin if piped)
    pub prompt: Option<String>,
}

impl Cli {
    pub fn warn_ignored_flags(&self) {
        let ignored = [
            ("fallback-model", self.fallback_model.is_some()),
            ("settings", self.settings.is_some()),
            ("setting-sources", self.setting_sources.is_some()),
            ("add-dir", self.add_dir.is_some()),
            ("strict-mcp-config", self.strict_mcp_config),
            ("include-hook-events", self.include_hook_events),
            ("mcp-config", self.mcp_config.is_some()),
            ("tools", self.tools.is_some()),
            ("betas", self.betas.is_some()),
            ("max-thinking-tokens", self.max_thinking_tokens.is_some()),
            ("effort", self.effort.is_some()),
            ("json-schema", self.json_schema.is_some()),
            ("max-budget-usd", self.max_budget_usd.is_some()),
            ("thinking", self.thinking.is_some()),
            ("thinking-display", self.thinking_display.is_some()),
        ];
        for (flag, set) in &ignored {
            if *set {
                eprintln!("warning: --{flag} is accepted but ignored");
            }
        }
    }

    pub fn is_sdk_mode(&self) -> bool {
        self.print && matches!(self.input_format, InputFormat::StreamJson)
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Manage API authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List all available models
    Models,
    /// Run the index tool on a file to see how it looks like
    Index { path: String },
    /// Manage MCP server authentication
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Update maki to the latest version
    Update {
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        /// Disable syntax highlighting
        #[arg(long)]
        no_color: bool,
    },
    /// Rollback to the previous version
    Rollback,
    /// Run as an ACP (Agent Client Protocol) server over stdio
    Acp {
        /// Model spec (provider/model-id)
        #[arg(short, long)]
        model: Option<String>,
        /// Skip all permission prompts
        #[arg(long)]
        yolo: bool,
    },
    /// Data migration utilities
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
}

#[derive(Subcommand)]
pub enum MigrateAction {
    /// Migrate files from ~/.maki/ to XDG directories
    Xdg,
}

#[derive(Subcommand)]
pub enum McpAction {
    /// Authenticate with an MCP server
    Auth {
        /// Server name from config
        server: String,
    },
    /// Remove stored OAuth credentials for an MCP server
    Logout {
        /// Server name from config
        server: String,
    },
}

#[derive(Subcommand)]
pub enum AuthAction {
    /// Authenticate with a provider
    Login {
        /// Provider slug (e.g. openai)
        provider: String,
    },
    /// Remove stored credentials for a provider
    Logout {
        /// Provider slug (e.g. openai)
        provider: String,
    },
}

pub fn normalize_tool_name(name: &str) -> Result<String> {
    let mut result = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    if !is_builtin_tool(&result) {
        bail!(
            "unknown tool '{}'. Valid tools: {}",
            name,
            all_builtin_tool_names().join(", ")
        );
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("Read", "read")]
    #[test_case("Bash", "bash")]
    #[test_case("CodeExecution", "code_execution")]
    #[test_case("code_execution", "code_execution"; "snake_passthrough")]
    #[test_case("TodoWrite", "todo_write")]
    #[test_case("todo_write", "todo_write"; "todo_write_already_snake_case")]
    fn normalize_tool_name_valid_inputs(input: &str, expected: &str) {
        assert_eq!(normalize_tool_name(input).unwrap(), expected);
    }

    #[test]
    fn normalize_tool_name_rejects_unknown() {
        let result = normalize_tool_name("NonExistentTool");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    #[test]
    fn normalize_tool_name_multi_edit_rejects_snake_variant() {
        assert!(normalize_tool_name("MultiEdit").is_err());
    }
}
