use std::env;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_agent::tools::ToolRegistry;
use maki_config::project::{self, ProjectDecision, TrustAnswer, TrustMode, policy_grant};
use maki_config::{Config, load_env_files};
use maki_lua::{InitFiles, Interaction, PluginHost};
use maki_storage::StateDir;

use crate::cli::Cli;
use crate::interrupt::Interrupt;
use crate::provider_scripts;
use crate::setup;

async fn load_stack(
    cli: &Cli,
    trust: &ProjectDecision,
) -> Result<(PluginHost, Config, Vec<String>)> {
    let mut plugin_host = PluginHost::start(
        Arc::clone(ToolRegistry::global_arc()),
        Interaction::None,
        !cli.no_jit,
    )
    .context("initialize lua plugin host")?;

    let (config, warnings) = super::load_plugins(
        &mut plugin_host,
        cli.no_plugins,
        super::BuiltinFailure::Fatal,
        Interaction::None,
        |host, names, warnings| {
            warnings.extend(trust.warning.clone());
            let mut config = host
                .load_init_files(
                    InitFiles::resolve(&trust.project_config, cli.no_plugins),
                    warnings,
                )
                .context("load init.lua files")?
                .unwrap_or_default()
                .into_config(&names(host)?)
                .context("invalid config")?;
            cli.apply_tool_flags(&mut config.agent)?;
            config.validate()?;
            Ok(config)
        },
    )
    .await?;
    Ok((plugin_host, config, warnings))
}

pub async fn run(
    cli: &Cli,
    model_arg: Option<String>,
    yolo: bool,
    trust_mode: TrustMode,
    legacy_notice: Option<String>,
) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    maki_providers::model_registry::load(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let mut trust = project::resolve(&storage, &cwd, trust_mode);
    load_env_files(&trust.project_config);

    let (mut plugin_host, mut config, mut warnings) = load_stack(cli, &trust).await?;

    // The `trust.paths` policy lives in the config it gates, so a grant for the
    // launch folder means building once untrusted and again trusted, as the TUI
    // does. Dropping the old host joins its thread before `load_env_files`
    // touches the process env.
    if policy_grant(&trust.state, &config.trust).is_some() {
        trust = project::apply_answer(&storage, trust, TrustAnswer::Trust);
        plugin_host.begin_shutdown();
        ToolRegistry::global().clear_lua();
        drop(plugin_host);
        load_env_files(&trust.project_config);
        (plugin_host, config, warnings) = load_stack(cli, &trust).await?;
    }
    warnings.extend(provider_scripts::startup_warning());
    warnings.extend(legacy_notice);
    super::report_warnings(warnings);

    let timeouts = maki_providers::Timeouts::from(&config.provider);

    let (model, warning) =
        setup::resolve_model(model_arg.as_deref(), &config.provider, &storage).await?;
    super::report_warnings(Vec::from_iter(warning));
    setup::remember_thinking(&mut config.session_defaults, &storage);

    setup::init_logging(&config.storage, trust.project_config.config_root());
    setup::init_telemetry(&config.telemetry);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();

    let prompt_slots = plugin_host.event_handle().collect_prompt_slots(None);

    let event_handle = plugin_host.event_handle();
    let interrupt = Interrupt::install();
    maki_acp::run(maki_acp::AcpParams {
        model,
        config: config.agent,
        timeouts,
        initial_wd: cwd,
        prompt_slots: Arc::new(prompt_slots),
        yolo: yolo || config.always_yolo,
        defaults: config.session_defaults,
        model_policy: Arc::new(config.provider.model_policy.clone()),
        plugin_rules: plugin_host.plugin_rules(),
        trust_mode,
        trust_policy: Arc::new(config.trust),
        on_session_end: Some(Arc::new(move |id, reason| {
            let handle = event_handle.clone();
            Box::pin(async move { handle.end_sessions([id], reason).await })
        })),
        storage,
        background: cli.background_wait().is_some(),
        host_prompt: cli.host_prompt(),
        stop: interrupt.stop(),
    })
    .await?;
    drop(plugin_host);
    interrupt.exit_if_caught();
    Ok(())
}
