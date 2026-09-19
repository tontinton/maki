use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_lock::Mutex;
use maki_config::{ModelPolicy, ProjectConfig, SessionDefaults};
use maki_providers::Timeouts;
use maki_providers::model::Model;
use maki_providers::provider::{self, Provider};
use maki_storage::StateDir;
use maki_storage::id::SessionRef;
use maki_storage::sessions::SessionClaim;
use serde_json::Value;
use tracing::error;

use crate::agent;
use crate::cancel::{CancelMap, CancelToken};
use crate::permissions::{PermissionManager, PluginRuleStore};
use crate::prompt::ResolvedSlots;
use crate::session::{Resumed, SessionTrack};
use crate::template;
use crate::tools::{FileAccess, LocalTools, RequestTools, ToolAudience, ToolRegistry};
use crate::types::EventSender;
use crate::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, EventStreamGuard,
    ImageSource, McpHandle, McpSession, PermissionsConfig, RunLedger, SessionEvents,
    SessionMailbox, ToolOutputLines, event_stream,
};

const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve a provider, or report the failure on the event stream and let the
/// caller give up. Every site that reaches a provider comes through here, so a
/// failed connect cannot be silent on one path and loud on another. What giving
/// up means is the caller's call: abandon the session, or skip this turn.
async fn connect(
    model: &mut Model,
    timeouts: Timeouts,
    event_tx: &EventSender,
) -> Option<Arc<dyn Provider>> {
    match provider::from_model_async(model, timeouts).await {
        Ok(p) => Some(Arc::from(p)),
        Err(e) => {
            error!(error = %e, "provider error");
            let _ = event_tx.send(AgentEvent::Error {
                message: e.user_message(),
            });
            None
        }
    }
}

pub struct HeadlessParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt: String,
    pub images: Vec<ImageSource>,
    pub prompt_slots: ResolvedSlots,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub resumed: Resumed,
    /// The right to write [`Self::resumed`]'s session, taken before its
    /// transcript was read.
    pub claim: SessionClaim,
    /// Where the transcript is written. The same dir the caller resolved the
    /// session from, so a run cannot read one session and write another.
    pub storage: StateDir,
    /// The `always_*` knobs. A headless run has no toggle UI, so config is the
    /// whole answer. The model gate stays in `RequestOptions::clamped`.
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub project_config: ProjectConfig,
}

pub struct HeadlessHandle {
    pub tool_names: Vec<String>,
    pub session_id: SessionRef,
    pub cwd: String,
    pub task: smol::Task<()>,
}

struct AgentSetup {
    vars: template::Vars,
    instructions: agent::Instructions,
    tools: RequestTools,
}

