//! Non-interactive (headless) mode: `maki "prompt" --print`.
//!
//! Wire format intentionally matches Claude Code so existing scripts work
//! unchanged. Keep `PrintResult` fields a strict subset of theirs. `StreamJson`
//! is JSONL with the same shape, `Text` prints the raw response only.
//!
//! We adopt new fields when Claude Code adds them but never invent our own.
//! Check their docs before changing anything here.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use maki_agent::headless::{HeadlessHandle, HeadlessParams};
use maki_agent::permissions::PluginRuleStore;
use maki_agent::tools::QUESTION_TOOL_NAME;
use maki_agent::{AgentConfig, AgentEvent, DoneReason, Envelope, ImageSource, PermissionsConfig};
use maki_config::ModelPolicy;
use maki_lua::session_snapshot::{HeadlessMeta, HeadlessSnapshot, MODE_BUILD};
use maki_lua::{EventHandle, SessionEndReason};
use maki_providers::model::Model;
use maki_providers::{TokenUsage, add_cost};
use maki_storage::id::SessionRef;
use serde::Serialize;
use serde_json::Value;

const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

// Fails fast: silently dropping an image the caller explicitly attached
// would be worse than erroring.
fn load_images(paths: &[PathBuf]) -> Result<Vec<ImageSource>> {
    paths
        .iter()
        .map(|path| {
            let media_type = maki_ui::image::media_type_for(path)
                .ok_or_else(|| eyre!("unsupported image type: {}", path.display()))?;
            maki_ui::image::load_file_image(path, media_type)
                .map_err(|e| eyre!("failed to load image: {e}"))
        })
        .collect()
}

#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Serialize)]
struct PrintResult {
    #[serde(rename = "type")]
    result_type: &'static str,
    subtype: &'static str,
    is_error: bool,
    duration_ms: u128,
    num_turns: u32,
    result: String,
    stop_reason: Option<DoneReason>,
    session_id: SessionRef,
    total_cost_usd: f64,
    usage: TokenUsage,
}

#[derive(Serialize)]
struct InitEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    cwd: &'a str,
    session_id: &'a SessionRef,
    tools: &'a [String],
    model: &'a str,
}

#[derive(Serialize)]
struct AssistantEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    message: AssistantMessage<'a>,
    session_id: &'a SessionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
}

#[derive(Serialize)]
struct AssistantMessage<'a> {
    model: &'a str,
    role: &'static str,
    content: &'a Value,
    usage: &'a TokenUsage,
}

#[derive(Serialize)]
struct UserEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    message: UserMessage<'a>,
    session_id: &'a SessionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'static str,
    content: &'a Value,
}

#[derive(Serialize)]
struct RetryEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    attempt: u32,
    retry_delay_ms: u64,
    error: &'a str,
    session_id: &'a SessionRef,
}

enum VerboseOutput {
    StreamJson,
    Json(Vec<Value>),
}

impl VerboseOutput {
    fn emit(&mut self, value: &impl Serialize) -> Result<()> {
        match self {
            Self::StreamJson => println!("{}", serde_json::to_string(value)?),
            Self::Json(events) => events.push(serde_json::to_value(value)?),
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    model: &Model,
    prompt_arg: Option<String>,
    image_paths: Vec<PathBuf>,
    format: OutputFormat,
    verbose: bool,
    config: AgentConfig,
    permissions_config: PermissionsConfig,
    timeouts: maki_providers::Timeouts,
    lua_handle: EventHandle,
    fast: bool,
    workflow: bool,
    model_policy: Arc<ModelPolicy>,
    plugin_rules: Arc<PluginRuleStore>,
) -> Result<()> {
    let prompt = match prompt_arg {
        Some(p) => p,
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).context("read stdin")?;
            buf
        }
    };

    let images = load_images(&image_paths)?;

    let prompt_slots = lua_handle.collect_prompt_slots();

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let (mcp_handle, mcp_config_errors) = smol::block_on(maki_agent::mcp::start_connected(&cwd));
    if !mcp_config_errors.is_empty() {
        eprintln!("MCP config error: {mcp_config_errors}");
    }

