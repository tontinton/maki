use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use maki_agent::mcp::{config as mcp_config, oauth as mcp_oauth};
use maki_agent::tools::ToolRegistry;
use maki_config::providers::{
    ProviderDef, ProvidersConfig, all_builtins, builtin_provider, resolve_api_key_env,
    resolve_base_url, resolve_default_model, resolve_display_name, resolve_login_url, slugify,
};
use maki_config::{load_env_files, load_permissions};
use maki_lua::PluginHost;
use maki_providers::provider::fetch_all_models;
use maki_providers::{copilot_auth, dynamic, openai_auth};
use maki_storage::StateDir;
use maki_storage::auth::ProviderCredentials;
use maki_storage::auth::{
    delete_provider_credentials, load_provider_credentials, save_provider_credentials,
};
use maki_storage::model::persist_model;

pub fn auth_login(provider: Option<&str>, storage: &StateDir) -> Result<()> {
    match provider {
        Some("openai") => openai_auth::login(storage)?,
        Some("copilot") => copilot_auth::login(storage)?,
        Some(slug) => login_provider(&slugify(slug), storage)?,
        None => login_interactive(storage)?,
    }
    Ok(())
}

fn login_provider(slug: &str, storage: &StateDir) -> Result<()> {
    let builtin = builtin_provider(slug);
    let is_custom = ProvidersConfig::load().get(slug).is_some();
    if builtin.is_none() && dynamic::display_name(slug).is_none() && !is_custom {
        bail!("unknown provider '{slug}'");
    }
    let mut config = ProvidersConfig::load();
    let def = config.get(slug).cloned();

    let plan = select_plan(slug, builtin, def.as_ref())?;

    let needs_url = builtin.is_some_and(|b| b.needs_url);
    let host_url = if needs_url {
        Some(prompt_host_url(
            slug,
            &resolve_display_name(slug, def.as_ref()),
            def.as_ref(),
        )?)
    } else {
        None
    };

    let api_key_optional = needs_url;
    let login_url = resolve_login_url(slug, plan.as_deref());
    let api_key = prompt_api_key(
        login_url.as_deref(),
        &resolve_display_name(slug, def.as_ref()),
        api_key_optional,
    )?;

    let mut provider_def = def.unwrap_or_default();
    if let Some(plan_name) = &plan {
        provider_def.plan = Some(plan_name.clone());
    }
    if let Some(url) = &host_url {
        provider_def.base_url = Some(url.clone());
    }

    let has_key = !api_key.is_empty();
    if has_key {
        let creds = ProviderCredentials { api_key };
        save_provider_credentials(storage, slug, &creds).context("save credentials")?;
    }

    if plan.is_some() || needs_url || host_url.is_some() || builtin.is_none() {
        config.upsert(slug.to_string(), provider_def);
        config.save().context("save providers.toml")?;
    }

    let default_model = if needs_url {
        None
    } else {
        resolve_default_model(slug, config.get(slug))
    };
    if let Some(model) = &default_model {
        persist_model(storage, model);
    }

    println!();
    let display = resolve_display_name(slug, config.get(slug));
    println!("  \x1b[32m✓\x1b[0m Configured: {}", display);
    if let Some(url) = resolve_base_url(slug, config.get(slug)) {
        println!("  Endpoint: {}", url);
    }
    if let Some(model) = &default_model {
        println!("  Default model: {}", model);
    }
    if has_key {
        println!("  Credentials: ~/.local/state/maki/auth/{}.json", slug);
    } else {
        let env_var = resolve_api_key_env(slug, config.get(slug));
        println!(
            "  Set API key via: {} or run: maki auth login {}",
            env_var, slug
        );
    }

    Ok(())
}

