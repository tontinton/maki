//! Spawns a child agent (subagent) with a restricted tool set.
//!
//! The child's model tier is capped at the parent's tier, so a weak parent cannot spawn a strong child.
//! Events are forwarded to the parent with `SubagentInfo` attached; Done/Error/ToolOutput/ToolPending are filtered.
//! Child cancellation is linked to the parent via `cancel.child()`, so parent cancellation propagates.

use std::sync::Arc;
use std::time::Instant;

use crate::{AgentEvent, EventSender, SubagentInfo, ToolOutput};
use maki_config::ToolOutputLines;
use maki_providers::model::ModelTier;
use maki_providers::provider;
use maki_providers::{ContentBlock, Model, ModelError, Role};
use maki_tool_macro::Tool;
use serde::Deserialize;
use tracing::info;
use uuid::Uuid;

use super::{DescriptionContext, FileReadTracker, ToolContext, ToolFilter};
use crate::agent;
use crate::template;
use crate::tools::{ToolAudience, ToolRegistry};
use crate::{Agent, AgentInput, AgentMode, AgentParams, AgentRunParams};

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Task {
    #[param(description = "Short (3-5 words) description of the task")]
    description: String,
    #[param(description = "Detailed task prompt for the agent")]
    prompt: String,
    #[param(
        description = "Subagent type: \"research\" (read-only, default) or \"general\" (can modify files)"
    )]
    subagent_type: Option<String>,
    #[param(
        description = "Model tier (optional, omit to use current model, capped at current tier):\n- \"strong\" (e.g. Opus): Deep reasoning, complex architecture, subtle bugs, most critical sections. ~5x cost of medium.\n- \"medium\" (e.g. Sonnet): Balanced. Refactors, features, multi-file changes.\n- \"weak\" (e.g. Haiku): Fast/cheap. Search, summarize, boilerplate, simple edits."
    )]
    model_tier: Option<String>,
}

impl Task {
    pub const NAME: &str = "task";
    pub const DESCRIPTION: &str = include_str!("task.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"description": "Find auth middleware", "prompt": "Search the codebase for authentication middleware. Return file paths and a summary of how auth is implemented.", "model_tier": "weak"}]"#,
    );

    pub async fn execute(&self, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let vars = template::env_vars();
        let agent_type = self.subagent_type.as_deref().unwrap_or("research");
        let (prompt, audience) = match agent_type {
            "research" => (crate::prompt::RESEARCH_PROMPT, ToolAudience::RESEARCH_SUB),
            "general" => (crate::prompt::GENERAL_PROMPT, ToolAudience::GENERAL_SUB),
            other => return Err(format!("unknown subagent type: {other}")),
        };

        let (model, provider): (Model, Arc<dyn provider::Provider>) = if let Some(ref tier_str) =
            self.model_tier
        {
            let requested: ModelTier = tier_str.parse().map_err(|e: ModelError| e.to_string())?;
            let effective = requested.min(ctx.model.tier);
            if effective == ctx.model.tier {
                (Model::clone(&ctx.model), Arc::clone(&ctx.provider))
            } else {
                let resolved_model = Model::from_tier_dynamic(
                    ctx.model.provider,
                    effective,
                    ctx.model.dynamic_slug.as_deref(),
                )
                .map_err(|e| e.to_string())?;
                let resolved_provider = provider::from_model_async(&resolved_model, ctx.timeouts)
                    .await
                    .map_err(|e| e.to_string())?;
                (resolved_model, Arc::from(resolved_provider))
            }
        } else {
            (Model::clone(&ctx.model), Arc::clone(&ctx.provider))
        };

        info!(
            description = %self.description,
            subagent_type = agent_type,
            model = %model.id,
            "subagent spawning",
        );

        let mut system = vars.apply(prompt).into_owned();
        let cwd_owned = vars.apply("{cwd}").into_owned();
        let text = smol::unblock(move || agent::load_instruction_text(&cwd_owned)).await;
        system.push_str(&text);
        let snapshot = ToolRegistry::native().iter();
        let tool_names: Vec<String> = snapshot
            .iter()
            .filter(|e| {
                e.tool.audience().contains(audience)
                    && super::is_tool_enabled(&ctx.config, e.name())
            })
            .map(|e| e.name().to_owned())
            .collect();
        let filter = ToolFilter::Only(tool_names);
        let ctx_desc = DescriptionContext {
            skills: &[],
            filter: &filter,
        };
        let mut tools =
            ToolRegistry::native().definitions(&vars, &ctx_desc, model.supports_tool_examples());
        if let Some(ref mcp) = ctx.mcp {
            mcp.extend_tools(&mut tools);
        }

        let session_id = Uuid::new_v4().to_string();
        let (sub_tx, sub_rx) = flume::unbounded::<crate::Envelope>();
        let sub_event_tx = EventSender::new(sub_tx, ctx.event_tx.run_id());
        let parent_tx = ctx.event_tx.clone();
        let (answer_tx, answer_rx) = flume::unbounded::<String>();
        let answer_rx = Arc::new(async_lock::Mutex::new(answer_rx));
        let subagent_info = ctx.tool_use_id.as_ref().map(|id| SubagentInfo {
            parent_tool_use_id: id.to_owned(),
            name: self.description.clone(),
            prompt: Some(self.prompt.clone()),
            model: Some(model.spec()),
            answer_tx: Some(answer_tx),
        });
        smol::spawn(async move {
            while let Ok(mut envelope) = sub_rx.recv_async().await {
                if matches!(
                    envelope.event,
                    AgentEvent::Done { .. }
                        | AgentEvent::Error { .. }
                        | AgentEvent::ToolOutput { .. }
                        | AgentEvent::ToolPending { .. }
                        | AgentEvent::SubagentHistory { .. }
                ) {
                    continue;
                }
                envelope.subagent = subagent_info.clone();
                let _ = parent_tx.send_envelope(envelope);
            }
        })
        .detach();

        let (child_trigger, child_cancel) = ctx.cancel.child();
        let input = AgentInput {
            message: self.prompt.clone(),
            mode: AgentMode::Build,
            ..Default::default()
        };

        let agent = Agent::new(
            AgentParams {
                provider,
                model,
                skills: Arc::clone(&ctx.skills),
                config: ctx.config.clone(),
                tool_output_lines: ToolOutputLines::default(),
                permissions: Arc::clone(&ctx.permissions),
                session_id: Some(session_id),
                timeouts: ctx.timeouts,
                file_tracker: FileReadTracker::fresh(),
            },
            AgentRunParams {
                history: crate::History::new(Vec::new()),
                system,
                event_tx: sub_event_tx,
                tools,
            },
        )
        .with_user_response_rx(answer_rx)
        .with_cancel(child_cancel)
        .with_mcp(ctx.mcp.clone());
        let start = Instant::now();
        let outcome = agent.run(input).await;
        let duration_ms = start.elapsed().as_millis() as u64;
        drop(child_trigger);
        let success = outcome.result.is_ok();
        info!(description = %self.description, duration_ms, success, "subagent completed");
        outcome
            .result
            .map_err(|e| format!("sub-agent error: {e}"))?;

        let messages = outcome.history.into_vec();

        let text = messages
            .iter()
            .rev()
            .filter(|m| matches!(m.role, Role::Assistant))
            .flat_map(|m| m.content.iter())
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("(no response)")
            .to_string();

        if let Some(tool_use_id) = ctx.tool_use_id.clone() {
            let _ = ctx.event_tx.send(AgentEvent::SubagentHistory {
                tool_use_id,
                messages,
            });
        }

        Ok(ToolOutput::Plain(text))
    }

