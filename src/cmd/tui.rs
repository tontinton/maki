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
use maki_config::project::{self, ProjectDecision, TrustAnswer, TrustMode, policy_grant};
use maki_config::{Config, ProjectConfig, load_env_files, load_permissions};
use maki_lua::{InitFiles, Interaction, PackPlan, PackReport, PluginHost};
use maki_providers::model::Model;
use maki_storage::StateDir;
use maki_ui::{OpenSession, RunOutcome};

use crate::cli::{Cli, normalize_tool_name};
use crate::resume::{self, Resolved};
use crate::setup;

const FALLBACK_MODEL_SPEC: &str = "anthropic/claude-sonnet-4-20250514";
const STALE_PROJECT_CONFIG: &str = ".maki/config.toml";
const STALE_GLOBAL_CONFIG: &str = "config.toml";
const CONFIG_FALLBACK_WARNING: &str = "config reload failed, using previous config";
const MODEL_FALLBACK_WARNING: &str = "model resolution failed, keeping previous model";
const POLICY_GRANT_NOTICE: &str = "folder trusted by trust.paths pattern";

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
        maki_providers::Timeouts::from(&self.config.provider)
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

fn discover_commands(disable: bool, cwd: &Path) -> Vec<CustomCommand> {
    if disable {
        return Vec::new();
    }
    command::discover_commands(cwd)
}

fn load_config(
    plugin_host: &PluginHost,
    cli: &Cli,
    init_files: InitFiles,
    project_config: &ProjectConfig,
    names: &super::KnownNames<'_>,
    warnings: &mut Vec<String>,
) -> Result<Config> {
    let raw_config = plugin_host
        .load_init_files(init_files, warnings)
        .context("load init.lua files")?;

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(&names(plugin_host)?)
        .context("invalid config")?;
    config.permissions = load_permissions(project_config);

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
/// What every generation is built from, unchanged for the life of the process.
struct Launch<'a> {
    cli: &'a Cli,
    cwd: &'a Path,
    storage: &'a StateDir,
    interaction: Interaction,
}

/// The one construction path for a generation: first startup passes
/// `fallback: None` (fail-fast); `/reload` passes the last-good config and
/// model so a broken config reopens the UI with a warning instead of exiting.
fn build_stack(
    launch: &Launch<'_>,
    trust: &ProjectDecision,
    fallback: Option<(Config, Model)>,
) -> Result<(Stack, Vec<String>)> {
    let cli = launch.cli;
    let mut plugin_host = PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !cli.no_jit)
        .context("initialize lua plugin host")?;

    let (fallback_config, fallback_model) = fallback.unzip();
    let (config, mut warnings) = super::load_plugins(
        &mut plugin_host,
        cli.no_plugins,
        if fallback_model.is_some() {
            super::BuiltinFailure::Warn
        } else {
            super::BuiltinFailure::Fatal
        },
        launch.interaction,
        |host, names, warnings| {
            warnings.extend(trust.warning.clone());
            let loaded = load_config(
                host,
                cli,
                InitFiles::resolve(&trust.project_config, cli.no_plugins),
                &trust.project_config,
                names,
                warnings,
            );
            config_or_fallback(loaded, fallback_config, warnings)
        },
    )?;

    let commands = discover_commands(cli.no_commands, launch.cwd);

    let model_result = setup::resolve_model(cli.model.as_deref(), &config.provider, launch.storage);
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
        super::sanitize_warnings(&warnings),
    ))
}

