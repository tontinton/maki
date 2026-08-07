use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_lock::Mutex;
use flume::Receiver;
use maki_providers::Message;
use maki_providers::Timeouts;
use maki_providers::TokenUsage;
use maki_providers::model::Model;
use maki_providers::provider::{self, Provider};
use maki_storage::StateDir;
use maki_storage::StorageError;
use maki_storage::id::{MakiId, SessionRef};
use maki_storage::sessions::{SESSIONS_DIR, Session, SessionError, SessionLog};
use serde_json::Value;
use tracing::{error, warn};

use crate::agent::{self, History};
use crate::cancel::{CancelMap, CancelToken};
use crate::permissions::PermissionManager;
use crate::prompt::ResolvedSlots;
use crate::template;
use crate::tools::{DescriptionContext, FileReadTracker, ToolAudience, ToolFilter, ToolRegistry};
use crate::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, Envelope,
    EventSender, ImageSource, McpHandle, McpSession, PermissionsConfig, SessionMailbox, ToolOutput,
    ToolOutputLines,
};

type StoredSession = Session<Message, TokenUsage, ToolOutput>;

struct SessionStore {
    dir: StateDir,
    session: StoredSession,
    log: Option<SessionLog>,
    /// The history epoch last mirrored into `session`; `None` until the first
    /// `record_turn`. A mismatch means the history was rewritten (compaction,
    /// restore sanitize) and the session needs a full replace, not a delta.
    synced_epoch: Option<u64>,
}

impl SessionStore {
    fn open(session_id: MakiId, cwd: &str, model_spec: &str) -> Result<Option<Self>, SessionError> {
        let dir = match StateDir::resolve() {
            Ok(dir) => dir,
            Err(e) => {
                warn!(error = %e, "state dir unavailable; session will not be persisted");
                return Ok(None);
            }
        };
        Self::open_in(dir, session_id, cwd, model_spec).map(Some)
    }

    fn open_in(
        dir: StateDir,
        session_id: MakiId,
        cwd: &str,
        model_spec: &str,
    ) -> Result<Self, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        let (session, log) = match StoredSession::load(session_id, &dir) {
            Ok(session) => {
                let log = SessionLog::open(&sessions_dir, &session)?;
                (session, Some(log))
            }
            Err(SessionError::Storage(StorageError::NotFound(_))) => {
                let mut session = StoredSession::new(model_spec, cwd);
                session.id = session_id;
                let log = SessionLog::rewrite(&sessions_dir, &session)?;
                (session, Some(log))
            }
            Err(e) => return Err(e),
        };
        Ok(Self {
            dir,
            session,
            log,
            synced_epoch: None,
        })
    }

    fn record_turn(&mut self, messages: &[Message], history_epoch: u64, model_spec: String) {
        // The loop hands over the full accumulated history every turn; push
        // only the delta so appends stay O(delta) instead of rewriting the
        // whole session. History rewrites (compaction, restore sanitize) mint
        // a fresh epoch; any mismatch forces a full replace, which mints a
        // fresh session epoch and voids the append cursors.
        let saved = self.session.messages().len();
        if self.synced_epoch == Some(history_epoch) && messages.len() >= saved {
            for msg in &messages[saved..] {
                self.session.push_message(msg.clone());
            }
        } else if self.synced_epoch.is_some() || !messages.is_empty() {
            // A resumed session first synced with no history keeps the loaded
            // messages; any other mismatch is a real divergence.
            self.session.replace_messages(messages.to_vec());
        }
        self.synced_epoch = Some(history_epoch);
        self.session.set_model(model_spec);
        self.session.update_title_if_default();
        self.persist();
    }

    fn persist(&mut self) {
        let Some(sessions_dir) = self.dir.ensure_subdir(SESSIONS_DIR).ok() else {
            return;
        };
        if let Err(e) = self.write_through(&sessions_dir) {
            warn!(error = %e, session_id = %self.session.id, "failed to persist session");
        }
    }

    fn write_through(&mut self, sessions_dir: &Path) -> Result<(), SessionError> {
        match &mut self.log {
            Some(log) => {
                if let Err(SessionError::LogDiverged { .. }) = log.append(&self.session) {
                    // The cursor is void and still holds the lock; drop it so
                    // the full rewrite below can lock the session.
                    self.log = None;
                    self.log = Some(SessionLog::rewrite(sessions_dir, &self.session)?);
                }
            }
            None => self.log = Some(SessionLog::rewrite(sessions_dir, &self.session)?),
        }
        Ok(())
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
    pub fast: bool,
    pub workflow: bool,
}

pub struct HeadlessHandle {
    pub event_rx: Receiver<Envelope>,
    pub tool_names: Vec<String>,
    pub session_id: SessionRef,
    pub cwd: String,
    pub task: smol::Task<()>,
}