    pub fn start_header(&self) -> String {
        self.description.clone()
    }
}

super::impl_tool!(Task, audience = super::ToolAudience::MAIN);

impl super::ToolInvocation for Task {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Task::start_header(self)))
    }
    fn permission_scope(&self) -> Option<String> {
        Some(format!("task:{}", self.description))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Task::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The audience bitmask decides which agents can call each tool, so flipping a flag is
    /// a behavior change (letting `memory` into the interpreter, say, hands subagents a new
    /// power). To move a tool between audiences, change the tool file and this map together.
    #[test]
    fn audience_matrix_is_locked() {
        const MAIN: ToolAudience = ToolAudience::MAIN;
        const RES: ToolAudience = ToolAudience::RESEARCH_SUB;
        const GEN: ToolAudience = ToolAudience::GENERAL_SUB;
        const INT: ToolAudience = ToolAudience::INTERPRETER;
        let all = MAIN | RES | GEN | INT;

        let expected: BTreeMap<&str, ToolAudience> = BTreeMap::from([
            (super::super::BASH_TOOL_NAME, all),
            (super::super::READ_TOOL_NAME, all),
            (super::super::GLOB_TOOL_NAME, all),
            (super::super::GREP_TOOL_NAME, all),
            (super::super::WRITE_TOOL_NAME, MAIN | GEN | INT),
            (super::super::EDIT_TOOL_NAME, MAIN | GEN | INT),
            (super::super::MULTIEDIT_TOOL_NAME, MAIN | GEN | INT),
            (super::super::BATCH_TOOL_NAME, MAIN | RES | GEN),
            (super::super::CODE_EXECUTION_TOOL_NAME, MAIN | RES | GEN),
            (super::super::MEMORY_TOOL_NAME, MAIN | GEN),
            (super::super::QUESTION_TOOL_NAME, MAIN),
            (super::super::TODOWRITE_TOOL_NAME, MAIN),
            (super::super::SKILL_TOOL_NAME, MAIN),
            (super::super::TASK_TOOL_NAME, MAIN),
        ]);

        let snapshot = ToolRegistry::native().iter();
        let actual: BTreeMap<String, ToolAudience> = snapshot
            .iter()
            .map(|e| (e.name().to_owned(), e.tool.audience()))
            .collect();

        assert_eq!(
            actual.len(),
            expected.len(),
            "native tool count drift: expected {}, got {} ({:?})",
            expected.len(),
            actual.len(),
            actual.keys().collect::<Vec<_>>()
        );

        for (name, want) in &expected {
            let got = actual
                .get(*name)
                .unwrap_or_else(|| panic!("missing tool '{name}'"));
            assert_eq!(
                got.bits(),
                want.bits(),
                "audience drift for '{name}': expected {want:?}, got {got:?}"
            );
        }
    }
}