/// The tab a run opens on. Which session that is lives in [`crate::resume`],
/// and the only step left here is the TUI-only one: an explicit `--model` is a
/// choice about the session being opened, and a session's own spec is what
/// every later switch reads.
fn open_tab(resolved: Resolved, model: &str, explicit_model: bool, cwd: &str) -> OpenSession {
    let mut tab = resolved.into_session(model, cwd);
    if explicit_model {
        tab.session.set_model(model.to_owned());
    }
    tab
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

/// A project `.env` is loaded exactly once, when a folder that was not trusted
/// becomes trusted. Every other rebuild must leave the process env alone.
fn should_load_project_env(was_trusted: bool, now_trusted: bool) -> bool {
    !was_trusted && now_trusted
}

/// Replaces a generation's stack. The only site in the binary that loads the
/// project environment, because the ordering is the whole safety argument: the
/// old plugin host's thread must be joined before `load_env_files` touches the
/// process env, and the env must be in place before the new host reads config.
fn rebuild(
    launch: &Launch<'_>,
    mut stack: Stack,
    teardown: &mut Teardown,
    trust: &ProjectDecision,
    was_trusted: bool,
    pack: Option<PackPlan>,
    fallback: Option<(Config, Model)>,
) -> Result<(Stack, Vec<String>, Option<PackReport>)> {
    // Shut the old host down first so nothing can repopulate the registry after
    // the clear: its senders disconnect, the watchdog aborts in-flight
    // callbacks, and only this thread issues loads.
    stack.plugin_host.begin_shutdown();
    ToolRegistry::global().clear_lua();

    let load_env = should_load_project_env(was_trusted, trust.project_config.is_trusted());
    // A package plan has to wait for the old revision leases to close, and an
    // env load may not run beside a thread that can read the env; an ordinary
    // reload does neither and drops the stack in the background.
    if pack.is_some() || load_env {
        teardown.join();
        drop(stack);
    } else {
        teardown.defer(move || drop(stack));
    }
    let pack_report = pack.map(maki_lua::apply_pack_plan);

    if load_env {
        // This calls `std::env::set_var`. On the startup grant path nothing
        // long-lived exists yet, so the join above is the whole story. The only
        // site that reaches it with threads alive is `/trust`, a rare explicit
        // action, and only after the join: smol's worker pool and any telemetry
        // exporter may still be parked.
        load_env_files(&trust.project_config);
    }

    let (stack, warnings) = build_stack(launch, trust, fallback)?;
    Ok((stack, warnings, pack_report))
}

pub fn run(mut cli: Cli) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    maki_providers::model_registry::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());

    // Only the interactive UI can answer an install confirmation or a trust
    // prompt. The other modes refuse the install with its reason and skip
    // untrusted project config with a warning, terminal or not.
    let headless = cli.print || cli.is_sdk_mode();
    let interaction = if headless {
        Interaction::None
    } else {
        Interaction::Tty
    };
    // `--trust` answers the question up front, so it holds wherever Maki runs.
    // Without it the store is the only source of an answer this early.
    let mode = if cli.trust {
        TrustMode::Session
    } else {
        TrustMode::Consult
    };
    let mut trust = project::resolve(&storage, &cwd, mode);
    load_env_files(&trust.project_config);
    warn_stale_config_toml(&trust.project_config);
    let mut teardown = Teardown::default();
    let launch = Launch {
        cli: &cli,
        cwd: &cwd,
        storage: &storage,
        interaction,
    };
    let (mut stack, mut startup_warnings) = build_stack(&launch, &trust, None)?;

    // The card owns the terminal, so it is drawn before logging, telemetry and
    // the panic hook claim it, and while the process is still single-threaded.
    // Every mode settles the question here, so a policy grant reaches `-p` and
    // the SDK exactly as it reaches the UI; only the card is TUI-only.
    let can_ask = !headless && io::stdin().is_terminal() && io::stderr().is_terminal();
    // Declared before the early returns so a policy grant can name its pattern
    // in the UI's first frame. A headless run returns before the UI exists and
    // drops it; its grant is already visible through `maki trust list`.
    let mut notice = None;
    // `unanswered` rather than `question`: a recorded `Never` is an answer, and
    // neither the card nor the policy may overturn one.
    let answer = trust.state.unanswered().and_then(|question| {
        policy_grant(question, &stack.config.trust)
            .map(|pattern| {
                notice = Some(format!("{POLICY_GRANT_NOTICE} {pattern}"));
                TrustAnswer::Trust
            })
            // `prompt = false` silences the card only: a `paths` match is an
            // answer given in advance, not a prompt.
            .or_else(|| {
                (can_ask && stack.config.trust.prompt).then(|| maki_ui::ask_trust(question))
            })
    });
    match answer {
        Some(answer) => {
            let was_trusted = trust.project_config.is_trusted();
            trust = project::apply_answer(&storage, trust, answer);
            if should_load_project_env(was_trusted, trust.project_config.is_trusted()) {
                // The untrusted build's warnings describe a config this run no
                // longer uses, and `build_stack` reports `trust.warning` itself.
                (stack, startup_warnings, _) = rebuild(
                    &launch,
                    stack,
                    &mut teardown,
                    &trust,
                    was_trusted,
                    None,
                    None,
                )?;
            } else {
                startup_warnings.extend(trust.warning.clone());
            }
        }
        // A run that cannot ask says so instead of silently dropping the files.
        None => startup_warnings.extend(trust.state.restricted_warning()),
    }

    setup::init_logging(&stack.config.storage);
    setup::init_telemetry(&stack.config.telemetry);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();

    // Discovery runs before logging is initialized, so a package problem has
    // no log to reach either. The TUI shows these in its first generation; a
    // mode that never opens the UI has to report them here or a broken
    // package fails in complete silence.
    if cli.is_sdk_mode() || cli.print {
        for warning in &startup_warnings {
            eprintln!("warning: {warning}");
        }
    }

    // Resolved before the modes diverge, so `--print`, the SDK and the UI
    // cannot pick different sessions, different ids or a different cwd to look
    // in, and all three report the same start to telemetry.
    let cwd_str = cwd.to_string_lossy().into_owned();
    let resolved = resume::resolve(&cli, &cwd_str, &storage)?;
    setup::report_session_start(resolved.start_type, Some(&resolved.id));

    if cli.is_sdk_mode() {
        let (resumed, claim) = resolved.into_resumed();
        let prompt_slots = stack.plugin_host.event_handle().collect_prompt_slots();
        let timeouts = stack.timeouts();
        crate::sdk_mode::run(crate::sdk_mode::SdkParams {
            cli,
            resumed,
            claim,
            storage: storage.clone(),
            model: stack.model,
            config: stack.config.agent,
            permissions_config: stack.config.permissions,
            timeouts,
            prompt_slots,
            defaults: stack.config.session_defaults,
            model_policy: Arc::new(stack.config.provider.model_policy.clone()),
            plugin_rules: stack.plugin_host.plugin_rules(),
            lua_handle: stack.plugin_host.event_handle(),
            project_config: trust.project_config.clone(),
        })
        .context("run sdk mode")?;
        return Ok(());
    }

    if cli.print {
        let (resumed, claim) = resolved.into_resumed();
        let timeouts = stack.timeouts();
        crate::print::run(crate::print::PrintParams {
            model: stack.model,
            prompt: cli.initial_prompt,
            image_paths: cli.images,
            format: cli.output_format,
            verbose: cli.verbose,
            config: stack.config.agent,
            permissions_config: stack.config.permissions,
            timeouts,
            lua_handle: stack.plugin_host.event_handle(),
            defaults: stack.config.session_defaults,
            model_policy: Arc::new(stack.config.provider.model_policy.clone()),
            plugin_rules: stack.plugin_host.plugin_rules(),
            project_config: trust.project_config.clone(),
            resumed,
            claim,
            storage: storage.clone(),
        })
        .context("run print mode")?;
        return Ok(());
    }

    let mut tabs = vec![open_tab(
        resolved,
        &stack.model.spec(),
        cli.model.is_some(),
        &cwd_str,
    )];
    let mut focused = 0;
    let mut warnings = startup_warnings;
    let mut initial_prompt = read_initial_prompt(cli.initial_prompt.take())?;
    let launch = Launch {
        cli: &cli,
        cwd: &cwd,
        storage: &storage,
        interaction,
    };

    loop {
        for OpenSession { session, .. } in &mut tabs {
            if session.messages().is_empty() {
                stack.config.session_defaults.seed(&mut session.meta);
            }
        }

        let outcome = maki_ui::run(
            maki_ui::EventLoopParams {
                // The startup default only (`--model`, then last-used, then
                // config). It seeds `ModelSlots` and catches a session whose
                // model will not resolve. Otherwise each tab runs on its own
                // recorded spec.
                model: stack.model.clone(),
                needs_login: stack.needs_login,
                commands: std::mem::take(&mut stack.commands),
                sessions: std::mem::take(&mut tabs),
                focused,
                startup_warnings: std::mem::take(&mut warnings),
                startup_notice: notice.take(),
                storage: storage.clone(),
                config: stack.config.agent.clone(),
                ui_config: stack.config.ui.clone(),
                input_history_size: stack.config.storage.input_history_size,
                permissions: Arc::new(maki_agent::permissions::PermissionManager::new(
                    stack.config.permissions.clone(),
                    cwd.clone(),
                    trust.project_config.clone(),
                    stack.plugin_host.plugin_rules(),
                )),
                timeouts: stack.timeouts(),
                exit_on_done: cli.exit_on_done,
                lua_command_reader: stack.plugin_host.command_reader(),
                keymap_reader: stack.plugin_host.keymap_reader(),
                hint_reader: stack.plugin_host.hint_reader(),
                ui_action_rx: stack.plugin_host.ui_action_rx(),
                ui_wake_rx: stack.plugin_host.ui_wake_rx(),
                ui_attachment: stack.plugin_host.ui_attachment(),
                lua_event_handle: stack.plugin_host.event_handle(),
                model_policy: Arc::new(stack.config.provider.model_policy.clone()),
                project_config: trust.project_config.clone(),
                trust_question: trust.state.question().cloned(),
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
            RunOutcome::Reload {
                tabs: reloaded,
                focused: f,
                pack,
            } => {
                let started = Instant::now();
                let last_good = (stack.config.clone(), stack.model.clone());
                let was_trusted = trust.project_config.is_trusted();
                // Re-read the store: a `/trust` grant, or a `maki trust add`
                // run in another terminal, only takes effect from here.
                trust = project::resolve(&storage, &cwd, mode);
                let (new_stack, new_warnings, pack_report) = rebuild(
                    &launch,
                    stack,
                    &mut teardown,
                    &trust,
                    was_trusted,
                    pack,
                    Some(last_good),
                )?;
                tabs = reloaded;
                if tabs.is_empty() {
                    let replacement = Resolved::fresh(&storage);
                    setup::report_session_start(replacement.start_type, Some(&replacement.id));
                    tabs.push(replacement.into_session(&new_stack.model.spec(), &cwd_str));
                }
                stack = new_stack;
                if let Some(report) = pack_report {
                    notice = report.changed().then(|| report.summary());
                    warnings = super::sanitize_warnings(&report.failures);
                }
                warnings.extend(new_warnings);
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

/// Trust has nothing to say about this file, because Maki no longer reads it.
/// Someone who declined trust deserves to hear that the file is inert just as
/// much as someone who accepted.
fn warn_stale_config_toml(project_config: &ProjectConfig) {
    let stale_paths = [
        maki_config::global_config_dir().map(|dir| dir.join(STALE_GLOBAL_CONFIG)),
        Some(project_config.config_root().join(STALE_PROJECT_CONFIG)),
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
    use clap::Parser;
    use color_eyre::eyre::eyre;
    use maki_agent::tools::ToolRegistry;
    use maki_config::RawConfig;
    use maki_providers::Message;
    use maki_storage::sessions::SessionClaim;
    use maki_ui::AppSession;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::tempdir;
    use test_case::test_case;

    const STARTUP_SPEC: &str = "anthropic/claude-sonnet-4-5";
    const STORED_SPEC: &str = "zai/glm-4.6";
    const CWD: &str = "/project";
    const STORED_MESSAGE: &str = "the turn the stored session already had";

    fn no_names(_: &PluginHost) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    #[test_case(false, false, false ; "untrusted reload leaves the env alone")]
    #[test_case(true, true, false ; "ordinary reload of a trusted folder does not reload the env")]
    #[test_case(true, false, false ; "a revoked folder does not reload the env")]
    #[test_case(false, true, true ; "a fresh grant loads the project env")]
    fn project_env_loads_only_on_the_grant(was_trusted: bool, now_trusted: bool, expected: bool) {
        assert_eq!(should_load_project_env(was_trusted, now_trusted), expected);
    }

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
            .into_config(&[])
            .expect("default config")
    }

    #[test]
    fn broken_config_with_fallback_uses_last_good_and_warns() {
        let mut last_good = test_config();
        last_good.session_defaults.fast = true;
        let mut warnings = Vec::new();

        let config = config_or_fallback(Err(eyre!("boom")), Some(last_good), &mut warnings)
            .expect("fallback config");

        assert!(config.session_defaults.fast);
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

        let config = load_config(
            &plugin_host,
            &cli,
            InitFiles::Disabled,
            &ProjectConfig::for_project(dir.path()),
            &no_names,
            &mut Vec::new(),
        )
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

        match load_config(
            &plugin_host,
            &cli,
            InitFiles::GlobalAndProject(maki_dir.join("init.lua")),
            &ProjectConfig::for_project(dir.path()),
            &no_names,
            &mut Vec::new(),
        ) {
            Err(_) => {}
            Ok(_) => panic!("broken init.lua must error without --no-plugins"),
        }

        plugin_host.begin_shutdown();
    }

    /// A session's recorded spec is the single source of truth for the model
    /// its tab runs on, so both directions matter. An explicit `--model` used
    /// to be ignored on a resumed session, and without one the startup default
    /// must not overwrite the recorded spec, or per tab models die on every
    /// restart.
    #[test_case(false, STORED_SPEC ; "a resumed tab keeps its own spec")]
    #[test_case(true, STARTUP_SPEC ; "explicit model wins over a resumed spec")]
    fn explicit_model_overrides_only_a_resumed_spec(explicit_model: bool, expected_spec: &str) {
        let dir = tempdir().expect("tempdir");
        let storage = StateDir::from_path(dir.path().join("state"));
        let cwd = dir.path().to_string_lossy().into_owned();
        let mut stored = AppSession::new(STORED_SPEC, &cwd);
        stored.push_message(Message::user(STORED_MESSAGE.to_owned()));
        let stored_id = stored.id;
        stored
            .save(
                &SessionClaim::acquire(stored_id, &storage).expect("claim"),
                &storage,
            )
            .expect("write session to disk");
        let resolved = resume::resolve(&Cli::parse_from(["maki", "-c"]), &cwd, &storage)
            .expect("continue resolves");

        let session = open_tab(resolved, STARTUP_SPEC, explicit_model, &cwd).session;

        assert_eq!(session.id, stored_id);
        assert_eq!(
            session
                .messages()
                .iter()
                .filter_map(|m| m.user_text())
                .collect::<Vec<_>>(),
            vec![STORED_MESSAGE]
        );
        assert_eq!(session.model, expected_spec);
    }

    /// A fresh tab opens on the startup spec, which already folded in
    /// `--model`, and under the id the resolver picked, so what the run
    /// reports is what it writes.
    #[test]
    fn a_fresh_tab_opens_on_the_resolved_id_and_the_startup_spec() {
        let dir = tempdir().expect("tempdir");
        let storage = StateDir::from_path(dir.path().join("state"));
        let resolved = Resolved::fresh(&storage);
        let id = resolved.id.id();
        let session = open_tab(resolved, STARTUP_SPEC, false, CWD).session;

        assert_eq!(session.id, id);
        assert!(session.messages().is_empty());
        assert_eq!(session.model, STARTUP_SPEC);
    }
}
