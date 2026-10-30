use std::path::PathBuf;

use color_eyre::eyre::{Context, Result};
use maki_cedar::{DefaultModel, PermissionsModel, RuleModel, compile};
use maki_config::{
    DefaultEffect, Effect, PermissionsConfig, ProjectConfig, ToolKey, load_permissions,
};

use crate::cli::PermissionsAction;

const SCHEMA_EXT: &str = "cedarschema";

pub fn run(action: PermissionsAction) -> Result<()> {
    match action {
        PermissionsAction::ExportCedar { out } => export_cedar(out),
    }
}

fn export_cedar(out: Option<PathBuf>) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let project = ProjectConfig::discover(&cwd);
    let config = load_permissions(&project);
    let model = to_model(&config);
    let (policies, schema) = compile(&model).context("compile permissions to Cedar")?;

    match out {
        None => {
            print!("{policies}");
        }
        Some(path) => {
            std::fs::write(&path, &policies)
                .with_context(|| format!("write policies to {}", path.display()))?;
            let schema_path = path.with_extension(SCHEMA_EXT);
            std::fs::write(&schema_path, &schema)
                .with_context(|| format!("write schema to {}", schema_path.display()))?;
            println!(
                "wrote {} policies and {} schema",
                path.display(),
                schema_path.display()
            );
        }
    }
    Ok(())
}

/// Flatten maki's merged permission config into the engine-agnostic model the
/// Cedar compiler consumes. Tool keys map by structure, not Display, so an MCP
/// server wildcard keeps its bare name for the compiler's prefix match.
fn to_model(config: &PermissionsConfig) -> PermissionsModel {
    let rules = config
        .rules
        .iter()
        .map(|r| {
            let (tool_key, server_wildcard) = match &r.tool {
                ToolKey::Wildcard => ("*".to_string(), false),
                ToolKey::Native(name) => (name.to_string(), false),
                ToolKey::McpServer { server } => (server.to_string(), true),
                ToolKey::McpTool { server, tool } => (format!("{server}.{tool}"), false),
            };
            RuleModel {
                tool_key,
                server_wildcard,
                scope: r.scope.clone(),
                allow: r.effect == Effect::Allow,
            }
        })
        .collect();

    PermissionsModel {
        default: match config.default {
            DefaultEffect::Allow => DefaultModel::Allow,
            DefaultEffect::Deny => DefaultModel::Deny,
            DefaultEffect::Prompt => DefaultModel::Prompt,
        },
        rules,
    }
}