fn login_interactive(storage: &StateDir) -> Result<()> {
    let builtins = all_builtins();
    let config = ProvidersConfig::load();
    let custom_slugs: Vec<&String> = config
        .providers
        .keys()
        .filter(|s| builtin_provider(s).is_none())
        .collect();

    println!();
    println!("  Available providers:");
    println!();
    for (i, b) in builtins.iter().enumerate() {
        let status = if load_provider_credentials(storage, b.slug).is_some() {
            "\x1b[32m✓\x1b[0m"
        } else if env::var(b.default_api_key_env).is_ok() {
            "\x1b[33m~\x1b[0m"
        } else {
            " "
        };
        println!("  {} {}. {:<14} {}", status, i + 1, b.slug, b.display_name);
    }
    let mut idx = builtins.len();
    for slug in &custom_slugs {
        idx += 1;
        let status = if load_provider_credentials(storage, slug).is_some() {
            "\x1b[32m✓\x1b[0m"
        } else {
            " "
        };
        let display = config
            .get(slug)
            .and_then(|d| d.display_name.as_deref())
            .unwrap_or(slug);
        println!("  {} {}. {:<14} {}", status, idx, slug, display);
    }
    idx += 1;
    let custom_idx = idx;
    println!("    {}. Custom provider...", custom_idx);
    println!();

    print!("  Select [1-{}]: ", custom_idx);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice: usize = input.trim().parse().context("enter a number")?;

    if choice == 0 || choice > custom_idx {
        bail!("invalid selection");
    }

    if choice == custom_idx {
        login_custom(storage)?;
    } else if choice <= builtins.len() {
        let slug = builtins[choice - 1].slug;
        login_provider(slug, storage)?;
    } else {
        let slug = custom_slugs[choice - builtins.len() - 1];
        login_provider(slug, storage)?;
    }

    Ok(())
}

fn login_custom(storage: &StateDir) -> Result<()> {
    print!("  Provider name: ");
    io::stdout().flush()?;
    let mut name = String::new();
    io::stdin().read_line(&mut name)?;
    let slug = slugify(&name);
    if slug.is_empty() {
        bail!("provider name cannot be empty");
    }

    println!("  Protocol:");
    println!("    1. openai   (OpenAI-compatible chat completions)");
    println!("    2. anthropic (Anthropic messages API)");
    println!("    3. google   (Google Gemini API)");
    print!("  Select [1-3]: ");
    io::stdout().flush()?;
    let mut proto_input = String::new();
    io::stdin().read_line(&mut proto_input)?;
    let protocol = match proto_input.trim() {
        "1" | "openai" => "openai",
        "2" | "anthropic" => "anthropic",
        "3" | "google" => "google",
        _ => bail!("invalid protocol selection"),
    };

    print!("  Base URL: ");
    io::stdout().flush()?;
    let mut url_input = String::new();
    io::stdin().read_line(&mut url_input)?;
    let base_url = url_input.trim().to_string();
    if base_url.is_empty() {
        bail!("base URL cannot be empty");
    }

    let display_name = format!("Custom ({slug})");
    let api_key_env = format!("{}_API_KEY", slug.to_uppercase().replace('-', "_"));

    print!("  API key (or Enter to skip): ");
    io::stdout().flush()?;
    let mut key_input = String::new();
    io::stdin().read_line(&mut key_input)?;
    let api_key = key_input.trim().to_string();

    let mut config = ProvidersConfig::load();
    let provider_def = ProviderDef {
        display_name: Some(display_name),
        protocol: Some(
            protocol
                .parse()
                .map_err(|e: String| color_eyre::eyre::eyre!("{e}"))?,
        ),
        base_url: Some(base_url.clone()),
        api_key_env: Some(api_key_env.clone()),
        discover_models: true,
        ..Default::default()
    };

    let has_key = !api_key.is_empty();
    if has_key {
        let creds = ProviderCredentials { api_key };
        save_provider_credentials(storage, &slug, &creds).context("save credentials")?;
    }

    config.upsert(slug.clone(), provider_def);
    config.save().context("save providers.toml")?;

    println!();
    println!("  \x1b[32m✓\x1b[0m Configured: {}", slug);
    println!("  Endpoint: {}", base_url);
    if has_key {
        println!("  Credentials: ~/.local/state/maki/auth/{}.json", slug);
    } else {
        println!(
            "  Set API key via: {} or run: maki auth login {}",
            api_key_env, slug
        );
    }
    println!("  Use with: maki -m {}/<model>", slug);

    Ok(())
}

