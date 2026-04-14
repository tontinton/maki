mod print;
mod update;

use std::env;
use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::{Context, bail};
use maki_agent::ToolCall;
use maki_agent::command::{self, CustomCommand};
use maki_agent::mcp::{config as mcp_config, oauth as mcp_oauth};
use maki_agent::skill::{self, Skill};
use maki_config::load_config;
use maki_storage::DataDir;
use maki_ui::AppSession;
use tracing_subscriber::EnvFilter;

use maki_providers::model::{Model, ModelTier};
use maki_providers::provider::{ProviderKind, fetch_all_models};
use maki_providers::{dynamic, openai_auth};
use maki_storage::log::RotatingFileWriter;
use maki_storage::model::{persist_model, read_model};
use print::OutputFormat;

#[derive(Parser)]
#[command(name = "maki", version, about = "AI coding agent for the terminal")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Non-interactive mode. Runs the prompt and exits. Compatible with Claude Code's --print flag
    #[arg(short, long)]
    print: bool,

    /// Model spec (provider/model-id). Defaults to last used model, or claude-opus-4-6
    #[arg(short, long)]
    model: Option<String>,

    /// Include full turn-by-turn messages in --print output
    #[arg(long)]
    verbose: bool,

    /// Resume the most recent session in this directory
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// Resume a specific session by its ID
    #[arg(short = 's', long)]
    session: Option<String>,

    #[arg(long)]
    #[cfg(feature = "demo")]
    demo: bool,

    /// Output format for --print mode
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,

    /// Skip loading skill files from .maki/skills, .claude/skills, etc.
    #[arg(long)]
    no_skills: bool,

    /// Skip loading custom commands from .maki/commands, .claude/commands, etc.
    #[arg(long)]
    no_commands: bool,

    /// Disable rtk command rewriting
    #[arg(long)]
    no_rtk: bool,

    /// Skip all permission prompts (allow everything)
    #[arg(long)]
    yolo: bool,

    /// Exit after the agent completes (for automation workflows)
    #[arg(long)]
    exit_on_done: bool,

    /// Pre-approve tools (comma-separated). Accepts PascalCase (Claude Code) or snake_case.
    #[arg(long, value_delimiter = ',')]
    allowed_tools: Vec<String>,

    /// Initial prompt (reads stdin if piped)
    prompt: Option<String>,
}

fn normalize_tool_name(name: &str) -> Result<String> {
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
    if ToolCall::static_name(&result).is_none() {
        bail!(
            "unknown tool '{}'. Valid tools: {}",
            name,
            ToolCall::all_names().join(", ")
        );
    }
    Ok(result)
}

