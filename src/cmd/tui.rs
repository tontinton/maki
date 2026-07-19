use std::env;
use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_agent::command::{self, CustomCommand};
use maki_agent::tools::ToolRegistry;
use maki_config::{Config, load_env_files, load_permissions};
use maki_lua::PluginHost;
use maki_providers::model::Model;
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_ui::{AppSession, RunOutcome};

use crate::cli::{Cli, normalize_tool_name};
use crate::setup;

const FALLBACK_MODEL_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
const CONFIG_FALLBACK_WARNING: &str = "config reload failed, using previous config";
const MODEL_FALLBACK_WARNING: &str = "model resolution failed, keeping previous model";

/// One generation of the app: everything torn down and rebuilt on `/reload`.
/// Dropping it joins the Lua thread via `PluginHost::drop`.
struct Stack {
    plugin_host: PluginHost,
    config: Config,
    commands: Vec<CustomCommand>,
    model: Model,
    needs_login: bool,
}

impl Stack {
    fn timeouts(&self) -> maki_providers::Timeouts {
        maki_providers::Timeouts {
            connect: self.config.provider.connect_timeout,
            low_speed: self.config.provider.low_speed_timeout,
            stream: self.config.provider.stream_timeout,
        }
    }
}

/// Background teardown of the previous generation. `defer` keeps the slow
/// drop (a Lua thread join, capped at 2s in `PluginHost::drop`) off the
/// `/reload` hot path. Joining on replace and on drop covers every exit
/// path, including `?` unwinds, so no VM is abandoned mid-shutdown and at
/// most one teardown is ever in flight.
#[derive(Default)]
struct Teardown(Option<JoinHandle<()>>);

impl Teardown {
    fn defer(&mut self, work: impl FnOnce() + Send + 'static) {
        self.join();
        self.0 = Some(thread::spawn(work));
    }

    fn join(&mut self) {
        if let Some(handle) = self.0.take()
            && handle.join().is_err()
        {
            tracing::warn!("background teardown panicked");
        }
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        self.join();
    }
}

fn discover_commands(disable: bool) -> Vec<CustomCommand> {
    if disable {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    command::discover_commands(&cwd)
}

fn load_config(plugin_host: &PluginHost, cli: &Cli, cwd: &Path) -> Result<Config> {
    let raw_config = plugin_host
        .load_init_files(cwd)
        .context("load init.lua files")?;

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(cli.no_rtk)
        .context("invalid config")?;
    config.permissions = load_permissions(cwd);

    if cli.yolo || config.always_yolo {
        config.permissions.yolo = true;
    }
    if !cli.allowed_tools.is_empty() {
        config.agent.allowed_tools = cli
            .allowed_tools
            .iter()
            .map(|t| normalize_tool_name(t))
            .collect::<Result<Vec<_>>>()?;
    }
    if !cli.disallowed_tools.is_empty() {
        config.agent.disabled_tools.extend(
            cli.disallowed_tools
                .iter()
                .filter_map(|t| normalize_tool_name(t).ok()),
        );
    }
    config.validate()?;
    Ok(config)
}

fn config_or_fallback(
    loaded: Result<Config>,
    fallback: Option<Config>,
    warnings: &mut Vec<String>,
) -> Result<Config> {
    match (loaded, fallback) {
        (Ok(config), _) => Ok(config),
        (Err(e), Some(last_good)) => {
            warnings.push(format!("{CONFIG_FALLBACK_WARNING}: {e:#}"));
            Ok(last_good)
        }
        (Err(e), None) => Err(e),
    }
}

/// The one construction path for a generation: first startup passes
/// `fallback: None` (fail-fast); `/reload` passes the last-good config and
/// model so a broken config reopens the UI with a warning instead of exiting.
fn build_stack(
    cli: &Cli,
    cwd: &Path,
    storage: &StateDir,
    fallback: Option<(Config, Model)>,
) -> Result<(Stack, Vec<String>)> {
    let mut warnings = Vec::new();

    let mut plugin_host = if cli.no_plugins {
        PluginHost::disabled()
    } else {
        PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !cli.no_jit)
            .context("initialize lua plugin host")?
    };

    let (fallback_config, fallback_model) = fallback.unzip();
    let reloading = fallback_model.is_some();
    let config = config_or_fallback(
        load_config(&plugin_host, cli, cwd),
        fallback_config,
        &mut warnings,
    )?;
    warnings.extend(config.migration_warnings.clone());

    if let Err(e) = plugin_host.load_builtins(&config.plugins) {
        let e = color_eyre::eyre::Report::from(e).wrap_err("load builtin plugins");
        if reloading {
            warnings.push(format!("{e:#}"));
        } else {
            return Err(e);
        }
    }

    let commands = discover_commands(cli.no_commands);

    let model_result = setup::resolve_model(cli.model.as_deref(), &config.provider, storage);
    let (model, needs_login) = match (model_result, fallback_model) {
        (Ok(m), _) => (m, false),
        (Err(e), Some(last_model)) => {
            warnings.push(format!("{MODEL_FALLBACK_WARNING}: {e:#}"));
            (last_model, false)
        }
        (Err(_), None) if !cli.print => {
            let placeholder = Model::from_spec(FALLBACK_MODEL_SPEC).expect("fallback model");
            (placeholder, true)
        }
        (Err(e), None) => return Err(e),
    };

    Ok((
        Stack {
            plugin_host,
            config,
            commands,
            model,
            needs_login,
        },
        warnings,
    ))
}

