use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use color_eyre::Result;
use color_eyre::eyre::Context;

use crossterm::event::{
    self, Event, KeyEventKind, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use maki_agent::command::CustomCommand;
use std::path::PathBuf;
use maki_agent::permissions::PermissionManager;
use maki_agent::{AgentConfig, CancelToken, McpCommand};
use maki_config::UiConfig;
use maki_lua::{EventHandle, HintReader, KeymapReader, LuaCommandReader, UiAction};
use maki_providers::Timeouts;
use maki_providers::provider::{Provider, fetch_all_models, from_model};
use maki_providers::{Message, Model};
use maki_storage::StateDir;
use tracing::warn;

use crate::AppSession;
use crate::agent::{AgentCommand, AgentHandles, ModelSlot, shared_queue::QueueItem};
use crate::app::shell::{ShellEvent, spawn_shell};
use crate::app::{App, Msg};
use crate::components::input::Submission;
use crate::components::usage_modal::UsageFetchState;
use crate::components::{Action, ExitRequest, Status};

use crate::storage_writer::StorageWriter;
use crate::terminal;

const ANIMATION_INTERVAL_MS: u64 = 16;
const IDLE_POLL_INTERVAL_MS: u64 = 100;

pub struct EventLoopParams {
    pub model: Model,
    pub needs_login: bool,
    pub commands: Vec<CustomCommand>,
    pub session: AppSession,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: Timeouts,
    pub exit_on_done: bool,
    pub lua_command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: HintReader,
    pub ui_action_rx: Option<flume::Receiver<UiAction>>,
    pub lua_event_handle: Option<EventHandle>,
    /// Open directly on the multi-session agents dashboard (`maki agents`).
    pub dashboard: bool,
}

/// One interactive session: its UI `App`, the agent runner driving it, and the
/// shell channel scoped to that session. The event loop owns a set of these and
/// focuses one at a time; unfocused runtimes keep making progress because their
/// agent tasks live on the smol executor regardless of focus.
pub(crate) struct SessionRuntime {
    app: App,
    handles: AgentHandles,
    shell_tx: flume::Sender<ShellEvent>,
    shell_rx: flume::Receiver<ShellEvent>,
}

pub(crate) struct EventLoop<'t> {
    terminal: &'t mut ratatui::DefaultTerminal,
    sessions: Vec<SessionRuntime>,
    focused: usize,
    model_slot: Arc<ArcSwap<ModelSlot>>,
    config: AgentConfig,
    permissions: Arc<PermissionManager>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    available_models: Arc<ArcSwapOption<Vec<String>>>,
    storage_writer: Arc<StorageWriter>,
    timeouts: Timeouts,
    ui_action_rx: Option<flume::Receiver<UiAction>>,
    spawn_ctx: SpawnContext,
    _model_fetch_task: smol::Task<()>,
}

/// Cloneable inputs needed to build additional `SessionRuntime`s at runtime
/// (e.g. when the dashboard spawns or opens a session). All fields are cheap
/// Arc-backed handles or shared config.
struct SpawnContext {
    storage: StateDir,
    ui_config: UiConfig,
    input_history_size: usize,
    lua_command_reader: LuaCommandReader,
    keymap_reader: KeymapReader,
    hint_reader: HintReader,
    lua_event_handle: Option<EventHandle>,
    custom_commands: Arc<[CustomCommand]>,
    cwd: PathBuf,
}

struct BackgroundModels {
    available: Arc<ArcSwapOption<Vec<String>>>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    task: smol::Task<()>,
}

fn merge_batch(
    available: &Arc<ArcSwapOption<Vec<String>>>,
    batch: maki_providers::provider::ModelBatch,
    warn_tx: &flume::Sender<String>,
) {
    for w in batch.warnings {
        let _ = warn_tx.try_send(w);
    }
    if batch.models.is_empty() {
        return;
    }
    let mut merged = available.load().as_deref().cloned().unwrap_or_default();
    for spec in &batch.models {
        if !merged.contains(spec) {
            merged.push(spec.clone());
        }
    }
    available.store(Some(Arc::new(merged)));
}

