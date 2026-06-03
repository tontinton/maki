use std::path::PathBuf;
use std::sync::Arc;

use flume::Receiver;
use serde_json::Value;
use tracing::error;

use crate::agent::{self, History};
use crate::permissions::PermissionManager;
use crate::prompt::ResolvedSlots;
use crate::template;
use crate::tools::{DescriptionContext, FileReadTracker, ToolFilter, ToolRegistry};
use crate::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, Envelope,
    EventSender, McpHandle, PermissionsConfig, ToolOutputLines,
};
use maki_providers::Timeouts;
use maki_providers::model::Model;
use maki_providers::provider::{self, Provider};

pub struct HeadlessParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt: String,
    pub prompt_slots: ResolvedSlots,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub fast: bool,
}

pub struct HeadlessHandle {
    pub event_rx: Receiver<Envelope>,
    pub tool_names: Vec<String>,
    pub session_id: String,
    pub cwd: String,
    pub task: smol::Task<()>,
}

pub fn spawn(params: HeadlessParams) -> HeadlessHandle {
    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let vars = template::env_vars();
    let mode = AgentMode::Build;
    let instructions = agent::load_instructions(&vars.apply("{cwd}"));

    let filter = ToolFilter::from_config(&params.config, &params.excluded_tools);
    let ctx = DescriptionContext { filter: &filter };
    let mut tools =
        ToolRegistry::native().definitions(&vars, &ctx, params.model.supports_tool_examples());

    if let Some(handle) = &params.mcp_handle {
        handle.extend_tools(&mut tools);
    }

    let system = agent::build_system_prompt(&vars, &mode, &instructions.text, &params.prompt_slots);

    let tool_names = extract_tool_names(&tools);

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();

    let session_id = uuid::Uuid::new_v4().to_string();

    let fast = params.fast;
    let task = smol::spawn({
        let session_id = session_id.clone();
        let mcp_shutdown = params.mcp_handle.clone();
        let working_dir_path = params.initial_wd.clone();
        async move {
            let event_tx = EventSender::new(raw_tx, 0);
            let provider: Arc<dyn Provider> =
                match provider::from_model_async(&params.model, params.timeouts).await {
                    Ok(p) => Arc::from(p),
                    Err(e) => {
                        error!(error = %e, "provider error");
                        let _ = event_tx.send(AgentEvent::Error {
                            message: e.user_message(),
                        });
                        return;
                    }
                };
            let error_tx = event_tx.clone();
            let agent = Agent::new(
                AgentParams {
                    provider,
                    model: params.model,
                    config: params.config,
                    tool_output_lines: ToolOutputLines::default(),
                    permissions: Arc::new(PermissionManager::new(
                        params.permissions_config,
                        working_dir_path,
                    )),
                    session_id: Some(session_id),
                    timeouts: params.timeouts,
                    file_tracker: FileReadTracker::fresh(),
                    prompt_slots: Arc::new(params.prompt_slots),
                },
                AgentRunParams {
                    history: History::new(Vec::new()),
                    system,
                    event_tx,
                    tools,
                },
            )
            .with_loaded_instructions(instructions.loaded)
            .with_mcp(params.mcp_handle);

            let outcome = agent
                .run(AgentInput {
                    message: params.prompt,
                    mode,
                    fast,
                    ..Default::default()
                })
                .await;

            if let Err(e) = outcome.result {
                error!(error = %e, "agent error");
                let _ = error_tx.send(AgentEvent::Error {
                    message: e.user_message(),
                });
            }

            if let Some(handle) = mcp_shutdown {
                handle.shutdown().await;
            }
        }
    });

    HeadlessHandle {
        event_rx,
        tool_names,
        session_id,
        cwd: working_dir,
        task,
    }
}

fn extract_tool_names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}
