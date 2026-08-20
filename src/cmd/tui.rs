use std::env;
use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_agent::command::{self, CustomCommand};
use maki_agent::tools::ToolRegistry;
use maki_config::{Config, load_env_files, load_permissions};
use maki_lua::{DiscoveredPackage, PluginHost};
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

/// Carries out `/packupdate` or `/packdel`, and reports what happened.
///
/// Runs between UI generations, which is the one place that can do this: it is
/// off the Lua thread, so unloading an owner cannot wait on the thread that
/// asked for it, and the terminal is not held, so a clone cannot stall a
/// redraw.
fn run_pack_command(
    stack: &Stack,
    command: &maki_lua::PackCommand,
    no_plugins: bool,
    reserved: &[String],
) -> Vec<String> {
    let declared = match stack.plugin_host.declared_packages() {
        Ok(declared) => declared,
        Err(e) => return vec![format!("could not read declared packages: {e}")],
    };

    let active = match stack.plugin_host.active_packages() {
        Ok(active) => active,
        Err(error) => return vec![format!("could not read active packages: {error}")],
    };
    let installed = match maki_lua::installed_names() {
        Some(installed) => installed,
        None => return vec!["the pack lockfile is unreadable".to_owned()],
    };

    let ops = match maki_lua::plan_command(command, &declared, &installed, &active) {
        Ok(ops) => ops,
        Err(msg) => return vec![msg],
    };
    // `/packupdate` and `/packdel` name managed packages, which resolve from
    // the lockfile, so no discovered set is needed here.
    let report = maki_lua::apply_pack_ops(
        &stack.plugin_host,
        &ops,
        &declared,
        &[],
        &stack.config.plugins,
        maki_lua::Interaction::Tty,
    );
    let mut messages = vec![report.summary()];
    messages.extend(report.failures.iter().cloned());
    let discovery = maki_lua::discover_installed(no_plugins);
    messages.extend(
        discovery
            .problems
            .into_iter()
            .map(|problem| super::sanitize_warning(format!("skipping package: {problem}"))),
    );
    let available: Vec<DiscoveredPackage> = discovery
        .packages
        .into_iter()
        .chain(
            declared
                .iter()
                .filter_map(|package| maki_lua::installed_package(&package.spec.name)),
        )
        .collect();
    match stack.plugin_host.active_packages() {
        Ok(active) => {
            if let Err(error) = maki_lua::arm_packages(
                &stack.plugin_host,
                &available,
                &declared,
                reserved,
                &active,
                &stack.config.plugins,
            ) {
                messages.push(format!("could not refresh package state: {error}"));
            }
        }
        Err(error) => messages.push(format!("could not refresh package state: {error}")),
    }
    messages
}

fn reserved_command_names(commands: &[CustomCommand]) -> Vec<String> {
    maki_ui::BUILTIN_COMMANDS
        .iter()
        .map(|command| command.name.to_string())
        .chain(commands.iter().map(CustomCommand::display_name))
        .collect()
}

fn catalog_packages(
    host: &PluginHost,
    packages: &[DiscoveredPackage],
    declared: &[maki_lua::Declared],
    reserved: &[String],
    config: &maki_config::PluginsConfig,
    warnings: &mut Vec<String>,
) {
    match host.active_packages() {
        Ok(active) => {
            if let Err(error) =
                maki_lua::arm_packages(host, packages, declared, reserved, &active, config)
            {
                warnings.push(format!("could not catalog packages: {error}"));
            }
        }
        Err(error) => warnings.push(format!("could not read active packages: {error}")),
    }
}

fn discover_commands(disable: bool) -> Vec<CustomCommand> {
    if disable {
        return Vec::new();
    }
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    command::discover_commands(&cwd)
}