fn spawn_model_fetch(model_slot: &Arc<ArcSwap<ModelSlot>>, timeouts: Timeouts) -> BackgroundModels {
    let available: Arc<ArcSwapOption<Vec<String>>> = Arc::new(ArcSwapOption::empty());
    let bg = Arc::clone(&available);
    let (warn_tx, warn_rx) = flume::unbounded::<String>();
    let warn_tx_bg = warn_tx.clone();
    let model_slot = Arc::clone(model_slot);
    let task = smol::spawn(async move {
        let warn_tx = warn_tx_bg;
        let done = Box::new(move || {
            let spec = model_slot.load().model.spec();
            let mut resolved = match Model::from_spec(&spec) {
                Ok(m) => m,
                Err(e) => {
                    warn!(spec = %spec, error = %e, "failed to resolve model after discovery");
                    return;
                }
            };
            let provider = match from_model(&mut resolved, timeouts) {
                Ok(p) => p,
                Err(e) => {
                    warn!(spec = %spec, error = %e, "failed to create provider after discovery");
                    return;
                }
            };
            model_slot.store(Arc::new(ModelSlot {
                model: resolved,
                provider: Arc::from(provider),
            }));
        });
        fetch_all_models(|batch| merge_batch(&bg, batch, &warn_tx), Some(done)).await;
    });
    BackgroundModels {
        available,
        warn_rx,
        warn_tx,
        task,
    }
}

fn restore_session(app: &mut App, handles: &AgentHandles) {
    app.permissions
        .load_session_rules(crate::app::session_state::stored_to_rules(
            &app.state.session.meta.session_rules,
        ));
    *handles
        .tool_outputs
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = app.state.session.tool_outputs.clone();
    app.restore_display();
    for w in app.state.warnings.drain(..) {
        app.status_bar.flash(w);
    }
}

impl<'t> EventLoop<'t> {
    pub(crate) fn new(
        terminal: &'t mut ratatui::DefaultTerminal,
        params: EventLoopParams,
    ) -> Result<Self> {
        let EventLoopParams {
            mut model,
            needs_login,
            commands,
            session,
            storage,
            config,
            ui_config,
            input_history_size,
            permissions,
            timeouts,
            exit_on_done,
            lua_command_reader,
            keymap_reader,
            hint_reader,
            ui_action_rx,
            lua_event_handle,
            dashboard,
        } = params;

        std::thread::spawn(crate::highlight::warmup);
        crate::update::spawn_check();

        let storage_writer = Arc::new(StorageWriter::new(storage.clone()));
        let (shell_tx, shell_rx) = flume::unbounded::<ShellEvent>();

        let resumed = !session.messages.is_empty();
        let initial_history = session.messages.clone();
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());

