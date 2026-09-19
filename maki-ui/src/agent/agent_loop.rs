use std::sync::Arc;

use arc_swap::ArcSwap;
use maki_agent::agent;
use maki_agent::mcp::config::McpServerStatus;
use maki_agent::mcp::{McpHandle, McpSession};
use maki_agent::permissions::PermissionManager;
use maki_agent::session::Resumed;
use maki_agent::template;
use maki_agent::template::Vars;
use maki_agent::tools::{FileAccess, RequestTools, ToolAudience, ToolRegistry};
use maki_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentParams, AgentRunParams, CancelMap,
    CancelToken, DoneReason, Envelope, EventSender, History, Instructions, McpCommand, PromptRole,
    RunLedger, SessionMailbox, SharedMessages, ToolOutputLines,
};
use maki_config::ModelPolicy;
use maki_lua::EventHandle;
use maki_providers::{AgentError, ContextGauge, Message, Model};
use maki_storage::id::SessionRef;
use tracing::error;

use super::ModelSlot;
use super::run_cancels::RunCancels;
use super::shared_queue::{self, QueueReceiver, QueueRun};

pub(super) struct AgentLoop {
    model_slot: Arc<ArcSwap<ModelSlot>>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    vars: Vars,
    instructions: Instructions,
    tools: RequestTools,
    mcp: Option<McpSession>,
    history: History,
    /// Owned beside `history` because it describes that transcript and outlives
    /// every run over it, so the provider's own counts pile up between turns.
    gauge: ContextGauge,
    btw_system: Arc<ArcSwap<String>>,
    cancels: Arc<RunCancels>,
    permissions: Arc<PermissionManager>,
    file_access: Arc<FileAccess>,
    agent_tx: flume::Sender<Envelope>,
    answer_rx: Arc<async_lock::Mutex<flume::Receiver<String>>>,
    queue: Arc<QueueReceiver>,
    session_id: SessionRef,
    mailbox: SessionMailbox,
    timeouts: maki_providers::Timeouts,
    lua_handle: EventHandle,
    subagent_cancels: Arc<CancelMap<String>>,
    model_policy: Arc<ModelPolicy>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        resumed: Resumed,
        shared_history: SharedMessages,
        btw_system: Arc<ArcSwap<String>>,
        mcp_handle: Option<McpHandle>,
        permissions: Arc<PermissionManager>,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: flume::Receiver<String>,
        queue: Arc<QueueReceiver>,
        cancels: Arc<RunCancels>,
        mailbox: SessionMailbox,
        timeouts: maki_providers::Timeouts,
        lua_handle: EventHandle,
        subagent_cancels: Arc<CancelMap<String>>,
        model_policy: Arc<ModelPolicy>,
    ) -> Self {
        let mcp = mcp_handle.map(|h| McpSession::new(h, &resumed.history));
        Self {
            session_id: resumed.id,
            model_slot,
            config,
            tool_output_lines,
            vars: Vars::default(),
            instructions: Instructions::default(),
            tools: RequestTools::default(),
            mcp,
            history: History::restored(resumed.history).with_mirror(shared_history),
            gauge: ContextGauge::restored(resumed.context_size),
            btw_system,
            cancels,
            permissions,
            file_access: FileAccess::fresh(),
            agent_tx,
            answer_rx: Arc::new(async_lock::Mutex::new(answer_rx)),
            queue,
            mailbox,
            timeouts,
            lua_handle,
            subagent_cancels,
            model_policy,
        }
    }

    pub(super) async fn run(mut self) {
        if !self.initialize().await {
            return;
        }

        while let Ok(()) = self.queue.recv_notify().await {
            let mut last_run_id = None;
            while let Some(mut run) = self.queue.pop_run() {
                let Some(run_id) = run.drop_cancelled(self.cancels.min_run_id()) else {
                    continue;
                };
                last_run_id = Some(run_id);
                self.process_run(run, run_id).await;
            }
            if let Some(run_id) = last_run_id {
                let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
                self.queue
                    .publish_if_empty(|| event_tx.try_send(AgentEvent::QueueDrained));
            }
        }
    }

    async fn process_run(&mut self, run: QueueRun, run_id: u64) {
        let live = self.cancels.start(run_id);
        let result = self.dispatch_run(run, run_id, live.token()).await;
        // A `tool_use_id` only names work inside the run that issued the call,
        // so the run ending is what stops whatever still hangs off one, rather
        // than a group emptying out.
        self.subagent_cancels.cancel_all();

        // A cancel arrives here as `Ok`, since esc is what the user asked for.
        // As an error it would draw a second "Cancelled." bubble under the one
        // the app already drew, leave the status bar red and fail the exit
        // under `--exit-on-done`.
        if let Err(e) = result {
            self.emit_error(run_id, e);
        }
    }

    async fn dispatch_run(
        &mut self,
        run: QueueRun,
        run_id: u64,
        cancel: &CancelToken,
    ) -> Result<(), AgentError> {
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
        match run {
            QueueRun::Compact(compaction) => {
                self.do_compact(&event_tx, compaction.instructions.as_deref(), cancel)
                    .await?;
            }
            QueueRun::Messages(messages) => {
                let inputs = messages
                    .into_iter()
                    .map(|queued| {
                        if !queued.displayed {
                            let _ = event_tx.send(AgentEvent::QueueItemConsumed {
                                text: queued.text,
                                images: queued.input.images.clone(),
                            });
                        }
                        queued.input
                    })
                    .collect();
                if let Some(input) = shared_queue::merge_inputs(inputs) {
                    self.do_agent_run(input, event_tx, cancel).await?;
                }
            }
        }
        Ok(())
    }

    /// Startup belongs to the loop, not to any run, so it watches teardown. Esc
    /// only means the user gave up on a prompt, and ending the loop over that
    /// would leave the session on screen with nothing to serve it.
    async fn initialize(&mut self) -> bool {
        let teardown = self.cancels.teardown().clone();
        self.vars = template::env_vars();
        self.reload_instructions().await;
        if teardown.is_cancelled() {
            return false;
        }
        self.publish_btw_system(&maki_agent::prompt::ResolvedSlots::default());

        let slot = self.model_slot.load();
        self.tools = self.build_tools(&slot.model, false);
        if let Some(ref mcp) = self.mcp {
            // The queue is drained right after this, and a prompt typed during
            // startup must still carry the MCP tools.
            if teardown.race(mcp.ready()).await.is_err() {
                return false;
            }
            spawn_oauth_for_needs_auth(mcp);
        }
        !teardown.is_cancelled()
    }

    async fn do_compact(
        &mut self,
        event_tx: &EventSender,
        instructions: Option<&str>,
        cancel: &CancelToken,
    ) -> Result<DoneReason, AgentError> {
        let slot = self.model_slot.load();
        let (provider, model) = agent::resolve_compaction_model(
            &slot.provider,
            &slot.model,
            self.timeouts,
            &self.model_policy,
        );
        // Compaction resizes the gauge, and the gauge has to describe the whole
        // next prompt. A standalone `/compact` has no mode of its own, so this
        // is the same Build-mode prompt `publish_btw_system` builds from the
        // vars, instructions and slots a run would use.
        let system = self.system_prompt(&self.lua_handle.collect_prompt_slots_async().await);
        let tools = agent::request_tools(&self.tools, self.mcp.as_ref());
        agent::compact(
            &*provider,
            &model,
            &mut self.history,
            &mut self.gauge,
            &system,
            &tools,
            event_tx,
            cancel,
            &self.config,
            instructions,
            Some(&self.session_id),
            self.timeouts.retry,
        )
        .await
    }

    async fn do_agent_run(
        &mut self,
        mut input: AgentInput,
        event_tx: EventSender,
        cancel: &CancelToken,
    ) -> Result<DoneReason, AgentError> {
        let slot = self.model_slot.load();

        let old_cwd = self.vars.apply("{cwd}").into_owned();
        self.vars = template::env_vars();
        if *self.vars.apply("{cwd}") != old_cwd {
            self.reload_instructions().await;
        }
        self.rebuild_tools(&slot.model, input.workflow);

        if let Some(ref prompt_ref) = input.prompt {
            let Some(ref mcp) = self.mcp else {
                return Err(AgentError::Tool {
                    tool: "mcp_prompt".into(),
                    message: "MCP not available".into(),
                });
            };
            let messages = mcp
                .get_prompt(&prompt_ref.qualified_name, &prompt_ref.arguments)
                .await
                .map_err(|e| AgentError::Tool {
                    tool: "mcp_prompt".into(),
                    message: e.to_string(),
                })?;
            for pm in messages {
                let text = pm.content.text.unwrap_or_default();
                let msg = match pm.role {
                    PromptRole::Assistant => Message {
                        role: maki_providers::Role::Assistant,
                        content: vec![maki_providers::ContentBlock::Text { text }],
                        ..Default::default()
                    },
                    PromptRole::User => Message::user(text),
                };
                input.preamble.push(msg);
            }
        }

        let prompt_slots = self.lua_handle.collect_prompt_slots_async().await;
        let system = agent::build_system_prompt(
            &self.vars,
            &input.mode,
            &self.instructions.text,
            &prompt_slots,
            &slot.model,
        );
        self.publish_btw_system(&prompt_slots);

        while self.answer_rx.lock().await.try_recv().is_ok() {}

        let mut agent = Agent::new(
            AgentParams {
                provider: Arc::clone(&slot.provider),
                model: slot.model.clone(),
                config: self.config.clone(),
                tool_output_lines: self.tool_output_lines,
                permissions: Arc::clone(&self.permissions),
                session_id: Some(self.session_id.clone()),
                task_id: None,
                mailbox: Some(self.mailbox.clone()),
                timeouts: self.timeouts,
                file_access: Arc::clone(&self.file_access),
                prompt_slots: Arc::new(prompt_slots),
                subagent_cancels: Arc::clone(&self.subagent_cancels),
                ledger: Arc::new(RunLedger::default()),
                registry: Arc::clone(maki_agent::tools::ToolRegistry::global_arc()),
                audience: ToolAudience::MAIN,
                model_policy: Arc::clone(&self.model_policy),
            },
            AgentRunParams {
                history: &mut self.history,
                gauge: &mut self.gauge,
                system,
                event_tx,
                tools: self.tools.clone(),
            },
        )
        .with_loaded_instructions(self.instructions.loaded.clone())
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_interrupt_source(Arc::clone(&self.queue) as Arc<dyn maki_agent::InterruptSource>)
        .with_cancel(cancel.clone())
        .with_mcp(self.mcp.clone());

        let result = agent.run(input).await;
        drop(agent);
        result
    }

    /// Base tools only. MCP definitions are injected per request by
    /// `Agent::request_tools`; baking them here would freeze the catalog.
    fn rebuild_tools(&mut self, model: &Model, workflow: bool) {
        self.tools = self.build_tools(model, workflow);
    }

    fn build_tools(&self, model: &Model, workflow: bool) -> RequestTools {
        RequestTools::build(
            ToolRegistry::global(),
            &self.vars,
            model,
            &self.config,
            &[],
            workflow,
            self.mcp.is_some(),
        )
    }

    async fn reload_instructions(&mut self) {
        let cwd = self.vars.apply("{cwd}").into_owned();
        self.instructions = smol::unblock(move || agent::load_instructions(&cwd)).await;
    }

    fn publish_btw_system(&self, prompt_slots: &maki_agent::prompt::ResolvedSlots) {
        self.btw_system
            .store(Arc::new(self.system_prompt(prompt_slots)));
    }

    /// Always pins `Build` mode: btw runs no tools, so Plan-mode constraints would only confuse
    /// the model, and a gauge sizing this only cares about the length. Everything else matches
    /// the live prompt.
    fn system_prompt(&self, prompt_slots: &maki_agent::prompt::ResolvedSlots) -> String {
        agent::build_system_prompt(
            &self.vars,
            &maki_agent::AgentMode::Build,
            &self.instructions.text,
            prompt_slots,
            &self.model_slot.load().model,
        )
    }

    fn emit_error(&self, run_id: u64, error: AgentError) {
        error!(error = %error, "agent error");
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
        let _ = event_tx.send(AgentEvent::Error {
            message: error.user_message(),
        });
    }
}

fn spawn_oauth_for_needs_auth(handle: &McpHandle) {
    let snapshot = handle.reader().load().clone();
    for info in snapshot.infos.iter() {
        let McpServerStatus::NeedsAuth { ref url } = info.status else {
            continue;
        };
        let Some(ref server_url) = info.url else {
            continue;
        };
        let handle = handle.clone();
        let server_name = info.name.clone();
        let server_url = server_url.clone();
        let www_auth = url.clone();
        let oauth = info.oauth.clone();
        smol::spawn(async move {
            let storage = match maki_storage::StateDir::resolve() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "cannot resolve storage for OAuth");
                    return;
                }
            };
            if let Err(e) = maki_agent::mcp::oauth::authenticate(
                &server_name,
                &server_url,
                www_auth.as_deref(),
                &storage,
                maki_agent::mcp::oauth::Interaction::Background,
                oauth,
            )
            .await
            {
                tracing::warn!(server = %server_name, error = %e, "background OAuth failed");
                return;
            }
            handle.send(McpCommand::Reconnect {
                server: server_name.clone(),
            });
            tracing::info!(server = %server_name, "MCP server authenticated via OAuth");
        })
        .detach();
    }
}
