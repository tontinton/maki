use std::env;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_agent::tools::ToolRegistry;
use maki_config::{load_env_files, load_permissions};
use maki_lua::PluginHost;
use maki_storage::StateDir;

use crate::setup;

pub fn run(model_arg: Option<String>, yolo: bool, no_plugins: bool, no_jit: bool) -> Result<()> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    maki_providers::model_registry::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let mut plugin_host = PluginHost::with_jit(Arc::clone(ToolRegistry::global_arc()), !no_jit)
        .context("initialize lua plugin host")?;

    let raw_config = plugin_host
        .load_init_files_or_skip(no_plugins, &cwd)
        .context("load init.lua files")?;

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(false)
        .context("invalid config")?;
    config.permissions = load_permissions(&cwd);

    if yolo || config.always_yolo {
        config.permissions.yolo = true;
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

    let model = setup::resolve_model(model_arg.as_deref(), &config.provider, &storage)?;

    setup::init_logging(&config.storage);
    setup::install_panic_log_hook();
    setup::warn_ignored_provider_fields();

    let prompt_slots = plugin_host.event_handle().collect_prompt_slots();

    maki_acp::run(maki_acp::AcpParams {
        model,
        config: config.agent,
        permissions_config: config.permissions,
        timeouts,
        initial_wd: cwd,
        prompt_slots: Arc::new(prompt_slots),
        yolo,
        model_policy: Arc::new(config.provider.model_policy.clone()),
        plugin_rules: plugin_host.plugin_rules(),
    })
}
