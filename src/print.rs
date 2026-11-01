//! Non-interactive (headless) mode: `maki "prompt" --print`.
//!
//! Wire format intentionally matches Claude Code so existing scripts work
//! unchanged. Keep `PrintResult` fields a strict subset of theirs. `StreamJson`
//! is JSONL with the same shape, `Text` prints the raw response only.
//!
//! We adopt new fields when Claude Code adds them but never invent our own.
//! Check their docs before changing anything here.

use std::io::{self, Read};
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};
use maki_agent::headless::{self, BackgroundMode, InteractiveHandle, InteractiveParams};
use maki_agent::permissions::PluginRuleStore;
use maki_agent::session::Resumed;
use maki_agent::tools::QUESTION_TOOL_NAME;
use maki_agent::{
    AgentConfig, AgentEvent, AgentInput, AgentMode, DoneReason, Envelope, ImageSource, InputSource,
    PermissionsConfig, SteerKind,
};
use maki_config::{ModelPolicy, ProjectConfig, SessionDefaults};
use maki_lua::session_snapshot::{HeadlessMeta, HeadlessSnapshot, MODE_BUILD};
use maki_lua::{EventHandle, SessionEndReason};
use maki_providers::TokenUsage;
use maki_providers::model::Model;
use maki_storage::StateDir;
use maki_storage::id::SessionRef;
use maki_storage::sessions::SessionClaim;
use serde::Serialize;
use serde_json::Value;

use crate::interrupt::Interrupt;
use crate::sdk_mode::NOTICE_SUBTYPE;

const SESSION_LINE_PREFIX: &str = "session: ";

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

/// Claude Code has no event for this, so it takes the SDK's shape. Without it
/// a woken turn's assistant messages follow the last turn's with nothing
/// between them that says what woke it.
#[derive(Serialize)]
struct NoticeEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    text: &'a str,
    session_id: &'a SessionRef,
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

/// What the result reports besides the spend. A turn woken by a background
/// result belongs to the same invocation, so turns add up over every run,
/// and the answer is the last one a run gave. A woken run that says nothing
/// keeps the answer before it.
#[derive(Default)]
struct Totals {
    result_text: String,
    /// The answer of the run still streaming.
    run_text: String,
    is_error: bool,
    num_turns: u32,
    stop_reason: Option<DoneReason>,
}