struct AgentSetup {
    vars: template::Vars,
    instructions: agent::Instructions,
    tools: Value,
}

fn setup(
    model: &Model,
    config: &AgentConfig,
    excluded_tools: &[&'static str],
    workflow: bool,
) -> AgentSetup {
    let vars = template::env_vars();
    let instructions = agent::load_instructions(&vars.apply("{cwd}"));
    let tools = tool_definitions(
        &vars,
        model,
        config,
        excluded_tools,
        workflow,
        ToolRegistry::global(),
    );

    AgentSetup {
        vars,
        instructions,
        tools,
    }
}

/// Base definitions only. MCP definitions are injected per request by
/// `Agent::request_tools`; storing them here would freeze the catalog.
fn tool_definitions(
    vars: &template::Vars,
    model: &Model,
    config: &AgentConfig,
    excluded_tools: &[&'static str],
    workflow: bool,
    registry: &ToolRegistry,
) -> Value {
    let filter = ToolFilter::from_config(config, model, excluded_tools);
    let ctx = DescriptionContext {
        filter: &filter,
        audience: ToolAudience::MAIN,
        workflow,
    };
    registry.definitions(vars, &ctx, model.supports_tool_examples())
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

pub fn spawn(params: HeadlessParams) -> HeadlessHandle {
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
        params.workflow,
    );

    let system = agent::build_system_prompt(
        &vars,
        &mode,
        &instructions.text,
        &params.prompt_slots,
        &params.model,
    );

    let mcp = params.mcp_handle.clone().map(|h| McpSession::new(h, &[]));
    let tool_names = advertised_tool_names(&tools, mcp.as_ref());

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();

    let session_id = MakiId::generate();
    let session_ref = SessionRef::from(session_id);
    let session_ref_clone = session_ref.clone();
    let mailbox = SessionMailbox::register(session_id);
    let fast = params.fast;
    let workflow = params.workflow;
    let task = smol::spawn({
        let mcp_shutdown = params.mcp_handle.clone();
        let working_dir_path = params.initial_wd.clone();
        async move {
            let event_tx = EventSender::new(raw_tx, 0);
            let mut model = params.model;
            let provider: Arc<dyn Provider> =
                match provider::from_model_async(&mut model, params.timeouts).await {
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
            let mut history = History::new(Vec::new());
            let mut agent = Agent::new(
                AgentParams {
                    provider,
                    model,
                    config: params.config,
                    tool_output_lines: ToolOutputLines::default(),
                    permissions: Arc::new(PermissionManager::new(
                        params.permissions_config,
                        working_dir_path,
                    )),
                    session_id: Some(session_ref_clone.clone()),
                    mailbox: Some(mailbox.clone()),
                    timeouts: params.timeouts,
                    file_tracker: FileReadTracker::fresh(),
                    prompt_slots: Arc::new(params.prompt_slots),
                    subagent_cancels: Arc::new(CancelMap::new()),
                    registry: Arc::clone(ToolRegistry::global_arc()),
                    audience: ToolAudience::MAIN,
                },
                AgentRunParams {
                    history: &mut history,
                    system,
                    event_tx,
                    tools,
                },
            )
            .with_loaded_instructions(instructions.loaded)
            .with_mcp(mcp);

            let result = agent
                .run(AgentInput {
                    message: params.prompt,
                    mode,
                    images: params.images,
                    preamble: Vec::new(),
                    thinking: Default::default(),
                    fast,
                    workflow,
                    prompt: None,
                })
                .await;
            drop(agent);

            if let Err(e) = result {
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
        session_id: session_ref,
        cwd: working_dir,
        task,
    }
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
    pub session_id: Option<SessionRef>,
    pub initial_history: Vec<Message>,
    pub yolo: bool,
    pub system_prompt_override: Option<String>,
    pub append_system_prompt: Option<String>,
    pub workflow: bool,
}

pub struct InteractiveHandle {
    pub event_rx: Receiver<Envelope>,
    pub tool_names: Vec<String>,
    pub input_tx: flume::Sender<AgentInput>,
    pub answer_tx: flume::Sender<String>,
    pub cancel_tx: flume::Sender<()>,
    pub model_tx: flume::Sender<Model>,
    pub session_id: SessionRef,
    pub permissions: Arc<PermissionManager>,
    pub task: smol::Task<()>,
}

pub fn spawn_interactive(params: InteractiveParams) -> InteractiveHandle {
    let AgentSetup {
        vars,
        instructions,
        mut tools,
    } = setup(
        &params.model,
        &params.config,
        &params.excluded_tools,
        params.workflow,
    );

    let mcp = params
        .mcp_handle
        .clone()
        .map(|h| McpSession::new(h, &params.initial_history));
    let tool_names = advertised_tool_names(&tools, mcp.as_ref());

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();
    let (input_tx, input_rx) = flume::unbounded::<AgentInput>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (cancel_tx, cancel_rx) = flume::bounded::<()>(1);
    let (model_tx, model_rx) = flume::unbounded::<Model>();

    let (session_id, session_ref) = match params.session_id.clone() {
        Some(w) => (w.id(), w),
        None => {
            let id = MakiId::generate();
            (id, SessionRef::from(id))
        }
    };
    let mailbox = SessionMailbox::register(session_id);

    let working_dir = params.initial_wd.to_string_lossy().into_owned();
    let permissions = Arc::new(PermissionManager::new(
        params.permissions_config,
        params.initial_wd,
    ));
    if params.yolo {
        permissions.toggle_yolo();
    }

    let answer_rx = Arc::new(Mutex::new(answer_rx));
    let file_tracker = FileReadTracker::fresh();

    let session_ref_clone = session_ref.clone();
    let task = smol::spawn({
        let permissions = Arc::clone(&permissions);
        async move {
            let mut model = params.model;
            let mut provider: Arc<dyn Provider> =
                match provider::from_model_async(&mut model, params.timeouts).await {
                    Ok(p) => Arc::from(p),
                    Err(e) => {
                        error!(error = %e, "provider error");
                        let _ = EventSender::new(raw_tx, 0).send(AgentEvent::Error {
                            message: e.user_message(),
                        });
                        return;
                    }
                };

            let mut run_id: u64 = 0;
            let mut store = match SessionStore::open(session_id, &working_dir, &model.spec()) {
                Ok(store) => store,
                Err(e) => {
                    error!(error = %e, session_id = %session_id, "cannot open session storage");
                    let _ = EventSender::new(raw_tx.clone(), run_id).send(AgentEvent::Error {
                        message: e.to_string(),
                    });
                    return;
                }
            };
            let mut history = History::restored(params.initial_history);

            while let Ok(input) = input_rx.recv_async().await {
                let event_tx = EventSender::new(raw_tx.clone(), run_id);
                let error_tx = event_tx.clone();

                if let Some(mut new_model) = model_rx.try_iter().last()
                    && new_model.spec() != model.spec()
                {
                    match provider::from_model_async(&mut new_model, params.timeouts).await {
                        Ok(p) => {
                            provider = Arc::from(p);
                            tools = tool_definitions(
                                &vars,
                                &new_model,
                                &params.config,
                                &params.excluded_tools,
                                params.workflow,
                                ToolRegistry::global(),
                            );
                            model = new_model;
                        }
                        Err(e) => {
                            error!(error = %e, "provider error");
                            let _ = error_tx.send(AgentEvent::Error {
                                message: e.user_message(),
                            });
                            run_id += 1;
                            continue;
                        }
                    }
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

                let (trigger, cancel) = CancelToken::new();
                let cancel_task = smol::spawn({
                    let cancel_rx = cancel_rx.clone();
                    async move {
                        if cancel_rx.recv_async().await.is_ok() {
                            trigger.cancel();
                        }
                    }
                });

                while answer_rx.lock().await.try_recv().is_ok() {}

                let mut agent = Agent::new(
                    AgentParams {
                        provider: Arc::clone(&provider),
                        model: model.clone(),
                        config: params.config.clone(),
                        tool_output_lines: ToolOutputLines::default(),
                        permissions: Arc::clone(&permissions),
                        session_id: Some(session_ref_clone.clone()),
                        mailbox: Some(mailbox.clone()),
                        timeouts: params.timeouts,
                        file_tracker: Arc::clone(&file_tracker),
                        prompt_slots: Arc::clone(&params.prompt_slots),
                        subagent_cancels: Arc::new(CancelMap::new()),
                        registry: Arc::clone(ToolRegistry::global_arc()),
                        audience: ToolAudience::MAIN,
                    },
                    AgentRunParams {
                        history: &mut history,
                        system,
                        event_tx,
                        tools: tools.clone(),
                    },
                )
                .with_loaded_instructions(instructions.loaded.clone())
                .with_user_response_rx(Arc::clone(&answer_rx))
                .with_cancel(cancel)
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

                if let Some(store) = &mut store {
                    store.record_turn(history.as_slice(), history.epoch(), model.spec());
                }
                run_id += 1;
            }

            if let Some(handle) = params.mcp_handle {
                handle.shutdown().await;
            }
        }
    });

    InteractiveHandle {
        event_rx,
        tool_names,
        input_tx,
        answer_tx,
        cancel_tx,
        model_tx,
        session_id: session_ref,
        permissions,
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

#[cfg(test)]
mod tests {
    use maki_storage::session_types::generate_title;
    use tempfile::TempDir;

    use super::*;

    const SESSION_ID: &str = "01965087-4c71-7f00-8000-000000000000";
    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";
    const HISTORY_EPOCH: u64 = 7;

    fn session_id() -> MakiId {
        SESSION_ID.parse().unwrap()
    }

    fn store_in(tmp: &TempDir) -> SessionStore {
        SessionStore::open_in(
            StateDir::from_path(tmp.path().to_path_buf()),
            session_id(),
            CWD,
            MODEL_SPEC,
        )
        .unwrap()
    }

    fn load(tmp: &TempDir) -> StoredSession {
        StoredSession::load(session_id(), &StateDir::from_path(tmp.path().to_path_buf())).unwrap()
    }

    #[test]
    fn new_session_is_loadable_before_first_turn() {
        let tmp = TempDir::new().unwrap();
        store_in(&tmp);
        let loaded = load(&tmp);
        assert_eq!(loaded.id, session_id());
        assert_eq!(loaded.cwd, CWD);
        assert_eq!(loaded.model, MODEL_SPEC);
        assert!(loaded.messages().is_empty());
    }

    #[test]
    fn record_turn_persists_messages_and_title() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        let messages = vec![Message::user("fix the login bug".into())];
        store.record_turn(&messages, HISTORY_EPOCH, MODEL_SPEC.into());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.title, generate_title(&messages));
    }

    #[test]
    fn record_turn_persists_observations() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(
            &[
                Message::user("fix the login bug".into()),
                Message::observation("build failed".into()),
            ],
            HISTORY_EPOCH,
            MODEL_SPEC.into(),
        );

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert!(loaded.messages()[1].is_observation());
    }

    #[test]
    fn reopening_resumes_existing_session() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(
            &[Message::user("first prompt".into())],
            HISTORY_EPOCH,
            MODEL_SPEC.into(),
        );
        drop(store);

        let mut store = store_in(&tmp);
        assert_eq!(store.session.messages().len(), 1);

        let messages = vec![
            Message::user("first prompt".into()),
            Message::user("second prompt".into()),
        ];
        store.record_turn(&messages, HISTORY_EPOCH, "other/model".into());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.model, "other/model");
    }

    #[test]
    fn record_turn_replaces_when_history_epoch_changes() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(
            &[Message::user("first prompt".into())],
            1,
            MODEL_SPEC.into(),
        );
        store.record_turn(
            &[Message::user("rewritten prompt".into())],
            2,
            MODEL_SPEC.into(),
        );

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.messages()[0].user_text(), Some("rewritten prompt"));
    }

    #[test]
    fn record_turn_appends_when_epoch_unchanged() {
        let tmp = TempDir::new().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(
            &[Message::user("first prompt".into())],
            1,
            MODEL_SPEC.into(),
        );
        store.record_turn(
            &[
                Message::user("first prompt".into()),
                Message::user("second prompt".into()),
            ],
            1,
            MODEL_SPEC.into(),
        );

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.messages()[1].user_text(), Some("second prompt"));
    }

    #[test]
    fn open_in_errors_when_session_is_locked() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        let err = match SessionStore::open_in(
            StateDir::from_path(tmp.path().to_path_buf()),
            session_id(),
            CWD,
            MODEL_SPEC,
        ) {
            Ok(_) => panic!("expected Locked error"),
            Err(e) => e,
        };
        assert!(matches!(err, SessionError::Locked { .. }));
        drop(store);
    }

    #[test]
    fn open_in_does_not_mint_on_corrupt_meta() {
        let tmp = TempDir::new().unwrap();
        store_in(&tmp).record_turn(
            &[Message::user("hi".into())],
            HISTORY_EPOCH,
            MODEL_SPEC.into(),
        );

        let folder = tmp.path().join(SESSIONS_DIR).join(session_id().to_string());
        std::fs::write(folder.join("meta.json"), b"not json").unwrap();

        let err = match SessionStore::open_in(
            StateDir::from_path(tmp.path().to_path_buf()),
            session_id(),
            CWD,
            MODEL_SPEC,
        ) {
            Ok(_) => panic!("expected storage error"),
            Err(e) => e,
        };
        assert!(matches!(err, SessionError::Storage(_)));
        assert!(
            folder.join("meta.json").exists(),
            "corrupt session must not be replaced"
        );
    }

    #[test]
    fn extract_tool_names_filters_valid_entries() {
        let tools = serde_json::json!([{"name": "read"}, {"type": "function"}, {"name": "bash"}]);
        assert_eq!(extract_tool_names(&tools), vec!["read", "bash"]);
    }

    #[test]
    fn advertised_names_show_tool_search_not_deferred_tools() {
        let base = serde_json::json!([{"name": "read"}]);
        let mcp = crate::mcp::stub_session(&[("srv.fetch_issue", "Fetch a GitHub issue")]);
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
}