    let handle = maki_agent::headless::spawn(HeadlessParams {
        model: model.clone(),
        config,
        permissions_config,
        timeouts,
        prompt,
        images,
        prompt_slots,
        excluded_tools: vec![QUESTION_TOOL_NAME],
        mcp_handle,
        initial_wd: cwd,
        fast,
        workflow,
        model_policy,
        plugin_rules,
    });

    let HeadlessHandle {
        event_rx,
        tool_names,
        session_id,
        cwd,
        task,
    } = handle;
    crate::setup::report_session_start(maki_otel::emit::START_FRESH, Some(&session_id));
    let start = Instant::now();

    let mut verbose_out = match format {
        OutputFormat::StreamJson => Some(VerboseOutput::StreamJson),
        _ if verbose => Some(VerboseOutput::Json(Vec::new())),
        _ => None,
    };

    if let Some(out) = &mut verbose_out {
        out.emit(&InitEvent {
            event_type: "system",
            subtype: "init",
            cwd: &cwd,
            session_id: &session_id,
            tools: &tool_names,
            model: &model.id,
        })?;
    }

    let mut result_text = String::new();
    let mut is_error = false;
    let mut num_turns: u32 = 0;
    let mut usage = TokenUsage::default();
    // Summed as the turns land: rates move mid-run, and only a turn knows the
    // rate it paid.
    let mut cost = None;
    let mut stop_reason: Option<DoneReason> = None;

    let snapshot = HeadlessSnapshot::default();
    snapshot.install(
        &lua_handle,
        HeadlessMeta {
            id: session_id.to_string(),
            cwd: cwd.clone(),
            model: model.spec(),
        },
        // `maki -p` always runs the agent in build mode
        // (`headless::spawn` hardcodes `AgentMode::Build`).
        || MODE_BUILD,
    );

