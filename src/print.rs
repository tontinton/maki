//! Non-interactive (headless) mode: `maki "prompt" --print`.
//!
//! Wire format intentionally matches Claude Code so existing scripts work
//! unchanged. Keep `PrintResult` fields a strict subset of theirs. `StreamJson`
//! is JSONL with the same shape, `Text` prints the raw response only.
//!
//! We adopt new fields when Claude Code adds them but never invent our own.
//! Check their docs before changing anything here.

use std::env;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use color_eyre::Result;
use color_eyre::eyre::Context;
use maki_agent::mcp;
use maki_agent::skill::Skill;
use maki_agent::tools::{
    DescriptionContext, FileReadTracker, QUESTION_TOOL_NAME, ToolFilter, ToolRegistry,
};
use maki_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, Envelope,
    EventSender, History, PermissionsConfig, agent, template,
};
use maki_providers::StopReason;
use maki_providers::TokenUsage;
use maki_providers::model::Model;
use maki_providers::provider::{self, Provider};
use serde::Serialize;
use serde_json::Value;
use tracing::error;
use uuid::Uuid;

const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
    stop_reason: Option<StopReason>,
    session_id: String,
    total_cost_usd: f64,
    usage: TokenUsage,
}

#[derive(Serialize)]
struct InitEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    cwd: &'a str,
    session_id: &'a str,
    tools: &'a [String],
    model: &'a str,
}

#[derive(Serialize)]
struct AssistantEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    message: AssistantMessage<'a>,
    session_id: &'a str,
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
    session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'static str,
    content: &'a Value,
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
    format: OutputFormat,
    verbose: bool,
    skills: Vec<Skill>,
    config: AgentConfig,
    permissions_config: PermissionsConfig,
    timeouts: maki_providers::Timeouts,
) -> Result<()> {
    let prompt = match prompt_arg {
        Some(p) => p,
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).context("read stdin")?;
            buf
        }
    };

    let cwd_path = env::current_dir().unwrap_or_else(|_| ".".into());
    let cwd = cwd_path.to_string_lossy().into_owned();
    let vars = template::env_vars();
    let mode = AgentMode::Build;
    let instructions = agent::load_instructions(&vars.apply("{cwd}"));
    let filter = ToolFilter::from_config(&config, &[QUESTION_TOOL_NAME]);
    let ctx = DescriptionContext {
        skills: &skills,
        filter: &filter,
    };
    let mut tools = ToolRegistry::native().definitions(&vars, &ctx, model.supports_tool_examples());

    let mcp_handle = smol::block_on(mcp::start(&cwd_path));

    if let Some(ref handle) = mcp_handle {
        handle.extend_tools(&mut tools);
    }

    let system = agent::build_system_prompt(&vars, &mode, &instructions.text);

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();
    let input = AgentInput {
        message: prompt,
        mode,
        ..Default::default()
    };

    let session_id = Uuid::new_v4().to_string();
    let start = Instant::now();

    let tool_names: Vec<String> = tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let model_clone = model.clone();
    let session_id_clone = session_id.clone();
    let agent_task = smol::spawn(async move {
        let event_tx = EventSender::new(raw_tx, 0);
        let provider: Arc<dyn Provider> =
            match provider::from_model_async(&model_clone, timeouts).await {
                Ok(p) => Arc::from(p),
                Err(e) => {
                    error!(error = %e, "provider error");
                    let _ = event_tx.send(AgentEvent::Error {
                        message: e.user_message(),
                    });
                    return;
                }
            };
        let skills: Arc<[Skill]> = Arc::from(skills);
        let error_tx = event_tx.clone();
        let agent = Agent::new(
            AgentParams {
                provider,
                model: model_clone,
                skills,
                config,
                permissions: Arc::new(maki_agent::permissions::PermissionManager::new(
                    permissions_config,
                    cwd_path.clone(),
                )),
                session_id: Some(session_id_clone),
                timeouts,
                file_tracker: FileReadTracker::fresh(),
            },
            AgentRunParams {
                history: History::new(Vec::new()),
                system,
                event_tx,
                tools,
            },
        )
        .with_loaded_instructions(instructions.loaded)
        .with_mcp(mcp_handle);
        let outcome = agent.run(input).await;
        if let Err(e) = outcome.result {
            error!(error = %e, "agent error");
            let _ = error_tx.send(AgentEvent::Error {
                message: e.user_message(),
            });
        }
    });

    let is_stream_json = matches!(format, OutputFormat::StreamJson);
    let mut verbose_out = verbose.then(|| match format {
        OutputFormat::StreamJson => VerboseOutput::StreamJson,
        _ => VerboseOutput::Json(Vec::new()),
    });

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
    let mut stop_reason: Option<StopReason> = None;

    while let Ok(envelope) = smol::block_on(event_rx.recv_async()) {
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
            | AgentEvent::BatchProgress(_)
            | AgentEvent::QueueItemConsumed { .. }
            | AgentEvent::AutoCompacting
            | AgentEvent::AuthRequired
            | AgentEvent::PermissionRequest { .. }
            | AgentEvent::QuestionPrompt { .. }
            | AgentEvent::SubagentHistory { .. }
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolAnnotation { .. }
            | AgentEvent::Retry { .. } => {
                if is_stream_json {
                    println!("{}", serde_json::to_string(&envelope)?);
                }
            }
            AgentEvent::TurnComplete(tc) => {
                if is_stream_json {
                    println!("{}", serde_json::to_string(&envelope)?);
                }
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
                if is_stream_json {
                    println!("{}", serde_json::to_string(&envelope)?);
                }
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
                stop_reason: sr,
            } => {
                if is_stream_json {
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                num_turns = *turns;
                usage = *u;
                stop_reason = *sr;
                break;
            }
            AgentEvent::Error { message } => {
                if is_stream_json {
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                is_error = true;
                result_text = message.clone();
                break;
            }
        }
    }
    smol::block_on(async {
        futures_lite::future::or(
            async {
                agent_task.await;
            },
            async {
                smol::Timer::after(AGENT_SHUTDOWN_TIMEOUT).await;
            },
        )
        .await;
    });

    let duration_ms = start.elapsed().as_millis();
    let total_cost_usd = usage.cost(&model.pricing);

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
                Some(VerboseOutput::StreamJson) => println!("{}", serde_json::to_string(&result)?),
                None if is_stream_json => {}
                None => println!("{}", serde_json::to_string(&result)?),
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

    #[test]
    fn wire_format_required_fields() {
        let result = PrintResult {
            result_type: "result",
            subtype: "success",
            is_error: false,
            duration_ms: 1234,
            num_turns: 2,
            result: "done".into(),
            stop_reason: Some(StopReason::EndTurn),
            session_id: "sess-123".into(),
            total_cost_usd: 0.003,
            usage: TokenUsage::default(),
        };
        let json: Value = serde_json::to_value(&result).unwrap();
        for field in PRINT_RESULT_FIELDS {
            assert!(json.get(field).is_some(), "PrintResult missing: {field}");
        }

        let init = InitEvent {
            event_type: "system",
            subtype: "init",
            cwd: "/tmp",
            session_id: "abc",
            tools: &["bash".into(), "read".into()],
            model: "test-model",
        };
        let json: Value = serde_json::to_value(&init).unwrap();
        for field in INIT_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "InitEvent missing: {field}");
        }
    }
}