impl Totals {
    /// `false` once the invocation failed, which ends it.
    fn fold(&mut self, event: &AgentEvent, parent_tool_use_id: Option<&str>) -> bool {
        match event {
            AgentEvent::TextDelta { text } if parent_tool_use_id.is_none() => {
                self.run_text.push_str(text);
            }
            AgentEvent::Steered { .. } => {
                if let Some(reason) = dropped_prompt(event, parent_tool_use_id) {
                    self.is_error = true;
                    self.run_text = reason.to_owned();
                }
            }
            AgentEvent::Done {
                num_turns, reason, ..
            } => {
                self.num_turns += *num_turns;
                self.stop_reason = Some(*reason);
                // Only a stop cancels a run here, and its answer is cut short.
                self.is_error |= *reason == DoneReason::Cancelled;
                let run_text = mem::take(&mut self.run_text);
                if !run_text.is_empty() {
                    self.result_text = run_text;
                }
            }
            AgentEvent::Error { message, .. } => {
                self.is_error = true;
                self.result_text = message.clone();
                return false;
            }
            _ => {}
        }
        true
    }
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

pub struct PrintParams {
    pub model: Model,
    pub prompt: Option<String>,
    pub image_paths: Vec<PathBuf>,
    pub format: OutputFormat,
    pub verbose: bool,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: maki_providers::Timeouts,
    pub lua_handle: EventHandle,
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub project_config: ProjectConfig,
    /// Which session this run continues and writes under, and where. Resolved
    /// by the caller from the same flags every other entry point reads.
    pub resumed: Resumed,
    /// The right to write [`Self::resumed`]'s session, taken when it was
    /// resolved and held for the whole run.
    pub claim: SessionClaim,
    pub storage: StateDir,
    /// How long to wait for background work once the prompt's run ends.
    /// `None` runs every call blocking instead.
    pub background_wait: Option<Duration>,
}

/// Hands back the interrupt it ran under, for the caller to exit with once
/// the plugin host is down.
pub async fn run(params: PrintParams) -> Result<Interrupt> {
    let PrintParams {
        model,
        prompt,
        image_paths,
        format,
        verbose,
        config,
        permissions_config,
        timeouts,
        lua_handle,
        defaults,
        model_policy,
        plugin_rules,
        project_config,
        resumed,
        claim,
        storage,
        background_wait,
    } = params;

    let prompt = match prompt {
        Some(p) => p,
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).context("read stdin")?;
            buf
        }
    };

    let images = load_images(&image_paths)?;
    // After the prompt is read, so a signal still ends a stdin read at once.
    let interrupt = Interrupt::install();

    let prompt_slots = lua_handle.collect_prompt_slots();

    let initial_wd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let cwd = initial_wd.to_string_lossy().into_owned();
    let (mcp_handle, mcp_config_errors) =
        maki_agent::mcp::start_connected(&initial_wd, project_config.clone()).await;
    if !mcp_config_errors.is_empty() {
        eprintln!("MCP config error: {mcp_config_errors}");
    }

    let (handle, mut events) = headless::spawn_interactive(InteractiveParams {
        model: model.clone(),
        config,
        permissions_config,
        timeouts,
        prompt_slots: Arc::new(prompt_slots),
        excluded_tools: vec![QUESTION_TOOL_NAME],
        mcp_handle,
        initial_wd,
        resumed,
        claim,
        storage,
        yolo: false,
        unattended: true,
        system_prompt_override: None,
        append_system_prompt: None,
        defaults,
        model_policy,
        plugin_rules,
        project_config,
        local_tools: Default::default(),
        background: BackgroundMode::from_wait(background_wait),
        stop: interrupt.stop(),
    });

    let InteractiveHandle {
        tool_names,
        input_tx,
        session_id,
        task,
        spend,
        ..
    } = handle;
    // One prompt, then the input ends. A session that never connected has
    // already said why on the event stream.
    let _ = input_tx.send(AgentInput::from_defaults(
        prompt,
        AgentMode::Build,
        images,
        defaults,
        InputSource::Headless,
    ));
    drop(input_tx);
    let start = Instant::now();

    let mut verbose_out = match format {
        OutputFormat::StreamJson => Some(VerboseOutput::StreamJson),
        _ if verbose => Some(VerboseOutput::Json(Vec::new())),
        _ => None,
    };

    match &mut verbose_out {
        Some(out) => out.emit(&InitEvent {
            event_type: "system",
            subtype: "init",
            cwd: &cwd,
            session_id: &session_id,
            tools: &tool_names,
            model: &model.id,
        })?,
        // Text mode never says which session it wrote, and a later `-r`
        // needs it. On stderr so a pipe reading the answer is unaffected.
        None => eprintln!("{SESSION_LINE_PREFIX}{session_id}"),
    }

    let mut totals = Totals::default();

    let snapshot = HeadlessSnapshot::default();
    snapshot.install(
        &lua_handle,
        HeadlessMeta {
            id: session_id.to_string(),
            cwd: cwd.clone(),
            model: model.spec(),
        },
        // `maki -p` sends its one prompt in build mode and never switches.
        || MODE_BUILD,
    );

    while let Some(envelope) = events.next().await {
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

        if !totals.fold(event, parent_tool_use_id) {
            break;
        }
        match event {
            AgentEvent::TextDelta { .. }
            | AgentEvent::ThinkingDelta { .. }
            | AgentEvent::ToolPending { .. }
            | AgentEvent::ToolStart(_)
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolDone(_)
            | AgentEvent::ToolMovable { .. }
            | AgentEvent::ToolParts { .. }
            | AgentEvent::QueueItemConsumed { .. }
            | AgentEvent::QueueDrained
            | AgentEvent::AutoCompacting { .. }
            | AgentEvent::CompactionDone { .. }
            | AgentEvent::AuthRequired
            | AgentEvent::AllowedByPlugin { .. }
            | AgentEvent::PermissionRequest { .. }
            | AgentEvent::SubagentHistory { .. }
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. }
            | AgentEvent::Nudge
            | AgentEvent::Notice { .. }
            | AgentEvent::PromptProgress { .. }
            | AgentEvent::StreamClosed
            | AgentEvent::Steered { .. }
            | AgentEvent::Done { .. }
            | AgentEvent::Error { .. } => {}
            AgentEvent::ObservationLine { text, .. } => {
                if let Some(out) = verbose_out
                    .as_mut()
                    .filter(|_| parent_tool_use_id.is_none())
                {
                    out.emit(&NoticeEvent {
                        event_type: "system",
                        subtype: NOTICE_SUBTYPE,
                        text,
                        session_id: &session_id,
                    })?;
                }
            }
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
        }
    }
    let track = headless::await_shutdown(task).await;
    lua_handle.end_sessions_blocking([session_id.id()], SessionEndReason::Completed);
    // Background work and plugin jobs are stopped by now, so nothing spends
    // any more. Dropping the track saves the full bill, also what no turn
    // was left to report.
    drop(track);
    let spent = spend.totals();

    let duration_ms = start.elapsed().as_millis();
    let Totals {
        result_text,
        is_error,
        num_turns,
        stop_reason,
        ..
    } = totals;
    // A stop before the session read the prompt runs nothing at all.
    let is_error = is_error || stop_reason.is_none();
    // Zero on an unpriced model, which is what its turns reported too.
    let total_cost_usd = spent.cost.unwrap_or_default();

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
                usage: spent.usage,
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

    Ok(interrupt)
}

