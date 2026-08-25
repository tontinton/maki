mod acp;
mod migrate;
mod subcmd;
mod tui;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_lua::{Declared, DiscoveredPackage, Interaction, PluginHost};
use maki_storage::StateDir;

use crate::cli::{AuthAction, Cli, Command, McpAction, MigrateAction};
use crate::update;

/// Strips control characters out of a message before it reaches a terminal.
///
/// Package code returns raw errors so logs keep their full detail. Terminal
/// and UI boundaries call this function once before they display a warning.
fn sanitize_warning(message: impl std::fmt::Display) -> String {
    maki_lua::sanitize_message(&message.to_string())
}

fn sanitize_warnings(warnings: Vec<String>) -> Vec<String> {
    warnings.into_iter().map(sanitize_warning).collect()
}

fn report_warnings(warnings: Vec<String>) {
    for warning in sanitize_warnings(warnings) {
        eprintln!("warning: {warning}");
    }
}

struct PackageDiscovery {
    names: Vec<String>,
    packages: Vec<DiscoveredPackage>,
    warnings: Vec<String>,
}

fn discover_external_packages(no_plugins: bool) -> PackageDiscovery {
    let discovery = maki_lua::discover_installed(no_plugins);
    PackageDiscovery {
        names: discovery.known_names(),
        packages: discovery.packages,
        warnings: discovery
            .problems
            .into_iter()
            .map(|problem| format!("skipping package: {problem}"))
            .collect(),
    }
}

fn load_external_packages(
    host: &PluginHost,
    packages: &[DiscoveredPackage],
    declared: &[Declared],
    reserved: &[String],
    config: &maki_config::PluginsConfig,
    interaction: Interaction,
    delivers_agent_events: bool,
) -> Result<Vec<String>> {
    let installed = maki_lua::install_declared(host, declared, interaction, delivers_agent_events);
    let mut warnings = installed.failures;
    let available: Vec<DiscoveredPackage> = packages
        .iter()
        .chain(installed.packages.iter())
        .cloned()
        .collect();

    let active = host.active_packages().context("read active packages")?;
    maki_lua::arm_packages(host, &available, declared, reserved, &active, config)
        .context("catalog activatable packages")?;
    warnings.extend(host.load_packages(packages, config));
    warnings.extend(host.load_declared_packages(&installed.packages, declared, config));
    warnings
        .extend(maki_lua::drain_pack_ops(host, declared, &available, config, interaction).failures);

    let active = host.active_packages().context("refresh active packages")?;
    maki_lua::arm_packages(host, &available, declared, reserved, &active, config)
        .context("refresh activatable packages")?;
    Ok(warnings)
}

pub fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Command::Auth { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                AuthAction::Login { provider } => {
                    subcmd::auth_login(provider.as_deref(), &storage)?
                }
                AuthAction::Logout { provider } => subcmd::auth_logout(&provider, &storage)?,
                AuthAction::Status => subcmd::auth_status(&storage)?,
            }
        }
        Some(Command::Index { path }) => {
            subcmd::index(&path, cli.no_plugins, cli.no_jit)?;
        }
        Some(Command::Models) => subcmd::models(cli.no_plugins, cli.no_jit)?,
        Some(Command::Mcp { action }) => {
            let storage = StateDir::resolve().context("resolve data directory")?;
            match action {
                McpAction::Auth { server } => subcmd::mcp_auth(&server, &storage)?,
                McpAction::Logout { server } => subcmd::mcp_logout(&server, &storage)?,
            }
        }
        Some(Command::Update { yes, no_color }) => {
            update::update(yes, no_color).map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        Some(Command::Rollback) => {
            update::rollback().map_err(|e| color_eyre::eyre::eyre!("{e}"))?;
        }
        Some(Command::Acp { model, yolo }) => {
            acp::run(model, yolo, cli.no_plugins, cli.no_jit)?;
        }
        Some(Command::Migrate { action }) => match action {
            MigrateAction::Xdg => migrate::xdg()?,
        },
        Some(Command::Prompt {
            variant,
            plan,
            tools,
            names,
        }) => {
            subcmd::prompt(&variant, plan, tools, names, cli.no_plugins, cli.no_jit)?;
        }
        None => {
            tui::run(cli)?;
        }
    }
    Ok(())
}
