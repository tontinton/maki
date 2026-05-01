use clap::{Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::bail;

use maki_agent::tools::{all_builtin_tool_names, is_builtin_tool};

use crate::print::OutputFormat;

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
    #[arg(short = 's', long)]
    pub session: Option<String>,

    #[arg(long)]
    #[cfg(feature = "demo")]
    pub demo: bool,

    /// Output format for --print mode
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub output_format: OutputFormat,

    /// Skip loading skill files from .maki/skills, .claude/skills, etc.
    #[arg(long)]
    pub no_skills: bool,

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
    #[arg(long)]
    pub yolo: bool,

    /// Exit after the agent completes (for automation workflows)
    #[arg(long)]
    pub exit_on_done: bool,

    /// Pre-approve tools (comma-separated). Accepts PascalCase (Claude Code) or snake_case.
    #[arg(long, value_delimiter = ',')]
    pub allowed_tools: Vec<String>,

    /// Initial prompt (reads stdin if piped)
    pub prompt: Option<String>,
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
    fn normalize_tool_name_valid_inputs(input: &str, expected: &str) {
        assert_eq!(normalize_tool_name(input).unwrap(), expected);
    }

    #[test]
    fn normalize_tool_name_rejects_unknown() {
        let result = normalize_tool_name("NonExistentTool");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown tool"));
        assert!(err.contains("read"));
    }
}