    while let Ok(envelope) = smol::block_on(event_rx.recv_async()) {
        // Folded in first, so a plugin handling `TurnEnd` finds the finished
        // totals when it calls `maki.session.read()`.
        snapshot.observe(&envelope);
        maki_lua::agent_autocmd::dispatch(
            &lua_handle,
            &session_id,
            &envelope,
            envelope.subagent.is_some(),
        );
        let Envelope {
            ref event,
            ref subagent,
            ..
        } = envelope;
        let parent_tool_use_id = subagent.as_ref().map(|s| s.parent_tool_use_id.as_str());

        match event {
            AgentEvent::TextDelta { text } => {
                if parent_tool_use_id.is_none() {
                    result_text.push_str(text);
                }
            }
            AgentEvent::ThinkingDelta { .. } => {}
            AgentEvent::ToolPending { .. }
            | AgentEvent::ToolStart(_)
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolDone(_)
            | AgentEvent::ReviewerVerdict(_)
            | AgentEvent::QueueItemConsumed { .. }
            | AgentEvent::QueueDrained
            | AgentEvent::AutoCompacting { .. }
            | AgentEvent::CompactionDone { .. }
            | AgentEvent::AuthRequired
            | AgentEvent::PermissionRequest { .. }
            | AgentEvent::SubagentHistory { .. }
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. }
            | AgentEvent::Nudge
            | AgentEvent::PromptProgress { .. } => {}
            AgentEvent::Retry {
                attempt,
                message,
                delay_ms,
            } => {
                if let Some(out) = &mut verbose_out {
                    out.emit(&RetryEvent {
                        event_type: "system",
                        subtype: "api_retry",
                        attempt: *attempt,
                        retry_delay_ms: *delay_ms,
                        error: message,
                        session_id: &session_id,
                    })?;
                }
            }
            AgentEvent::TurnComplete(tc) => {
                add_cost(&mut cost, tc.cost);
                if let Some(out) = &mut verbose_out {
                    let content_value = serde_json::to_value(&tc.message.content)?;
                    out.emit(&AssistantEvent {
                        event_type: "assistant",
                        message: AssistantMessage {
                            model: &tc.model,
                            role: "assistant",
                            content: &content_value,
                            usage: &tc.usage,
                        },
                        session_id: &session_id,
                        parent_tool_use_id,
                    })?;
                }
            }
            AgentEvent::ToolResultsSubmitted { message } => {
                if let Some(out) = &mut verbose_out {
                    let content_value = serde_json::to_value(&message.content)?;
                    out.emit(&UserEvent {
                        event_type: "user",
                        message: UserMessage {
                            role: "user",
                            content: &content_value,
                        },
                        session_id: &session_id,
                        parent_tool_use_id,
                    })?;
                }
            }
            AgentEvent::Done {
                usage: u,
                num_turns: turns,
                reason,
                cost: dcost,
                ..
            } => {
                num_turns = *turns;
                usage = *u;
                // The agent's own ledger also counts compaction spend, which
                // summing the turns misses.
                cost = (*dcost).or(cost);
                stop_reason = Some(*reason);
                break;
            }
            AgentEvent::Error { message } => {
                is_error = true;
                result_text = message.clone();
                break;
            }
        }
    }
    smol::block_on(async {
        futures_lite::future::or(task, async {
            smol::Timer::after(AGENT_SHUTDOWN_TIMEOUT).await;
        })
        .await;
    });
    lua_handle.end_sessions_blocking([session_id.id()], SessionEndReason::Completed);

    let duration_ms = start.elapsed().as_millis();
    // Zero on an unpriced model, which is what its turns reported too.
    let total_cost_usd = cost.unwrap_or_default();

    match format {
        OutputFormat::Text => {
            print!("{result_text}");
        }
        OutputFormat::Json | OutputFormat::StreamJson => {
            let result = PrintResult {
                result_type: "result",
                subtype: if is_error { "error" } else { "success" },
                is_error,
                duration_ms,
                num_turns,
                result: result_text,
                stop_reason,
                session_id,
                total_cost_usd,
                usage,
            };
            match verbose_out {
                Some(VerboseOutput::Json(mut events)) => {
                    events.push(serde_json::to_value(&result)?);
                    println!("{}", serde_json::to_string(&events)?);
                }
                _ => println!("{}", serde_json::to_string(&result)?),
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_providers::TokenUsage;

    const PRINT_RESULT_FIELDS: &[&str] = &[
        "type",
        "subtype",
        "is_error",
        "num_turns",
        "result",
        "stop_reason",
        "session_id",
        "total_cost_usd",
        "usage",
        "duration_ms",
    ];
    const INIT_EVENT_FIELDS: &[&str] = &["type", "subtype", "cwd", "session_id", "tools", "model"];
    const RETRY_EVENT_FIELDS: &[&str] = &[
        "type",
        "subtype",
        "attempt",
        "retry_delay_ms",
        "error",
        "session_id",
    ];

    #[test]
    fn wire_format_required_fields() {
        let result = PrintResult {
            result_type: "result",
            subtype: "success",
            is_error: false,
            duration_ms: 1234,
            num_turns: 2,
            result: "done".into(),
            stop_reason: Some(DoneReason::EndTurn),
            session_id: SessionRef::generate(),
            total_cost_usd: 0.003,
            usage: TokenUsage::default(),
        };
        let json: Value = serde_json::to_value(&result).unwrap();
        for field in PRINT_RESULT_FIELDS {
            assert!(json.get(field).is_some(), "PrintResult missing: {field}");
        }

        let sid = SessionRef::generate();
        let init = InitEvent {
            event_type: "system",
            subtype: "init",
            cwd: "/tmp",
            session_id: &sid,
            tools: &["bash".into(), "read".into()],
            model: "test-model",
        };
        let json: Value = serde_json::to_value(&init).unwrap();
        for field in INIT_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "InitEvent missing: {field}");
        }

        let retry = RetryEvent {
            event_type: "system",
            subtype: "api_retry",
            attempt: 2,
            retry_delay_ms: 3000,
            error: "rate_limit",
            session_id: &sid,
        };
        let json: Value = serde_json::to_value(&retry).unwrap();
        for field in RETRY_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "RetryEvent missing: {field}");
        }
    }
}
