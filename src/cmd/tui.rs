use std::env;
use std::io::{self, IsTerminal, Read};
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_agent::command::{self, CustomCommand};
use maki_agent::skill::{self, Skill};
use maki_agent::tools::ToolRegistry;
use maki_config::{load_env_files, load_permissions};
use maki_lua::PluginHost;
use maki_providers::model::Model;
use maki_storage::StateDir;
use maki_ui::AppSession;

use crate::cli::{Cli, normalize_tool_name};
use crate::setup;

fn discover_skills(disable: bool) -> Vec<Skill> {
    if disable {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    skill::discover_skills(&cwd)
}

fn discover_commands(disable: bool) -> Vec<CustomCommand> {
    if disable {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    command::discover_commands(&cwd)
}

fn resolve_session(
    continue_session: bool,
    session_id: Option<String>,
    model: &str,
    cwd: &str,
    storage: &StateDir,
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

fn read_initial_prompt(cli_prompt: Option<String>) -> Result<Option<String>> {
    match cli_prompt {
        Some(p) => Ok(Some(p)),
        None if !io::stdin().is_terminal() => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).context("read stdin")?;
            Ok(Some(buf))
        }
        None => Ok(None),
    }
}

pub fn run(cli: Cli) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    maki_providers::tier_map::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());

    load_env_files(&cwd);
    warn_stale_config_toml(&cwd);

    let mut plugin_host = if cli.no_plugins {
        PluginHost::disabled()
    } else {
        PluginHost::new(Arc::clone(ToolRegistry::native_arc()))
            .context("initialize lua plugin host")?
    };

    let raw_config = plugin_host
        .load_init_files(&cwd)
        .context("load init.lua files")?;

    let mut config = raw_config.unwrap_or_default().into_config(cli.no_rtk);
    config.permissions = load_permissions(&cwd);

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

    plugin_host
        .load_builtins(&config.plugins)
        .context("load builtin plugins")?;

    let timeouts = maki_providers::Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };

    let model = setup::resolve_model(cli.model.as_deref(), &config.provider, &storage)?;

    setup::init_logging(&storage, &config.storage);
    setup::install_panic_log_hook();

    let skills = discover_skills(cli.no_skills);
    let commands = discover_commands(cli.no_commands);

    if cli.print {
        crate::print::run(
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
        let initial_prompt = read_initial_prompt(cli.prompt)?;
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
                plugin_render_hints: plugin_host.drain_render_hints(),
                buf_click: plugin_host.event_handle().map(|eh| {
                    Arc::new(move |tool_id: &str, row: u32| {
                        eh.fire_click(tool_id, row);
                    }) as maki_ui::BufClickHandler
                }),
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
    Ok(())
}

fn warn_stale_config_toml(cwd: &std::path::Path) {
    let stale_paths = [
        maki_config::global_config_dir().map(|d| d.join("config.toml")),
        Some(cwd.join(".maki/config.toml")),
    ];
    for path in stale_paths.into_iter().flatten() {
        if path.is_file() {
            tracing::warn!(
                path = %path.display(),
                "config.toml found but no longer used. Migrate to init.lua. See https://maki.sh/docs/configuration/"
            );
        }
    }
}
