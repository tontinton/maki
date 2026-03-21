use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use maki_agent::agent;
use maki_agent::mcp::McpManager;
use maki_agent::mcp::config::{McpServerInfo, persist_enabled};
use maki_agent::permissions::PermissionManager;
use maki_agent::skill::Skill;
use maki_agent::template;
use maki_agent::template::Vars;
use maki_agent::tools::ToolCall;
use maki_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentParams, AgentRunParams, CancelToken,
    CancelTrigger, Envelope, EventSender, ExtractedCommand, History, LoadedInstructions,
};
use maki_providers::provider::Provider;
use maki_providers::{AgentError, Message, Model, TokenUsage};
use serde_json::Value;
use tracing::error;

use super::toggle_disabled;

pub(super) struct AgentLoop {
    provider: Arc<dyn Provider>,
    model: Model,
    skills: Arc<[Skill]>,
    config: AgentConfig,
    vars: Vars,
    instructions: String,
    loaded_instructions: LoadedInstructions,
    tools: Value,
    disabled: Vec<String>,
    mcp_manager: Option<Arc<McpManager>>,
    mcp_infos: Arc<ArcSwap<Vec<McpServerInfo>>>,
    mcp_pids: Arc<Mutex<Vec<u32>>>,
    history: History,
    shared_history: Arc<ArcSwap<Vec<Message>>>,
    cancel_trigger: Arc<Mutex<Option<CancelTrigger>>>,
    permissions: Arc<PermissionManager>,
    min_run_id: u64,
    agent_tx: flume::Sender<Envelope>,
    answer_rx: Arc<async_lock::Mutex<flume::Receiver<String>>>,
    ecmd_rx: Option<flume::Receiver<ExtractedCommand>>,
    toggle_rx: flume::Receiver<(String, bool)>,
}