fn select_plan(
    slug: &str,
    builtin: Option<&'static maki_config::providers::BuiltInProvider>,
    def: Option<&ProviderDef>,
) -> Result<Option<String>> {
    let plans = builtin.and_then(|b| b.plans);
    if plans.is_none_or(|p| p.len() <= 1) {
        if let Some(d) = def {
            return Ok(d.plan.clone());
        }
        return Ok(None);
    }
    let plans = plans.unwrap();

    if let Some(d) = def
        && d.plan.is_some()
    {
        return Ok(d.plan.clone());
    }

    println!();
    println!("  {} plan:", resolve_display_name(slug, def));
    for (i, (_key, plan)) in plans.iter().enumerate() {
        println!("    {}. {} ({})", i + 1, plan.display_name, plan.base_url);
    }
    println!();
    print!("  Select [1-{}]: ", plans.len());
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice: usize = input.trim().parse().context("enter a number")?;
    if choice == 0 || choice > plans.len() {
        bail!("invalid plan selection");
    }
    Ok(Some(plans[choice - 1].0.to_string()))
}

fn prompt_host_url(slug: &str, display_name: &str, def: Option<&ProviderDef>) -> Result<String> {
    let default = resolve_base_url(slug, def).unwrap_or_default();
    print!("  {} host URL [{}]: ", display_name, default);
    io::stdout().flush()?;

    let mut url = String::new();
    io::stdin().read_line(&mut url)?;
    let url = url.trim().to_string();

    Ok(if url.is_empty() { default } else { url })
}

fn prompt_api_key(url: Option<&str>, display_name: &str, optional: bool) -> Result<String> {
    if let Some(url) = url {
        if let Err(e) = open::that(url) {
            tracing::warn!(error = %e, "failed to open browser");
        }
        println!("  Opened {} in your browser.", url);
    }
    if optional {
        print!("  {} API key (or Enter to skip): ", display_name);
    } else {
        print!("  {} API key: ", display_name);
    }
    io::stdout().flush()?;

    let mut api_key = String::new();
    io::stdin().read_line(&mut api_key)?;
    let api_key = api_key.trim().to_string();

    Ok(api_key)
}

pub fn auth_logout(provider: &str, storage: &StateDir) -> Result<()> {
    let slug = slugify(provider);
    match provider {
        "openai" => openai_auth::logout(storage)?,
        "copilot" => copilot_auth::logout(storage)?,
        _ => {
            let mut config = ProvidersConfig::load();
            let deleted =
                delete_provider_credentials(storage, &slug).context("delete credentials")?;
            if deleted {
                println!("Removed credentials for '{}'.", slug);
            }
            if config.remove(&slug) {
                config.save().context("save providers.toml")?;
            }
            if !deleted && builtin_provider(&slug).is_none() {
                dynamic::logout(&slug)?;
            }
        }
    }
    Ok(())
}

pub fn auth_status(storage: &StateDir) -> Result<()> {
    let config = ProvidersConfig::load();
    let builtins = all_builtins();

    println!();
    for b in &builtins {
        let def = config.get(b.slug);
        let display = resolve_display_name(b.slug, def);

        if let Some(creds) = load_provider_credentials(storage, b.slug) {
            let plan_info = def
                .and_then(|d| d.plan.as_deref())
                .map(|p| format!(" ({})", p))
                .unwrap_or_default();
            let masked = if creds.api_key.len() > 8 {
                format!(
                    "{}...{}",
                    &creds.api_key[..4],
                    &creds.api_key[creds.api_key.len() - 4..]
                )
            } else {
                "****".to_string()
            };
            println!(
                "  \x1b[32m✓\x1b[0m {:<14} {} (key: {}){}",
                b.slug, display, masked, plan_info
            );
        } else if env::var(b.default_api_key_env).is_ok() {
            println!(
                "  \x1b[33m~\x1b[0m {:<14} {} (via {})",
                b.slug, display, b.default_api_key_env
            );
        } else if def.is_some_and(|d| d.base_url.is_some()) {
            println!("  \x1b[34m●\x1b[0m {:<14} {} (configured)", b.slug, display);
        } else {
            println!(
                "  \x1b[31m✗\x1b[0m {:<14} {} (run: maki auth login {})",
                b.slug, display, b.slug
            );
        }
    }

    for (slug, def) in &config.providers {
        if builtin_provider(slug).is_some() {
            continue;
        }
        let display = def.display_name.as_deref().unwrap_or(slug);
        if let Some(creds) = load_provider_credentials(storage, slug) {
            let masked = if creds.api_key.len() > 8 {
                format!(
                    "{}...{}",
                    &creds.api_key[..4],
                    &creds.api_key[creds.api_key.len() - 4..]
                )
            } else {
                "****".to_string()
            };
            println!(
                "  \x1b[32m✓\x1b[0m {:<14} {} (key: {})",
                slug, display, masked
            );
        } else {
            let default_env = format!("{}_API_KEY", slug.to_uppercase().replace('-', "_"));
            let env_var = def.api_key_env.as_deref().unwrap_or(&default_env);
            if env::var(env_var).is_ok() {
                println!(
                    "  \x1b[33m~\x1b[0m {:<14} {} (via {})",
                    slug, display, env_var
                );
            } else {
                println!(
                    "  \x1b[31m✗\x1b[0m {:<14} {} (run: maki auth login {})",
                    slug, display, slug
                );
            }
        }
    }
    println!();

    Ok(())
}

