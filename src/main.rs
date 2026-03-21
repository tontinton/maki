mod print;

use std::env;
use std::path::Path;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use color_eyre::Result;
use color_eyre::eyre::Context;
use maki_agent::skill::{self, Skill};
use maki_config::load_config;
use maki_storage::DataDir;
use maki_ui::AppSession;
use tracing_subscriber::EnvFilter;

use maki_providers::model::{DEFAULT_SPEC, Model};
use maki_providers::provider::fetch_all_models;
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

    /// Disable rtk command rewriting
    #[arg(long)]
    no_rtk: bool,

    /// Skip all permission prompts (allow everything)
    #[arg(long)]
    yolo: bool,

    /// Initial prompt (reads stdin if omitted in --print mode)
    prompt: Option<String>,
}

fn discover(disable: bool) -> Vec<Skill> {
    if disable {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    skill::discover_skills(&cwd)
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
    Index {
        path: String,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Authenticate with a provider
    Login {
        /// Provider slug (e.g. anthropic-oauth)
        provider: String,
    },
    /// Remove stored credentials for a provider
    Logout {
        /// Provider slug (e.g. anthropic-oauth)
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
        None => {
            let storage = DataDir::resolve().context("resolve data directory")?;
            let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
            let mut config = load_config(&cwd, cli.no_rtk);
            if cli.yolo {
                config.permissions.allow_all = true;
            }
            config.validate()?;
            let model = resolve_model(cli.model.as_deref(), &config.provider, &storage)?;
            init_logging(&storage, &config.storage);
            install_panic_log_hook();
            let skills = discover(cli.no_skills);
            if cli.print {
                print::run(
                    &model,
                    cli.prompt,
                    cli.output_format,
                    cli.verbose,
                    skills,
                    config.agent,
                    config.permissions,
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
                let session_id = maki_ui::run(maki_ui::EventLoopParams {
                    model,
                    skills,
                    session,
                    storage,
                    config: config.agent,
                    ui_config: config.ui,
                    input_history_size: config.storage.input_history_size,
                    permissions: Arc::new(maki_agent::permissions::PermissionManager::new(
                        config.permissions,
                        cwd.clone(),
                    )),
                    #[cfg(feature = "demo")]
                    demo: cli.demo,
                })
                .context("run UI")?;
                eprintln!("session: {session_id}");
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
    let default = provider_config
        .default_model
        .as_deref()
        .unwrap_or(DEFAULT_SPEC);
    Ok(Model::from_spec(default).expect("default model spec is always valid"))
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