fn load_config(
    plugin_host: &PluginHost,
    cli: &Cli,
    cwd: &Path,
    names: &[String],
) -> Result<Config> {
    let raw_config = plugin_host
        .load_init_files_or_skip(cli.no_plugins, cwd)
        .context("load init.lua files")?;

    // Read after the init files, because that is when `maki.pack.add` has run
    // and the declared set is complete. Both discovered and declared names
    // have to be known before validation, or `plugins.<name>` would reject a
    // package the user just declared.
    let declared = if cli.no_plugins {
        Vec::new()
    } else {
        plugin_host
            .declared_packages()
            .context("read declared packages")?
    };

    let known: Vec<String> = names
        .iter()
        .cloned()
        .chain(declared.iter().map(|d| d.spec.name.clone()))
        .collect();

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(cli.no_rtk, &known)
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

/// Returns the config to use, and whether it came from the fallback.
///
/// The second half matters because a rejected `init.lua` may also have called
/// `maki.pack.del`. Reporting that the old config was kept and then deleting
/// the package anyway would be the worst of both.
fn config_or_fallback(
    loaded: Result<Config>,
    fallback: Option<Config>,
    warnings: &mut Vec<String>,
) -> Result<(Config, bool)> {
    match (loaded, fallback) {
        (Ok(config), _) => Ok((config, false)),
        (Err(e), Some(last_good)) => {
            warnings.push(format!("{CONFIG_FALLBACK_WARNING}: {e:#}"));
            Ok((last_good, true))
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

    let mut plugin_host = PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !cli.no_jit)
        .context("initialize lua plugin host")?;

    // Discovered before the config is built, so `plugins.<name>` can configure
    // an installed package, and reused afterwards to load it.
    let discovery = maki_lua::discover_installed(cli.no_plugins);
    // Taken before the problems are consumed, and includes the names
    // discovery refused, so a package it could not read does not become a
    // config error pointing at the user's `plugins.<name>` table.
    let names = discovery.known_names();
    warnings.extend(
        discovery
            .problems
            .into_iter()
            .map(|problem| super::sanitize_warning(format!("skipping package: {problem}"))),
    );
    let packages = discovery.packages;

    let (fallback_config, fallback_model) = fallback.unzip();
    let reloading = fallback_model.is_some();
    let (config, config_rejected) = config_or_fallback(
        load_config(&plugin_host, cli, cwd, &names),
        fallback_config,
        &mut warnings,
    )?;

    if let Err(e) = plugin_host.load_builtins(&config.plugins) {
        let e = color_eyre::eyre::Report::from(e).wrap_err("load builtin plugins");
        if reloading {
            warnings.push(format!("{e:#}"));
        } else {
            return Err(e);
        }
    }

    let commands = discover_commands(cli.no_commands);

    // Everything the palette resolves ahead of a Lua command. A trigger on
    // one of these would never fire, so it is removed from automatic matching.
    let reserved = reserved_command_names(&commands);

    // Declared with `maki.pack.add` in init.lua. Installing here rather than
    // inside the call keeps a clone off the Lua thread, and is the phase
    // Neovim's own `load` default defers to.
    match plugin_host.declared_packages() {
        // Checked before anything is cloned or loaded, not after. A refused
        // config must not have any part of itself carried out, and installing
        // is already a visible act: it reaches the network, writes the
        // lockfile, and can run the fetched code under a package name the
        // fallback config happens to enable.
        Ok(_) if config_rejected => {
            catalog_packages(
                &plugin_host,
                &packages,
                &[],
                &reserved,
                &config.plugins,
                &mut warnings,
            );
            warnings.extend(plugin_host.load_packages(&packages, &config.plugins));
            warnings.push("package changes in the rejected config were not applied".to_owned());
            // Nothing drains the queue on this path, so it is closed here
            // instead. Left open, every later `maki.packadd` that the runtime
            // cannot serve would be recorded for a drain that already decided
            // not to run.
            if let Err(error) = plugin_host.seal_pack_ops() {
                warnings.push(format!(
                    "could not close the package activation queue: {error}"
                ));
            }
        }
        Ok(declared) => {
            let interaction = if cli.print || cli.is_sdk_mode() {
                maki_lua::Interaction::None
            } else {
                maki_lua::Interaction::Tty
            };
            warnings.extend(super::load_external_packages(
                &plugin_host,
                &packages,
                &declared,
                &reserved,
                &config.plugins,
                interaction,
                !cli.print && !cli.is_sdk_mode(),
            )?);
        }
        Err(e) => {
            catalog_packages(
                &plugin_host,
                &packages,
                &[],
                &reserved,
                &config.plugins,
                &mut warnings,
            );
            warnings.extend(plugin_host.load_packages(&packages, &config.plugins));
            warnings.push(format!("could not read declared packages: {e}"));
        }
    }

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
        let session = AppSession::load(id, storage).map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        setup::report_session_start(maki_otel::emit::START_RESUME, Some(session.id));
        return Ok(session);
    }
    if continue_session {
        match AppSession::latest(cwd, storage) {
            Ok(Some(session)) => {
                setup::report_session_start(maki_otel::emit::START_CONTINUE, Some(session.id));
                return Ok(session);
            }
            Ok(None) => {
                tracing::info!("no previous session found for this directory, starting new");
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to load latest session, starting new");
            }
        }
    }
    let session = AppSession::new(model, cwd);
    setup::report_session_start(maki_otel::emit::START_FRESH, Some(session.id));
    Ok(session)
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

    let (mut stack, startup_warnings) = build_stack(&cli, &cwd, &storage, None)?;

    setup::init_logging(&stack.config.storage);
    setup::init_telemetry(&stack.config.telemetry);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();

    if cli.is_sdk_mode() || cli.print {
        for warning in &startup_warnings {
            eprintln!("warning: {warning}");
        }
    }

    if cli.is_sdk_mode() {
        let fast = stack.config.always_fast && stack.model.supports_fast();
        let prompt_slots = stack.plugin_host.event_handle().collect_prompt_slots();
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
            model_policy: Arc::new(stack.config.provider.model_policy.clone()),
            plugin_rules: stack.plugin_host.plugin_rules(),
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
            Arc::new(stack.config.provider.model_policy.clone()),
            stack.plugin_host.plugin_rules(),
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
    let mut warnings = startup_warnings;
    let mut initial_prompt = read_initial_prompt(cli.initial_prompt.take())?;
    let mut teardown = Teardown::default();

    loop {
        for session in &mut tabs {
            if session.messages().is_empty() {
                session.meta.fast |= stack.config.always_fast;
                session.meta.workflow |= stack.config.always_workflow;
                if let Some(thinking) = stack.config.always_thinking {
                    session.meta.thinking = Some(thinking);
                }
            }
        }
        let focused_tab = &tabs[focused];
        let model = if focused_tab.messages().is_empty()
            || !stack
                .config
                .provider
                .model_policy
                .allows(&focused_tab.model)
        {
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
                ui_config: stack.config.ui.clone(),
                input_history_size: stack.config.storage.input_history_size,
                permissions: Arc::new(maki_agent::permissions::PermissionManager::new(
                    stack.config.permissions.clone(),
                    cwd.clone(),
                    stack.plugin_host.plugin_rules(),
                )),
                timeouts: stack.timeouts(),
                exit_on_done: cli.exit_on_done,
                lua_command_reader: stack.plugin_host.command_reader(),
                keymap_reader: stack.plugin_host.keymap_reader(),
                hint_reader: stack.plugin_host.hint_reader(),
                ui_action_rx: stack.plugin_host.ui_action_rx(),
                lua_event_handle: stack.plugin_host.event_handle(),
                model_policy: Arc::new(stack.config.provider.model_policy.clone()),
            },
            initial_prompt.take(),
        )
        .context("run UI")?;

        match outcome {
            RunOutcome::Exit { session_id, code } => {
                if let Some(session_id) = session_id {
                    eprintln!("Resume session:\n\n  maki -s {session_id}");
                }
                let started = Instant::now();
                drop(stack);
                let stack_ms = started.elapsed().as_millis() as u64;
                teardown.join();
                tracing::info!(
                    stack_ms,
                    teardown_ms = started.elapsed().as_millis() as u64 - stack_ms,
                    "plugin host and teardown joined"
                );
                if code != 0 {
                    maki_otel::shutdown(crate::TELEMETRY_SHUTDOWN_TIMEOUT);
                    std::process::exit(code);
                }
                return Ok(());
            }
            RunOutcome::Pack {
                tabs: reloaded,
                focused: f,
                command,
            } => {
                let started = Instant::now();
                let reserved = reserved_command_names(&stack.commands);
                warnings = run_pack_command(&stack, &command, cli.no_plugins, &reserved);
                tabs = reloaded;
                if tabs.is_empty() {
                    tabs.push(AppSession::new(&stack.model.spec(), &cwd_str));
                }
                stack.commands = discover_commands(cli.no_commands);
                focused = f.min(tabs.len() - 1);
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    tabs = tabs.len(),
                    "package command: resumed UI"
                );
            }
            RunOutcome::Reload {
                tabs: reloaded,
                focused: f,
            } => {
                let started = Instant::now();
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
                    let session = AppSession::new(&new_stack.model.spec(), &cwd_str);
                    setup::report_session_start(maki_otel::emit::START_FRESH, Some(session.id));
                    tabs.push(session);
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
    use std::fs;
    use std::path::PathBuf;
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
            .into_config(false, &[])
            .expect("default config")
    }

    #[test]
    fn broken_config_with_fallback_uses_last_good_and_warns() {
        let mut last_good = test_config();
        last_good.always_fast = true;
        let mut warnings = Vec::new();

        let (config, rejected) =
            config_or_fallback(Err(eyre!("boom")), Some(last_good), &mut warnings)
                .expect("fallback config");

        assert!(config.always_fast);
        assert!(
            rejected,
            "the caller has to know the new config was refused"
        );
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

    /// `--no-plugins` keeps the Lua host live (tools + default keymap
    /// still load) but skips user `init.lua`, so a broken project
    /// `init.lua` must not be executed in that mode.
    #[test]
    fn no_plugins_skips_broken_init_lua_but_keeps_host_alive() {
        use clap::Parser;
        use maki_agent::tools::ToolRegistry;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let maki_dir: PathBuf = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).expect("mkdir .maki");
        fs::write(
            maki_dir.join("init.lua"),
            "error('broken init lua must not run')",
        )
        .expect("write init.lua");

        let cli = Cli::parse_from(["maki", "--no-plugins"]);
        assert!(cli.no_plugins);

        let mut plugin_host = PluginHost::with_jit(Arc::new(ToolRegistry::new()), true)
            .expect("live host boots under --no-plugins");

        let config = load_config(&plugin_host, &cli, dir.path(), &[])
            .expect("no-plugins must skip the broken init.lua and still load defaults");
        assert!(
            !config.plugins.names.is_empty(),
            "default builtin plugins must still be enabled under --no-plugins"
        );

        plugin_host
            .load_builtins(&config.plugins)
            .expect("builtins load on the live host under --no-plugins");

        plugin_host.begin_shutdown();
    }

    /// Negative control for the test above: without `--no-plugins`, the
    /// same broken `init.lua` must surface as an error so the skip path
    /// cannot silently regress into a tautology.
    #[test]
    fn broken_init_lua_errors_without_no_plugins() {
        use clap::Parser;
        use maki_agent::tools::ToolRegistry;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let maki_dir: PathBuf = dir.path().join(".maki");
        fs::create_dir_all(&maki_dir).expect("mkdir .maki");
        fs::write(
            maki_dir.join("init.lua"),
            "error('broken init lua must not run')",
        )
        .expect("write init.lua");

        let cli = Cli::parse_from(["maki"]);
        assert!(!cli.no_plugins);

        let mut plugin_host =
            PluginHost::with_jit(Arc::new(ToolRegistry::new()), true).expect("live host boots");

        match load_config(&plugin_host, &cli, dir.path(), &[]) {
            Err(_) => {}
            Ok(_) => panic!("broken init.lua must error without --no-plugins"),
        }

        plugin_host.begin_shutdown();
    }
}