        let provider: Arc<dyn Provider> = if needs_login {
            Arc::from(maki_providers::provider::from_model_fallback(
                &mut model, timeouts,
            ))
        } else {
            Arc::from(from_model(&mut model, timeouts).context("create provider")?)
        };
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: model.clone(),
            provider,
        }));
        let bg = spawn_model_fetch(&model_slot, timeouts);
        let handles = AgentHandles::spawn(
            &model_slot,
            initial_history,
            config.clone(),
            ui_config.tool_output_lines,
            &permissions,
            cwd.clone(),
            Some(session.id.clone()),
            timeouts,
            lua_event_handle.clone(),
        );

        let custom_commands: Arc<[CustomCommand]> = Arc::from(commands);
        let spawn_ctx = SpawnContext {
            storage: storage.clone(),
            ui_config,
            input_history_size,
            lua_command_reader: lua_command_reader.clone(),
            keymap_reader: keymap_reader.clone(),
            hint_reader: hint_reader.clone(),
            lua_event_handle: lua_event_handle.clone(),
            custom_commands: Arc::clone(&custom_commands),
            cwd,
        };
        let mut app = App::new(
            &model,
            session,
            storage,
            bg.available.clone(),
            handles.mcp_reader(),
            handles.mcp_config_errors.clone(),
            lua_command_reader,
            keymap_reader,
            hint_reader,
            Arc::clone(&storage_writer),
            ui_config,
            input_history_size,
            Arc::clone(&permissions),
            custom_commands,
        );
        app.exit_on_done = exit_on_done;
        app.lua_event_handle = lua_event_handle;
        app.dashboard = dashboard;
        if dashboard {
            app.open_dashboard();
        }

        if needs_login {
            app.login_picker.open(app.storage.clone());
        }

        handles.apply_to_app(&mut app);

        if !handles.mcp_config_errors.is_empty() {
            app.flash(format!("MCP config error: {}", handles.mcp_config_errors));
        }

        if resumed {
            restore_session(&mut app, &handles);
        }

        Ok(Self {
            terminal,
            sessions: vec![SessionRuntime {
                app,
                handles,
                shell_tx,
                shell_rx,
            }],
            focused: 0,
            model_slot,
            config,
            permissions,
            warn_rx: bg.warn_rx,
            warn_tx: bg.warn_tx,
            available_models: bg.available,
            storage_writer,
            timeouts,
            ui_action_rx,
            spawn_ctx,
            _model_fetch_task: bg.task,
        })
    }

    pub(crate) fn run(mut self, initial_prompt: Option<String>) -> Result<(Option<String>, i32)> {
        if let Some(prompt) = initial_prompt {
            let sub = Submission {
                text: prompt,
                images: Vec::new(),
            };
            let actions = self.sessions[self.focused].app.handle_submit(sub);
            self.dispatch(self.focused, actions);
        }
        loop {
            self.tick();
            let had_agent_msg = self.drain_channels();
            self.terminal.draw(|f| self.sessions[self.focused].app.view(f))?;

            if self.sessions[self.focused].app.exit_request != ExitRequest::None {
                return Ok(self.shutdown());
            }

            self.poll_and_handle_input(had_agent_msg)?;
        }
    }

    fn tick(&mut self) {
        // Only the focused session drives UI-affecting timers/pollers; the
        // background sessions still make agent progress via drain_channels.
        let rt = &mut self.sessions[self.focused];
        rt.app.tick_edge_scroll();
        rt.app.tick_error_expiry();
        rt.app.poll_image_paste();
        rt.app.btw_modal.poll();
        rt.app.status_bar.poll_branch_update();
        rt.app.mcp_picker.refresh();
        rt.app.float_mgr.tick();
    }

    fn drain_channels(&mut self) -> bool {
        let mut had_agent_msg = false;

        // Every session advances, focused or not, so background work keeps
        // progressing while the user supervises from another session/dashboard.
        for idx in 0..self.sessions.len() {
            had_agent_msg |= self.drain_session(idx);
        }

        while let Ok(warning) = self.warn_rx.try_recv() {
            self.sessions[self.focused].app.flash(warning);
        }

        let slot_model = self.model_slot.load();
        if slot_model.model.context_window
            != self.sessions[self.focused].app.state.model.context_window
        {
            self.sessions[self.focused].app.update_model(&slot_model.model);
        }

        if let Some(rx) = &self.ui_action_rx {
            while let Ok(action) = rx.try_recv() {
                match action {
                    UiAction::Flash(msg) => {
                        self.sessions[self.focused].app.flash(msg);
                    }
                    UiAction::OpenEditor { path, reply_tx } => {
                        let code = match crate::terminal::open_in_editor(&path, self.terminal) {
                            Ok(code) => code,
                            Err(e) => {
                                self.sessions[self.focused].app.flash(e);
                                -1
                            }
                        };
                        let _ = reply_tx.send(code);
                    }
                    UiAction::OpenWin {
                        buf,
                        config,
                        focus,
                        event_tx,
                        cmd_rx,
                    } => {
                        self.sessions[self.focused].app
                            .float_mgr
                            .open(buf, config, focus, event_tx, cmd_rx);
                        if focus {
                            self.sessions[self.focused].app
                                .transition_plan(crate::app::mode::PlanTrigger::InteractivePrompt);
                        }
                    }
                }
            }
        }

        had_agent_msg
    }

    fn poll_and_handle_input(&mut self, had_agent_msg: bool) -> Result<()> {
        let has_pending_ui_action = self.ui_action_rx.as_ref().is_some_and(|rx| !rx.is_empty());
        let poll_duration = if had_agent_msg || has_pending_ui_action {
            Duration::ZERO
        } else if self.sessions[self.focused].app.is_animating() {
            Duration::from_millis(ANIMATION_INTERVAL_MS)
        } else {
            Duration::from_millis(IDLE_POLL_INTERVAL_MS)
        };

        if !event::poll(poll_duration)? {
            return Ok(());
        }

        if let Some(msg) = self.translate_input()? {
            let actions = self.sessions[self.focused].app.update(msg);
            self.dispatch(self.focused, actions);
        }
        Ok(())
    }

    fn translate_input(&mut self) -> Result<Option<Msg>> {
        let raw = event::read()?;
        match raw {
            Event::Key(key) if key.kind == KeyEventKind::Press => Ok(Some(Msg::Key(key))),
            Event::Key(_) => Ok(None),
            Event::Paste(text) => Ok(Some(Msg::Paste(text))),
            Event::Mouse(mouse) => Ok(self.translate_mouse(mouse)),
            _ => Ok(None),
        }
    }

    fn translate_mouse(&mut self, mouse: CtMouseEvent) -> Option<Msg> {
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let (scroll, extra) = aggregate_scroll(
                    mouse.column,
                    mouse.row,
                    scroll_delta(mouse.kind, self.sessions[self.focused].app.ui_config.mouse_scroll_lines),
                    self.sessions[self.focused].app.ui_config.mouse_scroll_lines,
                );
                if let Some(extra) = extra {
                    let actions = self.sessions[self.focused].app.update(scroll);
                    self.dispatch(self.focused, actions);
                    Some(extra)
                } else {
                    Some(scroll)
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (drag, extra) = coalesce_drag(mouse);
                let actions = self.sessions[self.focused].app.update(Msg::Mouse(drag));
                self.dispatch(self.focused, actions);
                extra
            }
            _ => Some(Msg::Mouse(mouse)),
        }
    }

    fn drain_session(&mut self, idx: usize) -> bool {
        while let Ok(event) = self.sessions[idx].shell_rx.try_recv() {
            self.sessions[idx].app.handle_shell_event(event);
        }

        let mut had_agent_msg = false;
        loop {
            match self.sessions[idx].handles.agent_rx.try_recv() {
                Ok(envelope) => {
                    had_agent_msg = true;
                    let actions = self.sessions[idx].app.update(Msg::Agent(Box::new(envelope)));
                    self.dispatch(idx, actions);
                }
                Err(flume::TryRecvError::Disconnected)
                    if self.sessions[idx].app.status == Status::Streaming =>
                {
                    self.sessions[idx].app.status =
                        Status::error("agent stopped unexpectedly".into());
                    break;
                }
                Err(_) => break,
            }
        }
        had_agent_msg
    }

    fn dispatch(&mut self, idx: usize, actions: Vec<Action>) {
        for action in actions {
            self.handle_action(idx, action);
        }
    }

    /// Build a fresh background `SessionRuntime` for the given stored session,
    /// sharing the process-wide resources captured in `spawn_ctx`.
    fn build_runtime(&self, session: AppSession, initial_history: Vec<Message>) -> SessionRuntime {
        let ctx = &self.spawn_ctx;
        let (shell_tx, shell_rx) = flume::unbounded::<ShellEvent>();
        let model = self.model_slot.load().model.clone();

        let handles = AgentHandles::spawn(
            &self.model_slot,
            initial_history,
            self.config.clone(),
            ctx.ui_config.tool_output_lines,
            &self.permissions,
            ctx.cwd.clone(),
            Some(session.id.clone()),
            self.timeouts,
            ctx.lua_event_handle.clone(),
        );

        let mut app = App::new(
            &model,
            session,
            ctx.storage.clone(),
            Arc::clone(&self.available_models),
            handles.mcp_reader(),
            handles.mcp_config_errors.clone(),
            ctx.lua_command_reader.clone(),
            ctx.keymap_reader.clone(),
            ctx.hint_reader.clone(),
            Arc::clone(&self.storage_writer),
            ctx.ui_config,
            ctx.input_history_size,
            Arc::clone(&self.permissions),
            Arc::clone(&ctx.custom_commands),
        );
        app.lua_event_handle = ctx.lua_event_handle.clone();
        handles.apply_to_app(&mut app);
        if !handles.mcp_config_errors.is_empty() {
            app.flash(format!("MCP config error: {}", handles.mcp_config_errors));
        }

        SessionRuntime {
            app,
            handles,
            shell_tx,
            shell_rx,
        }
    }

    /// Move focus to the previous (-1) or next (+1) live session runtime,
    /// wrapping around. No-op when only one session is running.
    fn cycle_focus(&mut self, delta: i32) {
        let n = self.sessions.len();
        if n <= 1 {
            return;
        }
        let cur = self.focused as i32;
        let next = (cur + delta).rem_euclid(n as i32) as usize;
        self.focused = next;
    }

    /// Focus an existing runtime by session id, or attach the stored session as
    /// a new background runtime and focus it. Does not disturb other sessions.
    fn focus_session(&mut self, id: String) {
        if let Some(pos) = self
            .sessions
            .iter()
            .position(|rt| rt.app.state.session.id == id)
        {
            self.focused = pos;
            return;
        }

        let session = match AppSession::load(&id, &self.spawn_ctx.storage) {
            Ok(s) => s,
            Err(e) => {
                self.sessions[self.focused]
                    .app
                    .flash(format!("Failed to load session: {e}"));
                return;
            }
        };
        let history = session.messages.clone();
        let mut rt = self.build_runtime(session, history.clone());
        if !history.is_empty() {
            restore_session(&mut rt.app, &rt.handles);
        }
        self.sessions.push(rt);
        self.focused = self.sessions.len() - 1;
    }

    /// Create a brand-new background session, optionally seeded with a task
    /// prompt, and focus it.
    fn spawn_session(&mut self, submission: Option<Box<Submission>>) {
        let cwd = self.spawn_ctx.cwd.to_string_lossy().into_owned();
        let model_spec = self.model_slot.load().model.spec();
        let mut session = AppSession::new(&model_spec, &cwd);
        if let Err(e) = session.save(&self.spawn_ctx.storage) {
            self.sessions[self.focused]
                .app
                .flash(format!("Failed to create session: {e}"));
            return;
        }
        let rt = self.build_runtime(session, Vec::new());
        self.sessions.push(rt);
        self.focused = self.sessions.len() - 1;

        if let Some(sub) = submission {
            let sub = *sub;
            if !sub.is_empty() {
                let idx = self.focused;
                let actions = self.sessions[idx].app.handle_submit(sub);
                self.dispatch(idx, actions);
            }
        }
    }

    fn respawn_agent(&mut self, idx: usize, history: Vec<Message>) {
        let model_slot = Arc::clone(&self.model_slot);
        let permissions = Arc::clone(&self.permissions);
        let config = self.config.clone();
        let rt = &mut self.sessions[idx];
        let lua_handle = rt.app.lua_event_handle.clone();
        let tool_output_lines = rt.app.ui_config.tool_output_lines;
        rt.handles.respawn(
            history,
            &model_slot,
            config,
            tool_output_lines,
            &permissions,
            &mut rt.app,
            lua_handle,
        );
    }

    fn handle_action(&mut self, idx: usize, action: Action) {
        match action {
            Action::SendMessage(input) => {
                let mut input = *input;
                input.preamble = self.sessions[idx].app.shell.drain_results();
                let run_id = self.sessions[idx].app.run_id;
                self.sessions[idx].handles.queue.push(QueueItem::Message {
                    text: input.message.clone(),
                    image_count: input.images.len(),
                    input,
                    run_id,
                    displayed: true,
                });
            }
            Action::CancelAgent { run_id } => {
                let _ = self.sessions[idx]
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::Cancel { run_id });
            }
            Action::CancelSubagent { tool_use_id } => {
                let _ = self.sessions[idx]
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::CancelSubagent { tool_use_id });
            }
            Action::NewSession => {
                self.respawn_agent(idx, Vec::new());
            }
            Action::LoadSession(loaded) => {
                let loaded = *loaded;
                if loaded.model_spec != self.model_slot.load().model.spec()
                    && let Ok(mut new_model) = Model::from_spec(&loaded.model_spec)
                    && let Ok(new_provider) = from_model(&mut new_model, self.timeouts)
                {
                    self.sessions[idx].app.usage_slot.store(None);
                    self.model_slot.store(Arc::new(ModelSlot {
                        model: new_model,
                        provider: Arc::from(new_provider),
                    }));
                }
                self.respawn_agent(idx, loaded.messages);
                *self.sessions[idx]
                    .handles
                    .tool_outputs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = loaded.tool_outputs;
            }
            Action::ChangeModel(spec) => self.change_model(spec),
            Action::RefreshProvider { slug } => self.refresh_provider(slug),
            Action::AssignTier(spec, tier) => {
                maki_providers::model_registry::set_and_persist(spec, tier, &self.sessions[idx].app.storage);
            }
            Action::UnassignTier(spec, tier) => {
                maki_providers::model_registry::unset_and_persist(&spec, tier, &self.sessions[idx].app.storage);
            }
            Action::Compact => {
                self.sessions[idx].handles.queue.push(QueueItem::Compact {
                    run_id: self.sessions[idx].app.run_id,
                });
            }
            Action::ToggleMcp(server_name, enabled) => {
                self.sessions[idx].handles.send_mcp(McpCommand::Toggle {
                    server: server_name,
                    enabled,
                });
            }
            Action::ShellCommand {
                id,
                command,
                visible,
            } => {
                let (trigger, cancel) = CancelToken::new();
                self.sessions[idx].app.shell.add_trigger(trigger);
                spawn_shell(
                    command,
                    id,
                    visible,
                    self.sessions[idx].shell_tx.clone(),
                    cancel,
                    self.config.clone(),
                );
            }
            Action::OpenEditor(path) => {
                if let Err(e) = terminal::open_in_editor(&path, self.terminal) {
                    self.sessions[idx].app.flash(e);
                }
            }
            Action::EditInputInEditor => {
                let current_text = self.sessions[idx].app.input_box.buffer.value();
                match terminal::edit_temp_content(&current_text, self.terminal) {
                    Ok(edited) => self.sessions[idx].app.input_box.set_input(edited),
                    Err(e) => self.sessions[idx].app.flash(e),
                }
            }
            Action::Btw(question) => {
                let slot = self.model_slot.load();
                self.sessions[idx].app.start_btw(
                    question,
                    Arc::clone(&slot.provider),
                    slot.model.clone(),
                );
            }
            Action::FocusSession(id) => self.focus_session(id),
            Action::SpawnSession(prompt) => self.spawn_session(prompt),
            Action::FocusPrevSession => self.cycle_focus(-1),
            Action::FocusNextSession => self.cycle_focus(1),
            Action::ShowDashboard => {
                self.sessions[idx].app.open_dashboard();
            }
            Action::Suspend => terminal::suspend(self.terminal),
            Action::RefreshModels => self.refresh_models(),
            Action::RefreshUsage => self.refresh_usage(),
            Action::Quit => {}
        }
    }

    fn change_model(&mut self, spec: String) {
        match Model::from_spec(&spec) {
            Ok(mut new_model) => match from_model(&mut new_model, self.timeouts) {
                Ok(new_provider) => {
                    self.sessions[self.focused].app.update_model(&new_model);
                    self.sessions[self.focused].app.record_recent_model(&spec);
                    self.sessions[self.focused].app.usage_slot.store(None);
                    self.model_slot.store(Arc::new(ModelSlot {
                        model: new_model,
                        provider: Arc::from(new_provider),
                    }));
                }
                Err(e) => self.sessions[self.focused].app.flash(format!("Failed to create provider: {e}")),
            },
            Err(e) => self.sessions[self.focused].app.flash(format!("Invalid model: {e}")),
        }
    }

    fn refresh_models(&self) {
        let available = Arc::clone(&self.available_models);
        let warn_tx = self.warn_tx.clone();
        available.store(None);
        smol::spawn(async move {
            fetch_all_models(|batch| merge_batch(&available, batch, &warn_tx), None).await;
        })
        .detach();
    }

    fn refresh_usage(&self) {
        let provider = Arc::clone(&self.model_slot.load().provider);
        let slot = Arc::clone(&self.sessions[self.focused].app.usage_slot);
        slot.store(Some(Arc::new(UsageFetchState::Loading)));
        smol::spawn(async move {
            let state = match provider.fetch_usage().await {
                Ok(Some(usage)) => UsageFetchState::Ready(usage),
                Ok(None) => UsageFetchState::Unsupported,
                Err(e) => UsageFetchState::Error(e.user_message()),
            };
            slot.store(Some(Arc::new(state)));
        })
        .detach();
    }

    fn refresh_provider(&mut self, slug: String) {
        let current = self.model_slot.load();
        let current_model = &current.model;

        if current_model.provider.to_string() == slug {
            let mut m = current_model.clone();
            if let Ok(provider) = maki_providers::provider::from_model(&mut m, self.timeouts) {
                self.sessions[self.focused].app.usage_slot.store(None);
                self.model_slot.store(Arc::new(ModelSlot {
                    model: m,
                    provider: Arc::from(provider),
                }));
            }
        } else if let Some(builtin) = maki_config::providers::builtin_provider(&slug) {
            let spec = builtin.default_model.to_string();
            self.change_model(spec);
        }
    }

    fn shutdown(mut self) -> (Option<String>, i32) {
        let focused = self.focused;
        let exit_code = self.sessions[focused].app.exit_request.code();
        let session_id = self.sessions[focused]
            .app
            .has_content()
            .then(|| self.sessions[focused].app.state.session.id.clone());

        for rt in self.sessions.drain(..) {
            let SessionRuntime {
                mut app, handles, ..
            } = rt;
            maki_agent::mcp::kill_process_groups(&handles.mcp_reader().load().pids);
            app.cmd_tx = None;
            app.answer_tx = None;
            drop(app);
            handles.shutdown(Duration::from_secs(3));
        }

        match Arc::try_unwrap(self.storage_writer) {
            Ok(writer) => writer.shutdown(Duration::from_secs(3)),
            Err(_) => {
                warn!("storage writer has outstanding references, skipping graceful shutdown")
            }
        }
        (session_id, exit_code)
    }
}

fn scroll_delta(kind: MouseEventKind, lines: u32) -> i32 {
    if kind == MouseEventKind::ScrollUp {
        lines as i32
    } else {
        -(lines as i32)
    }
}

fn aggregate_scroll(
    column: u16,
    row: u16,
    mut delta: i32,
    scroll_lines: u32,
) -> (Msg, Option<Msg>) {
    while event::poll(Duration::ZERO).unwrap_or(false) {
        if let Ok(Event::Mouse(next)) = event::read() {
            match next.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    delta += scroll_delta(next.kind, scroll_lines);
                }
                _ => return (Msg::Scroll { column, row, delta }, Some(Msg::Mouse(next))),
            }
        } else {
            break;
        }
    }
    (Msg::Scroll { column, row, delta }, None)
}

fn coalesce_drag(mut latest: CtMouseEvent) -> (CtMouseEvent, Option<Msg>) {
    while event::poll(Duration::ZERO).unwrap_or(false) {
        if let Ok(Event::Mouse(next)) = event::read() {
            if matches!(next.kind, MouseEventKind::Drag(MouseButton::Left)) {
                latest = next;
            } else {
                return (latest, Some(Msg::Mouse(next)));
            }
        } else {
            break;
        }
    }
    (latest, None)
}