fn discover(disable: bool) -> Vec<Skill> {
    if disable {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    skill::discover_skills(&cwd)
}

fn discover_cmds(disable: bool) -> Vec<CustomCommand> {
    if disable {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    command::discover_commands(&cwd)
}

#[derive(Subcommand)]
enum Command {
    /// Manage API authentication
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List all available models
    Models,
    /// Run the index tool on a file to see how it looks like
    Index { path: String },
    /// Run the find_symbol tool
    FindSymbol {
        /// Symbol name to search for
        symbol: String,
        /// Path to the file containing the symbol
        file: String,
        /// Line number (1-indexed)
        line: usize,
    },
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
enum McpAction {
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
enum AuthAction {
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

fn main() {
    color_eyre::install().ok();
    if let Err(e) = run() {
        print_error(&e);
        std::process::exit(1);
    }
}

fn print_error(e: &color_eyre::Report) {
    const RED: &str = "\x1b[31m";
    const BOLD_RED: &str = "\x1b[1;31m";
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";

    eprintln!();
    eprintln!("{BOLD_RED}✖ {e}{RESET}");
    let causes: Vec<_> = e.chain().skip(1).collect();
    let last = causes.len().saturating_sub(1);
    for (i, cause) in causes.iter().enumerate() {
        let branch = if i == last { "└─" } else { "├─" };
        eprintln!("{DIM}{branch}{RESET} {RED}{cause}{RESET}");
    }
    eprintln!();
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Auth { action }) => {
            let storage = DataDir::resolve().context("resolve data directory")?;
            match action {
                AuthAction::Login { provider } => match provider.as_str() {
                    "openai" => openai_auth::login(&storage)?,
                    slug => dynamic::login(slug)?,
                },
                AuthAction::Logout { provider } => match provider.as_str() {
                    "openai" => openai_auth::logout(&storage)?,
                    slug => dynamic::logout(slug)?,
                },
            }
        }
        Some(Command::Index { path }) => {
            let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
            let config = load_config(&cwd, false);
            let output =
                maki_code_index::index_file(Path::new(&path), config.agent.index_max_file_size)
                    .context("index file")?;
            print!("{output}");
        }
        Some(Command::FindSymbol { symbol, file, line }) => {
            let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
            let result = maki_code_index::find_symbol::find_symbol(
                &cwd,
                Path::new(&file),
                line,
                &symbol,
                1,
                None,
            )
            .context("find_symbol")?;
            println!(
                "Scope: {}\n{} references ({} files searched, {} parsed)",
                result.scope,
                result.references.len(),
                result.stats.files_grepped,
                result.stats.files_parsed
            );
            for r in &result.references {
                println!("{}", r.format_relative(&cwd));
            }
        }
        Some(Command::Models) => {
            smol::block_on(fetch_all_models(|batch| {
                for model in batch.models {
                    println!("{model}");
                }
                for warning in batch.warnings {
                    eprintln!("warning: {warning}");
                }
            }));
        }
        Some(Command::Mcp { action }) => {
            let storage = DataDir::resolve().context("resolve data directory")?;
            match action {
                McpAction::Auth { server } => {
                    smol::block_on(async {
                        let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
                        let config = mcp_config::load_config(&cwd);
                        let raw = config.mcp.get(&server).ok_or_else(|| {
                            color_eyre::eyre::eyre!("unknown MCP server: {server}")
                        })?;
                        let url = match mcp_config::parse_server(server.clone(), raw.clone())?
                            .transport
                        {
                            mcp_config::Transport::Http { url, .. } => url,
                            _ => color_eyre::eyre::bail!(
                                "server '{server}' is not an HTTP transport"
                            ),
                        };
                        mcp_oauth::authenticate(&server, &url, None, &storage).await?;
                        eprintln!("Successfully authenticated with MCP server '{server}'");
                        Ok(())
                    })?;
                }
                McpAction::Logout { server } => {
                    let deleted = maki_storage::auth::delete_mcp_auth(&storage, &server)?;
                    if deleted {
                        eprintln!("Removed OAuth credentials for MCP server '{server}'");
                    } else {
                        eprintln!("No stored credentials for MCP server '{server}'");
                    }
                }
            }
        }
        Some(Command::Update { yes, no_color }) => {
            update::update(yes, no_color).map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        Some(Command::Rollback) => {
            update::rollback().map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        None => {
            let storage = DataDir::resolve().context("resolve data directory")?;
            maki_providers::tier_map::load_from_storage(&storage);
            let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
            let mut config = load_config(&cwd, cli.no_rtk);
            let timeouts = maki_providers::Timeouts {
                connect: config.provider.connect_timeout,
                low_speed: config.provider.low_speed_timeout,
                stream: config.provider.stream_timeout,
            };
            if cli.yolo || config.always_yolo {
                config.permissions.allow_all = true;
            }
            if !cli.allowed_tools.is_empty() {
                config.agent.allowed_tools = cli
                    .allowed_tools
                    .iter()
                    .map(|t| normalize_tool_name(t))
                    .collect::<Result<Vec<_>>>()?;
            }
            config.validate()?;
            let model = resolve_model(cli.model.as_deref(), &config.provider, &storage)?;
            init_logging(&storage, &config.storage);
            install_panic_log_hook();
            let skills = discover(cli.no_skills);
            let commands = discover_cmds(cli.no_commands);
            if cli.print {
                print::run(
                    &model,
                    cli.prompt,
                    cli.output_format,
                    cli.verbose,
                    skills,
                    config.agent,
                    config.permissions,
                    timeouts,
                )
                .context("run print mode")?;
            } else {
                let cwd_str = cwd.to_string_lossy().into_owned();
                let session = resolve_session(
                    cli.continue_session,
                    cli.session,
                    &model.spec(),
                    &cwd_str,
                    &storage,
                )?;
                let model = if session.messages.is_empty() {
                    model
                } else {
                    Model::from_spec(&session.model).unwrap_or(model)
                };
                let initial_prompt = match cli.prompt {
                    Some(p) => Some(p),
                    None if !io::stdin().is_terminal() => {
                        let mut buf = String::new();
                        io::stdin().read_to_string(&mut buf).context("read stdin")?;
                        Some(buf)
                    }
                    None => None,
                };
                let (session_id, exit_code) = maki_ui::run(
                    maki_ui::EventLoopParams {
                        model,
                        skills,
                        commands,
                        session,
                        storage,
                        config: config.agent,
                        ui_config: config.ui,
                        input_history_size: config.storage.input_history_size,
                        permissions: Arc::new(maki_agent::permissions::PermissionManager::new(
                            config.permissions,
                            cwd.clone(),
                        )),
                        timeouts,
                        exit_on_done: cli.exit_on_done,
                        #[cfg(feature = "demo")]
                        demo: cli.demo,
                    },
                    initial_prompt,
                )
                .context("run UI")?;
                if let Some(session_id) = session_id {
                    eprintln!("session: {session_id}");
                }
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
            }
        }
    }
    Ok(())
}

fn resolve_session(
    continue_session: bool,
    session_id: Option<String>,
    model: &str,
    cwd: &str,
    storage: &DataDir,
) -> Result<AppSession> {
    if let Some(id) = session_id {
        return AppSession::load(&id, storage).map_err(|e| color_eyre::eyre::eyre!("{e}"));
    }
    if continue_session {
        match AppSession::latest(cwd, storage) {
            Ok(Some(session)) => return Ok(session),
            Ok(None) => {
                tracing::info!("no previous session found for this directory, starting new");
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to load latest session, starting new");
            }
        }
    }
    Ok(AppSession::new(model, cwd))
}

fn resolve_model(
    explicit: Option<&str>,
    provider_config: &maki_config::ProviderConfig,
    storage: &DataDir,
) -> Result<Model> {
    if let Some(spec) = explicit {
        let model = Model::from_spec(spec).context("invalid --model spec")?;
        persist_model(storage, &model.spec());
        return Ok(model);
    }
    if let Some(spec) = read_model(storage) {
        if let Ok(m) = Model::from_spec(&spec) {
            return Ok(m);
        }
        tracing::warn!(spec, "saved model no longer valid, falling back to default");
    }
    if let Some(spec) = provider_config.default_model.as_deref() {
        return Model::from_spec(spec).context("invalid default_model in config");
    }
    auto_detect_model().ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "no provider available - set an API key (e.g. ANTHROPIC_API_KEY) or run `maki auth login`\n\nSee https://maki.sh/docs/providers/ for setup instructions"
        )
    })
}

const PROVIDER_PRIORITY: &[ProviderKind] = &[
    ProviderKind::Anthropic,
    ProviderKind::OpenAi,
    ProviderKind::Zai,
    ProviderKind::ZaiCodingPlan,
];

fn auto_detect_model() -> Option<Model> {
    for tier in [ModelTier::Strong, ModelTier::Medium] {
        for &provider in PROVIDER_PRIORITY {
            if provider.is_available()
                && let Ok(model) = Model::from_tier(provider, tier)
            {
                return Some(model);
            }
        }
    }
    None
}

fn install_panic_log_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".into()
        };
        let location = info.location().map(|l| l.to_string());
        tracing::error!(
            panic.payload = %payload,
            panic.location = location.as_deref().unwrap_or("<unknown>"),
            "panic occurred"
        );
        prev(info);
    }));
}

fn init_logging(storage: &DataDir, storage_config: &maki_config::StorageConfig) {
    let Ok(writer) = RotatingFileWriter::new(
        storage,
        storage_config.max_log_bytes,
        storage_config.max_log_files,
    ) else {
        return;
    };
    let writer = Mutex::new(writer);
    let filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(writer)
        .init();
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
        assert!(err.contains("bash"));
    }
}