fn resolve_session(
    continue_session: bool,
    session_id: Option<&str>,
    model: &str,
    cwd: &str,
    storage: &StateDir,
) -> Result<AppSession> {
    if let Some(raw) = session_id {
        let id: MakiId = raw
            .parse()
            .map_err(|e| color_eyre::eyre::eyre!("invalid session id {raw:?}: {e}"))?;
        return AppSession::load(id, storage).map_err(|e| color_eyre::eyre::eyre!("{e}"));
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

pub fn run(mut cli: Cli) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    maki_providers::model_registry::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());

    load_env_files(&cwd);
    warn_stale_config_toml(&cwd);

    let (mut stack, _) = build_stack(&cli, &cwd, &storage, None)?;

    setup::init_logging(&stack.config.storage);
    setup::install_panic_log_hook();

    if cli.is_sdk_mode() {
        let fast = stack.config.always_fast && stack.model.supports_fast();
        let prompt_slots = stack
            .plugin_host
            .event_handle()
            .map(|h| h.collect_prompt_slots())
            .unwrap_or_default();
        let timeouts = stack.timeouts();
        crate::sdk_mode::run(crate::sdk_mode::SdkParams {
            cli,
            model: stack.model,
            config: stack.config.agent,
            permissions_config: stack.config.permissions,
            timeouts,
            prompt_slots,
            fast,
            workflow: stack.config.always_workflow,
        })
        .context("run sdk mode")?;
        return Ok(());
    }
    if cli.print {
        let fast = stack.config.always_fast && stack.model.supports_fast();
        let timeouts = stack.timeouts();
        crate::print::run(
            &stack.model,
            cli.initial_prompt,
            cli.images,
            cli.output_format,
            cli.verbose,
            stack.config.agent,
            stack.config.permissions,
            timeouts,
            stack.plugin_host.event_handle(),
            fast,
            stack.config.always_workflow,
        )
        .context("run print mode")?;
        return Ok(());
    }

    let cwd_str = cwd.to_string_lossy().into_owned();
    let mut tabs = vec![resolve_session(
        cli.continue_session,
        cli.session.as_deref(),
        &stack.model.spec(),
        &cwd_str,
        &storage,
    )?];
    let mut focused = 0;
    let mut warnings: Vec<String> = Vec::new();
    let mut initial_prompt = read_initial_prompt(cli.initial_prompt.take())?;
    let mut teardown = Teardown::default();

    loop {
        for session in &mut tabs {
            if session.messages.is_empty() {
                session.meta.fast |= stack.config.always_fast;
                session.meta.workflow |= stack.config.always_workflow;
                if let Some(thinking) = stack.config.always_thinking {
                    session.meta.thinking = Some(thinking);
                }
            }
        }
        let focused_tab = &tabs[focused];
        let model = if focused_tab.messages.is_empty() {
            stack.model.clone()
        } else {
            Model::from_spec(&focused_tab.model).unwrap_or_else(|_| stack.model.clone())
        };

        let outcome = maki_ui::run(
            maki_ui::EventLoopParams {
                model,
                needs_login: stack.needs_login,
                commands: std::mem::take(&mut stack.commands),
                sessions: std::mem::take(&mut tabs),
                focused,
                startup_warnings: std::mem::take(&mut warnings),
                storage: storage.clone(),
                config: stack.config.agent.clone(),
                ui_config: stack.config.ui,
                input_history_size: stack.config.storage.input_history_size,
                permissions: Arc::new(maki_agent::permissions::PermissionManager::new(
                    stack.config.permissions.clone(),
                    cwd.clone(),
                )),
                timeouts: stack.timeouts(),
                exit_on_done: cli.exit_on_done,
                lua_command_reader: stack.plugin_host.command_reader(),
                keymap_reader: stack.plugin_host.keymap_reader(),
                hint_reader: stack.plugin_host.hint_reader(),
                ui_action_rx: stack.plugin_host.ui_action_rx(),
                lua_event_handle: stack.plugin_host.event_handle(),
            },
            initial_prompt.take(),
        )
        .context("run UI")?;

        match outcome {
            RunOutcome::Exit { session_id, code } => {
                if let Some(session_id) = session_id {
                    eprintln!("Resume session:\n\n  maki -s {session_id}");
                }
                if code != 0 {
                    teardown.join();
                    std::process::exit(code);
                }
                return Ok(());
            }
            RunOutcome::Reload {
                tabs: reloaded,
                focused: f,
            } => {
                let started = std::time::Instant::now();
                let last_good = (stack.config.clone(), stack.model.clone());
                // Shut the old host down first so nothing can repopulate
                // the registry after the clear: its senders disconnect, the
                // watchdog aborts in-flight callbacks, and only this thread
                // issues loads. The old VM then shares nothing with the new
                // stack, so its slow join (up to 2s) can run on a
                // background thread.
                stack.plugin_host.begin_shutdown();
                ToolRegistry::global().clear_lua();
                teardown.defer(move || drop(stack));
                let (new_stack, new_warnings) = build_stack(&cli, &cwd, &storage, Some(last_good))?;
                tabs = reloaded;
                if tabs.is_empty() {
                    tabs.push(AppSession::new(&new_stack.model.spec(), &cwd_str));
                }
                stack = new_stack;
                warnings = new_warnings;
                focused = f.min(tabs.len() - 1);
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    tabs = tabs.len(),
                    "reload: rebuilt plugins and config"
                );
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre::eyre;
    use maki_config::RawConfig;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// `second_saw_first` requires both joins: `defer` joining the first
    /// closure before spawning the second, and `Drop` joining the second
    /// before the assert reads the flag.
    #[test]
    fn teardown_defer_joins_previous_and_drop_joins_last() {
        let first_done = Arc::new(AtomicBool::new(false));
        let second_saw_first = Arc::new(AtomicBool::new(false));
        let mut teardown = Teardown::default();

        let set = Arc::clone(&first_done);
        teardown.defer(move || set.store(true, Ordering::Release));

        let read = Arc::clone(&first_done);
        let record = Arc::clone(&second_saw_first);
        teardown.defer(move || record.store(read.load(Ordering::Acquire), Ordering::Release));

        drop(teardown);
        assert!(second_saw_first.load(Ordering::Acquire));
    }

    #[test]
    fn teardown_swallows_panic_and_keeps_working() {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        let after_panic_ran = Arc::new(AtomicBool::new(false));
        let mut teardown = Teardown::default();
        teardown.defer(|| panic!("intentional"));
        let set = Arc::clone(&after_panic_ran);
        teardown.defer(move || set.store(true, Ordering::Release));
        drop(teardown);

        std::panic::set_hook(prev_hook);
        assert!(after_panic_ran.load(Ordering::Acquire));
    }

    fn test_config() -> Config {
        RawConfig::default()
            .into_config(false)
            .expect("default config")
    }

    #[test]
    fn broken_config_with_fallback_uses_last_good_and_warns() {
        let mut last_good = test_config();
        last_good.always_fast = true;
        let mut warnings = Vec::new();

        let config = config_or_fallback(Err(eyre!("boom")), Some(last_good), &mut warnings)
            .expect("fallback config");

        assert!(config.always_fast);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].starts_with(CONFIG_FALLBACK_WARNING),
            "{warnings:?}"
        );
        assert!(warnings[0].contains("boom"), "{warnings:?}");
    }

    #[test]
    fn broken_config_without_fallback_is_fatal() {
        let mut warnings = Vec::new();
        let err = match config_or_fallback(Err(eyre!("boom")), None, &mut warnings) {
            Err(e) => e,
            Ok(_) => panic!("expected error without fallback"),
        };
        assert!(err.to_string().contains("boom"));
        assert!(warnings.is_empty());
    }
}
