use std::sync::Arc;

use arc_swap::ArcSwap;
use maki_agent::agent;
use maki_agent::mcp::McpHandle;
use maki_agent::mcp::config::McpServerStatus;
use maki_agent::permissions::PermissionManager;
use maki_agent::template;
use maki_agent::template::Vars;
use maki_agent::tools::{
    DescriptionContext, FileReadTracker, ToolAudience, ToolFilter, ToolRegistry,
};
use maki_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentParams, AgentRunParams, CancelMap,
    CancelToken, CancelTrigger, Envelope, EventSender, History, Instructions, McpCommand,
    PromptRole, ToolOutputLines,
};
use maki_lua::EventHandle;
use maki_providers::{AgentError, Message, Model, TokenUsage};
use serde_json::Value;
use tracing::error;

use super::ModelSlot;
use super::cancel_map::RunCancelMap;
use super::shared_queue::{QueueItem, QueueReceiver};

pub(super) struct AgentLoop {
    model_slot: Arc<ArcSwap<ModelSlot>>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    vars: Vars,
    instructions: Instructions,
    tools: Value,
    mcp_handle: Option<McpHandle>,
    history: History,
    btw_system: Arc<ArcSwap<String>>,
    cancel_map: Arc<RunCancelMap>,
    init_cancel: CancelToken,
    permissions: Arc<PermissionManager>,
    file_tracker: Arc<FileReadTracker>,
    min_run_id: u64,
    agent_tx: flume::Sender<Envelope>,
    answer_rx: Arc<async_lock::Mutex<flume::Receiver<String>>>,
    queue: Arc<QueueReceiver>,
    session_id: Option<String>,
    timeouts: maki_providers::Timeouts,
    lua_handle: Option<EventHandle>,
    subagent_cancels: Arc<CancelMap<String>>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        initial_history: Vec<Message>,
        shared_history: Arc<ArcSwap<Vec<Message>>>,
        btw_system: Arc<ArcSwap<String>>,
        mcp_handle: Option<McpHandle>,
        permissions: Arc<PermissionManager>,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: flume::Receiver<String>,
        queue: Arc<QueueReceiver>,
        cancel_map: Arc<RunCancelMap>,
        init_cancel: CancelToken,
        session_id: Option<String>,
        timeouts: maki_providers::Timeouts,
        lua_handle: Option<EventHandle>,
        subagent_cancels: Arc<CancelMap<String>>,
    ) -> Self {
        Self {
            model_slot,
            config,
            tool_output_lines,
            vars: Vars::default(),
            instructions: Instructions::default(),
            tools: Value::Null,
            mcp_handle,
            history: History::restored(initial_history).with_mirror(shared_history),
            btw_system,
            cancel_map,
            init_cancel,
            permissions,
            file_tracker: FileReadTracker::fresh(),
            min_run_id: 0,
            agent_tx,
            answer_rx: Arc::new(async_lock::Mutex::new(answer_rx)),
            queue,
            session_id,
            timeouts,
            lua_handle,
            subagent_cancels,
        }
    }

    pub(super) async fn run(mut self) {
        if !self.initialize().await {
            return;
        }

        while let Ok(()) = self.queue.recv_notify().await {
            while let Some(entry) = self.queue.pop() {
                if entry.run_id() < self.min_run_id {
                    continue;
                }
                self.process_entry(entry).await;
            }
        }
    }

    async fn process_entry(&mut self, entry: QueueItem) {
        let run_id = entry.run_id();
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);

        let result = match entry {
            QueueItem::Message {
                text,
                image_count,
                input,
                displayed,
                ..
            } => {
                if !displayed {
                    let _ = event_tx.send(AgentEvent::QueueItemConsumed { text, image_count });
                }
                self.do_agent_run(input, event_tx, run_id).await
            }
            QueueItem::Compact { .. } => self.do_compact(&event_tx).await,
        };

        if let Err(e) = result {
            self.emit_error(run_id, e);
        }
    }

    async fn initialize(&mut self) -> bool {
        self.vars = template::env_vars();
        self.reload_instructions().await;
        if self.init_cancel.is_cancelled() {
            return false;
        }
        self.publish_btw_system(&maki_agent::prompt::ResolvedSlots::default());

        let slot = self.model_slot.load();
        self.tools = self.build_tools(&slot.model, false);
        if let Some(ref mcp) = self.mcp_handle {
            mcp.extend_tools(&mut self.tools);
            spawn_oauth_for_needs_auth(mcp);
        }
        !self.init_cancel.is_cancelled()
    }

    async fn do_compact(&mut self, event_tx: &EventSender) -> Result<(), AgentError> {
        let slot = self.model_slot.load();
        let (provider, model) =
            agent::resolve_compaction_model(&slot.provider, &slot.model, self.timeouts);
        agent::compact(&*provider, &model, &mut self.history, event_tx).await
    }

    async fn do_agent_run(
        &mut self,
        mut input: AgentInput,
        event_tx: EventSender,
        run_id: u64,
    ) -> Result<(), AgentError> {
        let slot = self.model_slot.load();

        let old_cwd = self.vars.apply("{cwd}").into_owned();
        self.vars = template::env_vars();
        if *self.vars.apply("{cwd}") != old_cwd {
            self.reload_instructions().await;
        }
        self.rebuild_tools(&slot.model, input.workflow);

        for msg in std::mem::take(&mut input.preamble) {
            self.history.push(msg);
        }

        if let Some(ref prompt_ref) = input.prompt {
            let Some(ref mcp) = self.mcp_handle else {
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
                self.history.push(msg);
            }
        }

        let prompt_slots = match self.lua_handle.as_ref() {
            Some(h) => h.collect_prompt_slots_async().await,
            None => maki_agent::prompt::ResolvedSlots::default(),
        };
        let system = agent::build_system_prompt(
            &self.vars,
            &input.mode,
            &self.instructions.text,
            &prompt_slots,
            &slot.model,
        );
        self.publish_btw_system(&prompt_slots);
        let (trigger, cancel) = CancelToken::new();
        self.set_cancel_trigger(run_id, trigger);

        while self.answer_rx.lock().await.try_recv().is_ok() {}

        let mut agent = Agent::new(
            AgentParams {
                provider: Arc::clone(&slot.provider),
                model: slot.model.clone(),
                config: self.config.clone(),
                tool_output_lines: self.tool_output_lines,
                permissions: Arc::clone(&self.permissions),
                session_id: self.session_id.clone(),
                timeouts: self.timeouts,
                file_tracker: Arc::clone(&self.file_tracker),
                prompt_slots: Arc::new(prompt_slots),
                subagent_cancels: Arc::clone(&self.subagent_cancels),
                registry: Arc::clone(maki_agent::tools::ToolRegistry::native_arc()),
                audience: ToolAudience::MAIN,
            },
            AgentRunParams {
                history: &mut self.history,
                system,
                event_tx,
                tools: self.tools.clone(),
            },
        )
        .with_loaded_instructions(self.instructions.loaded.clone())
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_interrupt_source(Arc::clone(&self.queue) as Arc<dyn maki_agent::InterruptSource>)
        .with_cancel(cancel)
        .with_mcp(self.mcp_handle.clone());

        let result = agent.run(input).await;
        drop(agent);

        self.clear_cancel_trigger(run_id);

        if matches!(result, Err(AgentError::Cancelled)) {
            self.min_run_id = run_id + 1;
        }

        result
    }

    fn rebuild_tools(&mut self, model: &Model, workflow: bool) {
        let mut tools = self.build_tools(model, workflow);
        if let Some(ref mcp) = self.mcp_handle {
            mcp.extend_tools(&mut tools);
        }
        self.tools = tools;
    }

    fn build_tools(&self, model: &Model, workflow: bool) -> Value {
        let examples = model.supports_tool_examples();
        let filter = ToolFilter::from_config(&self.config, model, &[]);
        let ctx = DescriptionContext {
            filter: &filter,
            audience: ToolAudience::MAIN,
            workflow,
        };
        ToolRegistry::native().definitions(&self.vars, &ctx, examples)
    }

    async fn reload_instructions(&mut self) {
        let cwd = self.vars.apply("{cwd}").into_owned();
        self.instructions = smol::unblock(move || agent::load_instructions(&cwd)).await;
    }

    /// Always pins `Build` mode: btw runs no tools, so Plan-mode constraints would only confuse
    /// the model. Everything else matches the live prompt.
    fn publish_btw_system(&self, prompt_slots: &maki_agent::prompt::ResolvedSlots) {
        let slot = self.model_slot.load();
        let system = agent::build_system_prompt(
            &self.vars,
            &maki_agent::AgentMode::Build,
            &self.instructions.text,
            prompt_slots,
            &slot.model,
        );
        self.btw_system.store(Arc::new(system));
    }

    fn set_cancel_trigger(&self, run_id: u64, trigger: CancelTrigger) {
        self.cancel_map.insert(run_id, trigger);
    }

    fn clear_cancel_trigger(&self, run_id: u64) {
        self.cancel_map.remove(&run_id);
    }

    fn emit_error(&self, run_id: u64, error: AgentError) {
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
        match error {
            AgentError::Cancelled => {
                let _ = event_tx.send(AgentEvent::Done {
                    usage: TokenUsage::default(),
                    num_turns: 0,
                    stop_reason: None,
                });
            }
            e => {
                error!(error = %e, "agent error");
                let _ = event_tx.send(AgentEvent::Error {
                    message: e.user_message(),
                });
            }
        }
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
        smol::spawn(async move {
            let storage = match maki_storage::StateDir::resolve() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "cannot resolve storage for OAuth");
                    return;
                }
            };
            let auth_data = match maki_agent::mcp::oauth::authenticate(
                &server_name,
                &server_url,
                www_auth.as_deref(),
                &storage,
            )
            .await
            {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "background OAuth failed");
                    return;
                }
            };
            let Some(ref tokens) = auth_data.tokens else {
                return;
            };
            handle.send(McpCommand::Reconnect {
                server: server_name.clone(),
                url: server_url,
                token: tokens.access.clone(),
            });
            tracing::info!(server = %server_name, "MCP server authenticated via OAuth");
        })
        .detach();
    }
}
