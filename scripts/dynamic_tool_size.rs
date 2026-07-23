use std::sync::Arc;

use maki_agent::template::Vars;
use maki_agent::tools::{DescriptionContext, ToolAudience, ToolFilter, ToolRegistry};
use maki_providers::Model;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = ToolRegistry::global_arc();
    let _host = maki_lua::PluginHost::with_all_builtins(Arc::clone(registry))?;

    let vars = Vars::new();
    let model = Model::from_spec("anthropic/claude-sonnet-4-6")?;

    let filter = ToolFilter::All;
    let ctx = DescriptionContext {
        filter: &filter,
        audience: ToolAudience::MAIN,
        workflow: false,
    };

    let modes = ["default", "research", "build", "compact"];

    println!("Tool definition size by mode:");
    println!("{:<15} {:<15} {:<15}", "Mode", "Tool Count", "Bytes");
    println!("{}", "-".repeat(45));

    for mode in &modes {
        let allowed = registry.active_tools_for_mode(mode, &[]);
        let defs =
            registry.definitions_filtered(&vars, &ctx, model.supports_tool_examples(), &allowed);
        let bytes = serde_json::to_vec(&defs)?.len();
        let count = allowed.len();

        println!("{:<15} {:<15} {:<15}", mode, count, bytes);
    }

    let all_defs = registry.definitions(&vars, &ctx, model.supports_tool_examples());
    let all_bytes = serde_json::to_vec(&all_defs)?.len();
    let all_count = registry.names().len();

    println!(
        "{:<15} {:<15} {:<15}",
        "all (unfiltered)", all_count, all_bytes
    );

    Ok(())
}