/// Nothing reached the model, so the reason is the only answer there is. A
/// script checking the exit status should see that the prompt never ran.
fn dropped_prompt<'a>(event: &'a AgentEvent, parent_tool_use_id: Option<&str>) -> Option<&'a str> {
    match event {
        AgentEvent::Steered {
            kind: SteerKind::MessageDropped,
            text,
        } if parent_tool_use_id.is_none() => Some(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::RunTotals;
    use maki_providers::TokenUsage;
    use test_case::test_case;

    const DROP_REASON: &str = "blocked by a plugin";
    const PARENT_TOOL_USE_ID: &str = "toolu_parent";

    #[test_case(SteerKind::MessageDropped, None, Some(DROP_REASON) ; "top_level_drop_is_the_result")]
    #[test_case(SteerKind::MessageDropped, Some(PARENT_TOOL_USE_ID), None ; "subagent_drop_is_ignored")]
    #[test_case(SteerKind::MessageRewritten, None, None ; "other_steers_are_ignored")]
    fn dropped_prompt_only_counts_a_top_level_drop(
        kind: SteerKind,
        parent: Option<&str>,
        expected: Option<&str>,
    ) {
        let event = AgentEvent::Steered {
            kind,
            text: DROP_REASON.into(),
        };
        assert_eq!(dropped_prompt(&event, parent), expected);
    }

    const FIRST_ANSWER: &str = "started the build";
    const WOKEN_ANSWER: &str = "the build passed";
    const SUBAGENT_TEXT: &str = "child chatter";
    const ERROR_MESSAGE: &str = "provider error";
    const RUN_TURNS: u32 = 2;
    const RUNS: u32 = 2;

    fn text_delta(text: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: text.into() }
    }

    fn run_done(reason: DoneReason) -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            cost: None,
            list_cost: None,
            context_size: 0,
            context_window: 0,
            num_turns: RUN_TURNS,
            reason,
        }
    }

    #[test]
    fn a_run_cut_short_by_a_stop_is_an_error() {
        let mut totals = Totals::default();
        assert!(totals.fold(&text_delta(FIRST_ANSWER), None));
        assert!(totals.fold(&run_done(DoneReason::Cancelled), None));

        assert!(totals.is_error);
        assert_eq!(totals.result_text, FIRST_ANSWER);
    }

    /// A woken turn is part of the same invocation: its turns add up, and its
    /// answer, if it gave one, is the one the caller gets.
    #[test_case(WOKEN_ANSWER, WOKEN_ANSWER ; "woken_answer_replaces_the_first")]
    #[test_case("", FIRST_ANSWER ; "silent_woken_run_keeps_the_first")]
    fn totals_add_up_every_run_and_answer_with_the_last_text(woken: &str, expected: &str) {
        let mut totals = Totals::default();
        for answer in [FIRST_ANSWER, woken] {
            assert!(totals.fold(&text_delta(answer), None));
            assert!(totals.fold(&text_delta(SUBAGENT_TEXT), Some(PARENT_TOOL_USE_ID)));
            assert!(totals.fold(&run_done(DoneReason::EndTurn), None));
        }

        assert_eq!(totals.result_text, expected);
        assert_eq!(totals.num_turns, RUN_TURNS * RUNS);
        assert_eq!(totals.stop_reason, Some(DoneReason::EndTurn));
        assert!(!totals.is_error);
    }

    #[test]
    fn an_error_ends_the_invocation_with_its_message() {
        let mut totals = Totals::default();
        assert!(totals.fold(&text_delta(FIRST_ANSWER), None));

        let error = AgentEvent::Error {
            message: ERROR_MESSAGE.into(),
            auth: false,
            totals: RunTotals::default(),
        };
        assert!(!totals.fold(&error, None));

        assert!(totals.is_error);
        assert_eq!(totals.result_text, ERROR_MESSAGE);
    }

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
    const NOTICE_EVENT_FIELDS: &[&str] = &["type", "subtype", "text", "session_id"];
    const NOTICE_TEXT: &str = "Background bash finished: cargo test";
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

        let notice = NoticeEvent {
            event_type: "system",
            subtype: NOTICE_SUBTYPE,
            text: NOTICE_TEXT,
            session_id: &sid,
        };
        let json: Value = serde_json::to_value(&notice).unwrap();
        for field in NOTICE_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "NoticeEvent missing: {field}");
        }
    }
}
