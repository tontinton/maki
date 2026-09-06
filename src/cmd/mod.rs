mod acp;
mod migrate;
mod subcmd;
mod tui;

use color_eyre::Result;
use color_eyre::eyre::Context;

use maki_config::Config;
use maki_lua::{DiscoveredPackage, Interaction, PluginHost};
use maki_storage::StateDir;

use crate::cli::{AuthAction, Cli, Command, McpAction, MigrateAction, TrustAction};
use crate::project_trust;
use crate::update;

fn sanitize_warnings(warnings: &[String]) -> Vec<String> {
    warnings
        .iter()
        .map(String::as_str)
        .map(maki_lua::sanitize_message)
        .collect()
}

fn report_warnings(warnings: Vec<String>) {
    for warning in sanitize_warnings(&warnings) {
        eprintln!("warning: {warning}");
    }
}

/// What a builtin that fails to load means for the run in progress.
#[derive(Clone, Copy)]
enum BuiltinFailure {
    /// Startup has nothing to fall back on.
    Fatal,
    /// `/reload` keeps the open UI alive, so the failure is only reported.
    Warn,
}

/// Discovered package names plus the ones `init.lua` declared with
/// `maki.pack.add`. Resolved by `build_config` instead of handed to it, because
/// the declared set is only complete once the init files have run, and
/// validation would otherwise reject a `plugins.<name>` for a package the user
/// just declared.
type KnownNames<'a> = dyn Fn(&PluginHost) -> Result<Vec<String>> + 'a;

/// The plugin startup every entry point shares. Packages are discovered before
/// `build_config` runs, so `plugins.<name>` can configure an installed package,
/// declared ones are installed after it, and everything is loaded after the
/// builtins, so a package claiming a builtin tool name is the side that fails.
///
/// `interaction` decides whether an install that needs the user's confirmation
/// may ask for it or has to fail; only the interactive UI can answer.
///
/// Warnings are returned sanitized, leaving the sink to the caller; the extra
/// `Vec` handed to `build_config` is for warnings raised while building it.
fn load_plugins(
    host: &mut PluginHost,
    no_plugins: bool,
    on_builtin_failure: BuiltinFailure,
    interaction: Interaction,
    build_config: impl FnOnce(&PluginHost, &KnownNames<'_>, &mut Vec<String>) -> Result<Config>,
) -> Result<(Config, Vec<String>)> {
    let discovery = maki_lua::discover_installed(no_plugins);
    // Includes the names discovery refused, so a package it could not read does
    // not become a config error pointing at the user's `plugins.<name>` table.
    let discovered_names = discovery.known_names();
    let mut warnings: Vec<String> = discovery
        .problems
        .into_iter()
        .map(|problem| format!("skipping package: {problem}"))
        .collect();

    let config = build_config(
        host,
        &|host: &PluginHost| {
            let mut names = discovered_names.clone();
            names.extend(declared_packages(host)?.into_iter().map(|d| d.spec.name));
            names.sort();
            names.dedup();
            Ok(names)
        },
        &mut warnings,
    )?;

    // Before any plugin can call `maki.net`, so the first request already sees
    // the hosts the user exempted from the private-address block.
    maki_lua::set_allowed_private_hosts(&config.net.allowed_private_hosts);

    if let Err(e) = host.load_builtins(&config.plugins) {
        let e = color_eyre::eyre::Report::from(e).wrap_err("load builtin plugins");
        match on_builtin_failure {
            BuiltinFailure::Fatal => return Err(e),
            BuiltinFailure::Warn => warnings.push(format!("{e:#}")),
        }
    }

    // Installing here rather than inside `maki.pack.add` keeps a clone off the
    // Lua thread, and is the phase Neovim's own `load` default defers to.
    let declared = declared_packages(host)?;
    let installed = maki_lua::install_declared(&declared, interaction);
    warnings.extend(installed.failures);
    let available: Vec<DiscoveredPackage> = discovery
        .packages
        .into_iter()
        .chain(installed.packages)
        .collect();
    warnings.extend(host.load_declared_packages(&available, &declared, &config.plugins));

    Ok((config, sanitize_warnings(&warnings)))
}

fn declared_packages(host: &PluginHost) -> Result<Vec<maki_lua::Declared>> {
    host.declared_packages().context("read declared packages")
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
        Some(Command::Trust { action }) => {
            let storage = StateDir::resolve().context("resolve state directory")?;
            match action {
                TrustAction::Add { path, yes } => {
                    project_trust::add(&storage, path.as_deref(), yes)?
                }
                TrustAction::Remove { path } => project_trust::remove(&storage, path.as_deref())?,
                TrustAction::List => {
                    for decision in project_trust::list(&storage)? {
                        println!("{decision}");
                    }
                }
            }
        }
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
