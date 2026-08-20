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

    let discovery = maki_lua::discover_installed(no_plugins);
    for problem in discovery.problems {
        eprintln!(
            "warning: {}",
            super::sanitize_warning(format!("skipping package: {problem}"))
        );
    }
    let packages = discovery.packages;
    let raw_config = plugin_host
        .load_init_files_or_skip(no_plugins, &cwd)
        .context("load init.lua files")?;

    // Read after the init files, so declared packages are configurable and
    // installable here too, not only under the terminal UI.
    let declared = if no_plugins {
        Vec::new()
    } else {
        plugin_host
            .declared_packages()
            .context("read declared packages")?
    };

    let known: Vec<String> = packages
        .iter()
        .map(|p| p.name.clone())
        .chain(declared.iter().map(|d| d.spec.name.clone()))
        .collect();
    let mut config = raw_config
        .unwrap_or_default()
        .into_config(false, &known)
        .context("invalid config")?;
    config.permissions = load_permissions(&cwd);

    if yolo || config.always_yolo {
        config.permissions.yolo = true;
    }
    config.validate()?;

    plugin_host
        .load_builtins(&config.plugins)
        .context("load builtin plugins")?;
    for warning in plugin_host.load_packages(&packages, &config.plugins) {
        eprintln!("warning: {warning}");
    }
    let installed = maki_lua::install_declared(&declared, maki_lua::Interaction::None);
    for warning in installed
        .failures
        .iter()
        .cloned()
        .chain(plugin_host.load_packages(&installed.packages, &config.plugins))
    {
        eprintln!("warning: {warning}");
    }

    // Same order as the terminal entry point: everything loaded first, then
    // whatever `init.lua` asked to change. Draining here rather than inside
    // the Lua call is what keeps unloading from waiting on the thread that
    // requested it.
    let mut active: std::collections::BTreeSet<String> = packages
        .iter()
        .chain(&installed.packages)
        .filter(|p| p.eager && config.plugins.packages.iter().any(|n| n == &p.name))
        .map(|p| p.name.clone())
        .collect();
    let available: Vec<maki_lua::DiscoveredPackage> = packages
        .iter()
        .chain(&installed.packages)
        .cloned()
        .collect();
    maki_lua::drain_pack_ops(
        &plugin_host,
        &declared,
        &available,
        &mut active,
        &config.plugins,
    );

    let timeouts = maki_providers::Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };

    let model = setup::resolve_model(model_arg.as_deref(), &config.provider, &storage)?;

    setup::init_logging(&config.storage);
    setup::init_telemetry(&config.telemetry);
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
