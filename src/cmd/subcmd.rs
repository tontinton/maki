use std::env;
use std::path::Path;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use maki_agent::mcp::{config as mcp_config, oauth as mcp_oauth};
use maki_agent::tools::ToolRegistry;
use maki_config::{load_env_files, load_permissions};
use maki_lua::PluginHost;
use maki_providers::provider::fetch_all_models;
use maki_providers::{copilot_auth, dynamic, openai_auth};
use maki_storage::StateDir;

pub fn auth_login(provider: &str, storage: &StateDir) -> Result<()> {
    match provider {
        "openai" => openai_auth::login(storage)?,
        "copilot" => copilot_auth::login()?,
        slug => dynamic::login(slug)?,
    }
    Ok(())
}

pub fn auth_logout(provider: &str, storage: &StateDir) -> Result<()> {
    match provider {
        "openai" => openai_auth::logout(storage)?,
        "copilot" => copilot_auth::logout()?,
        slug => dynamic::logout(slug)?,
    }
    Ok(())
}

pub fn models() {
    smol::block_on(fetch_all_models(|batch| {
        for model in batch.models {
            println!("{model}");
        }
        for warning in batch.warnings {
            eprintln!("warning: {warning}");
        }
    }));
}

pub fn index(path: &str, no_plugins: bool) -> Result<()> {
    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let mut host = if no_plugins {
        PluginHost::disabled()
    } else {
        PluginHost::new(Arc::clone(ToolRegistry::native_arc()))
            .context("initialize lua plugin host")?
    };

    let raw_config = host.load_init_files(&cwd).context("load init.lua files")?;

    let mut config = raw_config.unwrap_or_default().into_config(false);
    config.permissions = load_permissions(&cwd);

    host.load_builtins(&config.plugins)
        .context("load builtin plugins")?;

    let abs_path = Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(path).to_path_buf());
    let input = serde_json::json!({"path": abs_path.to_str().unwrap_or(path)});
    let reg = ToolRegistry::native_arc();
    let entry = reg
        .get("index")
        .ok_or_else(|| color_eyre::eyre::eyre!("index tool not registered"))?;
    let inv = entry
        .tool
        .parse(&input)
        .map_err(|e| color_eyre::eyre::eyre!("parse index input: {e}"))?;
    let ctx = maki_agent::tools::cli_tool_ctx();
    let result: Result<maki_agent::ToolOutput, String> =
        smol::block_on(async { inv.execute(&ctx).await });
    match result {
        Ok(output) => print!("{}", output.as_text()),
        Err(e) => bail!("index failed: {e}"),
    }
    Ok(())
}

pub fn mcp_auth(server: &str, storage: &StateDir) -> Result<()> {
    smol::block_on(async {
        let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
        let config = mcp_config::load_config(&cwd);
        let raw = config
            .mcp
            .get(server)
            .ok_or_else(|| color_eyre::eyre::eyre!("unknown MCP server: {server}"))?;
        let url = match mcp_config::parse_server(server.to_owned(), raw.clone())?.transport {
            mcp_config::Transport::Http { url, .. } => url,
            _ => color_eyre::eyre::bail!("server '{server}' is not an HTTP transport"),
        };
        mcp_oauth::authenticate(server, &url, None, storage).await?;
        eprintln!("Successfully authenticated with MCP server '{server}'");
        Ok(())
    })
}

pub fn mcp_logout(server: &str, storage: &StateDir) -> Result<()> {
    let deleted = maki_storage::auth::delete_mcp_auth(storage, server)?;
    if deleted {
        eprintln!("Removed OAuth credentials for MCP server '{server}'");
    } else {
        eprintln!("No stored credentials for MCP server '{server}'");
    }
    Ok(())
}