/// Takes the handle rather than a second `bool`: two adjacent flags is one
/// silent swap away from a session that describes tools it cannot call.
fn setup(
    model: &Model,
    config: &AgentConfig,
    excluded_tools: &[&'static str],
    workflow: bool,
    mcp: Option<&McpHandle>,
) -> AgentSetup {
    let vars = template::env_vars();
    let instructions = agent::load_instructions(&vars.apply("{cwd}"));
    let tools = RequestTools::build(
        ToolRegistry::global(),
        &vars,
        model,
        config,
        excluded_tools,
        workflow,
        mcp.is_some(),
    );

    AgentSetup {
        vars,
        instructions,
        tools,
    }
}

/// Names advertised to SDK clients: base tools plus what the first request
/// would carry from MCP (always-load definitions and `tool_search`).
fn advertised_tool_names(tools: &Value, mcp: Option<&McpSession>) -> Vec<String> {
    let mut probe = tools.clone();
    if let Some(mcp) = mcp {
        mcp.extend_tools(&mut probe);
    }
    extract_tool_names(&probe)
}

pub fn spawn(params: HeadlessParams) -> (HeadlessHandle, SessionEvents) {
    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let mode = AgentMode::Build;
    let AgentSetup {
        vars,
        instructions,
        tools,
    } = setup(
        &params.model,
        &params.config,
        &params.excluded_tools,
        params.defaults.workflow,
        params.mcp_handle.as_ref(),
    );

    let system = agent::build_system_prompt(
        &vars,
        &mode,
        &instructions.text,
        &params.prompt_slots,
        &params.model,
    );

    let mcp = params
        .mcp_handle
        .clone()
        .map(|h| McpSession::new(h, &params.resumed.history));
    let tool_names = advertised_tool_names(tools.definitions(), mcp.as_ref());

    let (guard, events) = event_stream();
    let event_tx = guard.sender(0);

    let session_ref = params.resumed.id.clone();
    let mailbox = SessionMailbox::register(session_ref.id());
    let run_session_ref = session_ref.clone();
    let defaults = params.defaults;
    let working_dir_path = params.initial_wd.clone();
    let task_working_dir = working_dir.clone();
    let task = smol::spawn(run_session(guard, params.mcp_handle.clone(), async move {
        let mut model = params.model;
        let Some(provider) = connect(&mut model, params.timeouts, &event_tx).await else {
            return;
        };
        let mut track = SessionTrack::open(
            params.resumed,
            params.claim,
            params.storage,
            &task_working_dir,
        );

        let error_tx = event_tx.clone();
        // Outlives `agent`, so dropping it writes the transcript once the agent
        // that produced it is gone, without this body saying so.
        let mut turn = track.turn(model.spec());
        let mut agent = Agent::new(
            AgentParams {
                provider,
                model: model.clone(),
                config: params.config,
                tool_output_lines: ToolOutputLines::default(),
                permissions: Arc::new(PermissionManager::new(
                    params.permissions_config,
                    working_dir_path,
                    params.project_config,
                    params.plugin_rules,
                )),
                session_id: Some(run_session_ref),
                task_id: None,
                mailbox: Some(mailbox),
                timeouts: params.timeouts,
                file_access: FileAccess::fresh(),
                prompt_slots: Arc::new(params.prompt_slots),
                subagent_cancels: Arc::new(CancelMap::new()),
                ledger: Arc::new(RunLedger::default()),
                registry: Arc::clone(ToolRegistry::global_arc()),
                audience: ToolAudience::MAIN,
                model_policy: Arc::clone(&params.model_policy),
            },
            turn.run_params(system, event_tx, tools),
        )
        .with_loaded_instructions(instructions.loaded)
        .with_mcp(mcp);

        let result = agent
            .run(AgentInput::from_defaults(
                params.prompt,
                mode,
                params.images,
                defaults,
            ))
            .await;
        drop(agent);

        if let Err(e) = result {
            error!(error = %e, "agent error");
            let _ = error_tx.send(AgentEvent::Error {
                message: e.user_message(),
            });
        }
    }));
    (
        HeadlessHandle {
            tool_names,
            session_id: session_ref,
            cwd: working_dir,
            task,
        },
        events,
    )
}

pub struct InteractiveParams {
    pub model: Model,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub excluded_tools: Vec<&'static str>,
    pub mcp_handle: Option<McpHandle>,
    pub initial_wd: PathBuf,
    pub resumed: Resumed,
    /// See [`HeadlessParams::claim`].
    pub claim: SessionClaim,
    /// See [`HeadlessParams::storage`].
    pub storage: StateDir,
    pub yolo: bool,
    pub system_prompt_override: Option<String>,
    pub append_system_prompt: Option<String>,
    /// The `always_*` knobs. `workflow` picks the tool catalog here; the rest
    /// are what a host without toggles puts on every [`AgentInput`] it sends.
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub project_config: ProjectConfig,
    /// Host-side overrides that shadow a registered tool's execution while
    /// keeping its advertised schema (e.g. ACP answers `question` via elicitation).
    pub local_tools: LocalTools,
}

pub struct InteractiveHandle {
    pub tool_names: Vec<String>,
    pub input_tx: flume::Sender<AgentInput>,
    pub answer_tx: flume::Sender<String>,
    pub cancel_tx: flume::Sender<()>,
    pub model_tx: flume::Sender<Model>,
    pub session_id: SessionRef,
    pub permissions: Arc<PermissionManager>,
    pub task: smol::Task<()>,
}

pub fn spawn_interactive(params: InteractiveParams) -> (InteractiveHandle, SessionEvents) {
    let AgentSetup {
        vars,
        instructions,
        mut tools,
    } = setup(
        &params.model,
        &params.config,
        &params.excluded_tools,
        params.defaults.workflow,
        params.mcp_handle.as_ref(),
    );

    let mcp = params
        .mcp_handle
        .clone()
        .map(|h| McpSession::new(h, &params.resumed.history));
    let tool_names = advertised_tool_names(tools.definitions(), mcp.as_ref());

    let (guard, events) = event_stream();
    let base_tx = guard.sender(0);
    let (input_tx, input_rx) = flume::unbounded::<AgentInput>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (cancel_tx, cancel_rx) = flume::bounded::<()>(1);
    let (model_tx, model_rx) = flume::unbounded::<Model>();

    let session_ref = params.resumed.id.clone();
    let mailbox = SessionMailbox::register(session_ref.id());

    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let mut permissions_config = params.permissions_config;
    permissions_config.yolo |= params.yolo;
    let permissions = Arc::new(PermissionManager::new(
        permissions_config,
        params.initial_wd,
        params.project_config,
        Arc::clone(&params.plugin_rules),
    ));

    let answer_rx = Arc::new(Mutex::new(answer_rx));
    let file_access = FileAccess::fresh();

    let session_ref_clone = session_ref.clone();
    let task_permissions = Arc::clone(&permissions);
    let task = smol::spawn(run_session(guard, params.mcp_handle.clone(), async move {
        let mut model = params.model;
        let Some(mut provider) = connect(&mut model, params.timeouts, &base_tx).await else {
            return;
        };
        let mut track =
            SessionTrack::open(params.resumed, params.claim, params.storage, &working_dir);
        let mut run_id: u64 = 0;

        while let Ok(input) = input_rx.recv_async().await {
            let (trigger, cancel) = CancelToken::new();
            let cancel_task = smol::spawn({
                let cancel_rx = cancel_rx.clone();
                async move {
                    if cancel_rx.recv_async().await.is_ok() {
                        trigger.cancel();
                    }
                }
            });

            // MCP connects in the background, so a prompt that beats it waits
            // here instead of shipping a turn without the MCP tools. The wait
            // is racing cancel: a slow server must not pin the whole session.
            if let Some(mcp) = &mcp {
                let _ = cancel.race(mcp.ready()).await;
            }

            let event_tx = base_tx.with_run_id(run_id);
            let error_tx = event_tx.clone();

            if let Some(mut new_model) = model_rx
                .try_iter()
                .last()
                .filter(|candidate| params.model_policy.allows(&candidate.spec()))
                && new_model.spec() != model.spec()
            {
                let Some(switched) = connect(&mut new_model, params.timeouts, &error_tx).await
                else {
                    run_id += 1;
                    continue;
                };
                provider = switched;
                tools = RequestTools::build(
                    ToolRegistry::global(),
                    &vars,
                    &new_model,
                    &params.config,
                    &params.excluded_tools,
                    params.defaults.workflow,
                    mcp.is_some(),
                );
                model = new_model;
            }

            let mut system = params.system_prompt_override.clone().unwrap_or_else(|| {
                agent::build_system_prompt(
                    &vars,
                    &input.mode,
                    &instructions.text,
                    &params.prompt_slots,
                    &model,
                )
            });
            if let Some(append) = &params.append_system_prompt {
                system.push('\n');
                system.push_str(append);
            }

            while answer_rx.lock().await.try_recv().is_ok() {}

            // Outlives `agent`, so this turn is on disk before the next prompt
            // is read, without the loop body remembering to write it.
            let mut turn = track.turn(model.spec());
            let mut agent = Agent::new(
                AgentParams {
                    provider: Arc::clone(&provider),
                    model: model.clone(),
                    config: params.config.clone(),
                    tool_output_lines: ToolOutputLines::default(),
                    permissions: Arc::clone(&task_permissions),
                    session_id: Some(session_ref_clone.clone()),
                    task_id: None,
                    mailbox: Some(mailbox.clone()),
                    timeouts: params.timeouts,
                    file_access: Arc::clone(&file_access),
                    prompt_slots: Arc::clone(&params.prompt_slots),
                    subagent_cancels: Arc::new(CancelMap::new()),
                    ledger: Arc::new(RunLedger::default()),
                    registry: Arc::clone(ToolRegistry::global_arc()),
                    audience: ToolAudience::MAIN,
                    model_policy: Arc::clone(&params.model_policy),
                },
                turn.run_params(system, event_tx, tools.clone()),
            )
            .with_loaded_instructions(instructions.loaded.clone())
            .with_user_response_rx(Arc::clone(&answer_rx))
            .with_cancel(cancel)
            .with_local_tools(Arc::clone(&params.local_tools))
            .with_mcp(mcp.clone());

            let result = agent.run(input).await;
            drop(agent);
            cancel_task.cancel().await;

            if let Err(ref e) = result {
                error!(error = %e, "agent error");
                let _ = error_tx.send(AgentEvent::Error {
                    message: e.user_message(),
                });
            }

            run_id += 1;
        }
    }));

    (
        InteractiveHandle {
            tool_names,
            input_tx,
            answer_tx,
            cancel_tx,
            model_tx,
            session_id: session_ref,
            permissions,
            task,
        },
        events,
    )
}

/// Waits for a session task that has nothing left to do but tear MCP down,
/// dropping it if that wedges. Safe to bound: the event stream already ended
/// (see [`run_session`]), and dropping the task cannot resurrect it.
pub async fn await_shutdown(task: smol::Task<()>) {
    futures_lite::future::or(task, async {
        smol::Timer::after(SESSION_SHUTDOWN_TIMEOUT).await;
    })
    .await;
}

/// Runs a session body, ends its event stream, then tears MCP down. The stream
/// ends with the run and not with teardown: a wedged shutdown must not keep a
/// consumer waiting for events that can no longer come.
async fn run_session(
    guard: EventStreamGuard,
    mcp_handle: Option<McpHandle>,
    body: impl Future<Output = ()>,
) {
    body.await;
    drop(guard);
    if let Some(handle) = mcp_handle {
        handle.shutdown().await;
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

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures_lite::future::poll_once;

    use super::*;
    use crate::mcp::McpCommand;

    const RUN_ID: u64 = 7;
    const STREAM_ENDED: &str = "the stream must end with the run, not with teardown";
    const SHUTDOWN_WEDGED: &str = "the MCP shutdown must still be waiting for its ack";
    const PROVIDER_ERROR: &str = "provider error";
    const BODY_EVENT: &str = "the body's event must be delivered before the close";
    const STILL_WORKING: &str = "await_shutdown must not return while the task still works";
    const TASK_DROPPED: &str = "await_shutdown dropped a task that had work left";

    #[test]
    fn extract_tool_names_filters_valid_entries() {
        let tools = serde_json::json!([{"name": "read"}, {"type": "function"}, {"name": "bash"}]);
        assert_eq!(extract_tool_names(&tools), vec!["read", "bash"]);
    }

    #[test]
    fn advertised_names_show_tool_search_not_deferred_tools() {
        let base = serde_json::json!([{"name": "read"}]);
        let mcp =
            crate::mcp::test_support::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
        let names = advertised_tool_names(&base, Some(&mcp));
        assert_eq!(
            names,
            vec!["read", crate::mcp::TOOL_SEARCH_TOOL_NAME],
            "clients must see the search tool, not deferred definitions"
        );
        assert_eq!(
            base,
            serde_json::json!([{"name": "read"}]),
            "probing must not bake MCP entries into the base tools"
        );
        assert_eq!(advertised_tool_names(&base, None), vec!["read"]);
    }

    /// The ordering the SDK exit rests on: the stream ends when the run ends,
    /// not when MCP teardown does. The handle here takes the `Shutdown` and
    /// never acks it, so the session future is still parked on its own timeout
    /// while the consumer already has the body's error and the end of the
    /// stream. The retained sender is the Lua tool context an idle VM never
    /// collects.
    #[test]
    fn stream_ends_with_the_run_not_with_mcp_teardown() {
        let (guard, mut events) = event_stream();
        let retained = guard.sender(RUN_ID);
        let event_tx = retained.clone();
        let (cmd_tx, cmd_rx) = flume::unbounded();
        smol::block_on(async {
            let mut session = pin!(run_session(
                guard,
                Some(McpHandle::for_test(cmd_tx)),
                async move {
                    let _ = event_tx.send(AgentEvent::Error {
                        message: PROVIDER_ERROR.into(),
                    });
                }
            ));
            assert!(
                poll_once(session.as_mut()).await.is_none(),
                "{SHUTDOWN_WEDGED}"
            );
            assert!(
                matches!(cmd_rx.try_recv(), Ok(McpCommand::Shutdown { .. })),
                "{SHUTDOWN_WEDGED}"
            );
            let envelope = events.next().await.expect(BODY_EVENT);
            assert!(matches!(
                envelope.event,
                AgentEvent::Error { message } if message == PROVIDER_ERROR
            ));
            assert!(
                matches!(poll_once(events.next()).await, Some(None)),
                "{STREAM_ENDED}"
            );
        });
        assert!(retained.send(AgentEvent::Nudge).is_ok());
    }

    /// `await_shutdown` bounds teardown, not the session: a task with work left
    /// runs to completion. Dropping it here would cancel prompts stdin already
    /// queued.
    #[test]
    fn await_shutdown_waits_for_a_task_that_still_works() {
        let (release_tx, release_rx) = flume::bounded::<()>(1);
        let finished = Arc::new(AtomicBool::new(false));
        let task = smol::spawn({
            let finished = Arc::clone(&finished);
            async move {
                let _ = release_rx.recv_async().await;
                finished.store(true, Ordering::SeqCst);
            }
        });

        smol::block_on(async {
            let mut shutdown = pin!(await_shutdown(task));
            assert!(
                poll_once(shutdown.as_mut()).await.is_none(),
                "{STILL_WORKING}"
            );
            release_tx.send(()).unwrap();
            shutdown.await;
        });
        assert!(finished.load(Ordering::SeqCst), "{TASK_DROPPED}");
    }
}