enum LoopEvent {
    Command(ExtractedCommand),
    Toggle(String, bool),
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        provider: Arc<dyn Provider>,
        model: Model,
        skills: Arc<[Skill]>,
        config: AgentConfig,
        initial_history: Vec<Message>,
        shared_history: Arc<ArcSwap<Vec<Message>>>,
        mcp_infos: Arc<ArcSwap<Vec<McpServerInfo>>>,
        mcp_pids: Arc<Mutex<Vec<u32>>>,
        initial_disabled: Vec<String>,
        permissions: Arc<PermissionManager>,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: flume::Receiver<String>,
        ecmd_rx: flume::Receiver<ExtractedCommand>,
        toggle_rx: flume::Receiver<(String, bool)>,
        cancel_trigger: Arc<Mutex<Option<CancelTrigger>>>,
    ) -> Self {
        Self {
            provider,
            model,
            skills,
            config,
            vars: Vars::default(),
            instructions: String::new(),
            loaded_instructions: LoadedInstructions::default(),
            tools: Value::Null,
            disabled: initial_disabled,
            mcp_manager: None,
            mcp_infos,
            mcp_pids,
            history: History::new(initial_history),
            shared_history,
            cancel_trigger,
            permissions,
            min_run_id: 0,
            agent_tx,
            answer_rx: Arc::new(async_lock::Mutex::new(answer_rx)),
            ecmd_rx: Some(ecmd_rx),
            toggle_rx,
        }
    }

    pub(super) async fn run(mut self) {
        self.initialize().await;

        loop {
            let event = {
                let ecmd_rx = self
                    .ecmd_rx
                    .as_ref()
                    .expect("ecmd_rx available between runs");
                let toggle_rx = &self.toggle_rx;
                futures_lite::future::race(
                    async { ecmd_rx.recv_async().await.ok().map(LoopEvent::Command) },
                    async {
                        toggle_rx
                            .recv_async()
                            .await
                            .ok()
                            .map(|(s, e)| LoopEvent::Toggle(s, e))
                    },
                )
                .await
            };

            let Some(event) = event else { break };

            match event {
                LoopEvent::Toggle(name, enabled) => self.handle_mcp_toggle(name, enabled),
                LoopEvent::Command(cmd) => self.handle_command(cmd).await,
            }
        }
    }

    async fn initialize(&mut self) {
        self.vars = template::env_vars();
        self.reload_instructions().await;

        self.tools = ToolCall::definitions(
            &self.vars,
            &self.skills,
            self.model.family.supports_tool_examples(),
        );

        let cwd = PathBuf::from(self.vars.apply("{cwd}").into_owned());
        self.init_mcp(&cwd).await;
    }

    async fn init_mcp(&mut self, cwd: &Path) {
        let mcp_config = maki_agent::mcp::config::load_config(cwd);
        self.disabled.sort_unstable();
        self.disabled.dedup();

        if !mcp_config.is_empty() {
            self.mcp_infos
                .store(Arc::new(mcp_config.preliminary_infos(&self.disabled)));
        }

        let mcp_manager = McpManager::start_with_config(mcp_config).await;

        if let Some(ref mgr) = mcp_manager {
            mgr.extend_tools(&mut self.tools, &self.disabled);
            self.mcp_infos
                .store(Arc::new(mgr.server_infos(&self.disabled)));
            *self.mcp_pids.lock().unwrap_or_else(|e| e.into_inner()) = mgr.child_pids();
        }

        self.mcp_manager = mcp_manager;
    }

    async fn handle_command(&mut self, cmd: ExtractedCommand) {
        let (event_tx, run_id) = match &cmd {
            ExtractedCommand::Interrupt(_, run_id) | ExtractedCommand::Compact(run_id)
                if *run_id >= self.min_run_id =>
            {
                (EventSender::new(self.agent_tx.clone(), *run_id), *run_id)
            }
            _ => return,
        };

        let result = match cmd {
            ExtractedCommand::Compact(_) => self.do_compact(&event_tx).await,
            ExtractedCommand::Interrupt(input, run_id) => {
                self.do_agent_run(input, event_tx, run_id).await
            }
            ExtractedCommand::Cancel | ExtractedCommand::Ignore => return,
        };

        if let Err(e) = result {
            self.emit_error(run_id, e);
        }
    }

    async fn do_compact(&mut self, event_tx: &EventSender) -> Result<(), AgentError> {
        let result =
            agent::compact(&*self.provider, &self.model, &mut self.history, event_tx).await;
        self.sync_shared_history();
        result
    }

    async fn do_agent_run(
        &mut self,
        mut input: AgentInput,
        event_tx: EventSender,
        run_id: u64,
    ) -> Result<(), AgentError> {
        let old_cwd = self.vars.apply("{cwd}").into_owned();
        self.vars = template::env_vars();
        if *self.vars.apply("{cwd}") != old_cwd {
            self.reload_instructions().await;
            self.rebuild_tools();
        }

        for msg in mem::take(&mut input.preamble) {
            self.history.push(msg);
        }
        self.sync_shared_history_with_pending(&input);

        let system = agent::build_system_prompt(&self.vars, &input.mode, &self.instructions);
        let (trigger, cancel) = CancelToken::new();
        self.set_cancel_trigger(Some(trigger));

        while self.answer_rx.lock().await.try_recv().is_ok() {}

        let ecmd_rx = self.ecmd_rx.take().expect("ecmd_rx available before run");
        let agent = Agent::new(
            AgentParams {
                provider: Arc::clone(&self.provider),
                model: self.model.clone(),
                skills: Arc::clone(&self.skills),
                config: self.config,
                permissions: Arc::clone(&self.permissions),
            },
            AgentRunParams {
                history: mem::replace(&mut self.history, History::new(Vec::new())),
                system,
                event_tx,
                tools: self.tools.clone(),
            },
        )
        .with_loaded_instructions(self.loaded_instructions.clone())
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_cmd_rx(ecmd_rx)
        .with_cancel(cancel)
        .with_mcp(self.mcp_manager.clone());

        let outcome = agent.run(input).await;

        self.set_cancel_trigger(None);
        self.history = outcome.history;
        self.sync_shared_history();
        self.ecmd_rx = Some(outcome.cmd_rx.expect("cmd_rx was set"));

        if matches!(outcome.result, Err(AgentError::Cancelled)) {
            self.min_run_id = run_id + 1;
        }

        outcome.result
    }

    fn handle_mcp_toggle(&mut self, server_name: String, enabled: bool) {
        toggle_disabled(&mut self.disabled, &server_name, enabled);
        self.rebuild_tools();

        if let Some(ref mcp) = self.mcp_manager {
            let infos = mcp.server_infos(&self.disabled);
            self.persist_mcp_toggle(&infos, &server_name, enabled);
            self.mcp_infos.store(Arc::new(infos));
        }
    }

    fn rebuild_tools(&mut self) {
        let mut tools = ToolCall::definitions(
            &self.vars,
            &self.skills,
            self.model.family.supports_tool_examples(),
        );
        if let Some(ref mcp) = self.mcp_manager {
            mcp.extend_tools(&mut tools, &self.disabled);
        }
        self.tools = tools;
    }

    async fn reload_instructions(&mut self) {
        let cwd = self.vars.apply("{cwd}").into_owned();
        let (instructions, loaded) =
            smol::unblock(move || agent::load_instruction_files(&cwd)).await;
        self.instructions = instructions;
        self.loaded_instructions = loaded;
    }

    fn persist_mcp_toggle(&self, infos: &[McpServerInfo], server_name: &str, enabled: bool) {
        if let Some(info) = infos.iter().find(|i| i.name == server_name) {
            let path = info.config_path.clone();
            let name = server_name.to_owned();
            let server_for_log = server_name.to_owned();
            smol::spawn(async move {
                if let Err(e) =
                    smol::unblock(move || persist_enabled(&path, &name, enabled)).await
                {
                    tracing::warn!(error = %e, server = %server_for_log, "failed to persist MCP toggle");
                }
            })
            .detach();
        }
    }

    fn sync_shared_history(&self) {
        self.shared_history
            .store(Arc::new(self.history.as_slice().to_vec()));
    }

    fn sync_shared_history_with_pending(&self, input: &AgentInput) {
        let mut snapshot = self.history.as_slice().to_vec();
        snapshot.push(Message::user(input.effective_message()));
        self.shared_history.store(Arc::new(snapshot));
    }

    fn set_cancel_trigger(&self, trigger: Option<CancelTrigger>) {
        *self
            .cancel_trigger
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = trigger;
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