pub fn models() {
    smol::block_on(fetch_all_models(
        |batch| {
            for model in batch.models {
                println!("{model}");
            }
            for warning in batch.warnings {
                eprintln!("warning: {warning}");
            }
        },
        None,
    ));
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

    let mut config = raw_config
        .unwrap_or_default()
        .into_config(false)
        .context("invalid config")?;
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
    let result = smol::block_on(async { inv.execute(&ctx).await });
    match result.output {
        Ok(output) => print!("{}", output.as_text()),
        Err(e) => bail!("index failed: {e}"),
    }
    Ok(())
}

pub fn mcp_auth(server: &str, storage: &StateDir) -> Result<()> {
    smol::block_on(async {
        let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
        let (config, _) = mcp_config::load_config(&cwd);
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

pub fn prompt(
    variant: &crate::cli::PromptVariant,
    plan: bool,
    tools: bool,
    names: bool,
) -> Result<()> {
    use crate::cli::PromptVariant;
    use maki_agent::agent::{build_system_prompt, load_instruction_text};
    use maki_agent::prompt::{PromptId, assemble};
    use maki_agent::template;
    use maki_agent::tools::{DescriptionContext, ToolAudience, ToolFilter, ToolRegistry};
    use maki_providers::Model;

    if plan && !matches!(variant, PromptVariant::System) {
        bail!("--plan can only be used with the 'system' prompt variant");
    }

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let vars = template::env_vars();
    let reg = ToolRegistry::native_arc();
    let mut host = PluginHost::new(Arc::clone(reg)).context("initialize lua plugin host")?;
    let raw_config = host.load_init_files(&cwd).context("load init.lua files")?;
    let config = raw_config
        .unwrap_or_default()
        .into_config(false)
        .context("invalid config")?;
    host.load_builtins(&config.plugins)
        .context("load builtin plugins")?;

    if tools {
        let ctx = DescriptionContext {
            filter: &ToolFilter::All,
            audience: ToolAudience::MAIN,
            workflow: false,
        };
        let defs = reg.definitions(&vars, &ctx, true);
        if names {
            for name in defs
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d["name"].as_str())
            {
                println!("{name}");
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&defs)?);
        }
        return Ok(());
    }

    let cwd_str = cwd.to_string_lossy();
    let instructions = load_instruction_text(&cwd_str);
    let slots = host
        .event_handle()
        .map(|h| h.collect_prompt_slots())
        .unwrap_or_default();

    let output = match variant {
        PromptVariant::System => {
            let mode = if plan {
                maki_agent::AgentMode::Plan(std::path::PathBuf::from("plan.md"))
            } else {
                maki_agent::AgentMode::Build
            };
            let model_spec = config
                .provider
                .default_model
                .as_deref()
                .unwrap_or("anthropic/claude-sonnet-4-20250514");
            let model = Model::from_spec(model_spec).context("invalid default model")?;
            build_system_prompt(&vars, &mode, &instructions, &slots, &model)
        }
        PromptVariant::Research => assemble(PromptId::Research, &slots, &instructions),
        PromptVariant::General => assemble(PromptId::General, &slots, &instructions),
    };

    print!("{output}");
    Ok(())
}
