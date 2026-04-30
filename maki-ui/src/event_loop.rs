use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use color_eyre::Result;
use color_eyre::eyre::Context;

use crossterm::event::{
    self, Event, KeyEventKind, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use maki_agent::command::CustomCommand;
use maki_agent::permissions::PermissionManager;
use maki_agent::skill::Skill;
use maki_agent::{AgentConfig, CancelToken, McpCommand, RawRenderHints};
use maki_config::UiConfig;
use maki_providers::Timeouts;
use maki_providers::provider::{Provider, fetch_all_models, from_model};
use maki_providers::{Message, Model};
use maki_storage::StateDir;
use tracing::warn;

use crate::AppSession;
use crate::agent::{AgentCommand, AgentHandles, ModelSlot, shared_queue::QueueItem};
use crate::app::shell::{ShellEvent, spawn_shell};
use crate::app::{App, Msg};
#[cfg(feature = "demo")]
use crate::components;
use crate::components::input::Submission;
use crate::components::{Action, ExitRequest, Status};

#[cfg(feature = "demo")]
use crate::mock;
use crate::storage_writer::StorageWriter;
use crate::terminal;

const ANIMATION_INTERVAL_MS: u64 = 16;
const IDLE_POLL_INTERVAL_MS: u64 = 100;

pub type BufClickHandler = Arc<dyn Fn(&str, u32) + Send + Sync>;

pub struct EventLoopParams {
    pub model: Model,
    pub skills: Vec<Skill>,
    pub commands: Vec<CustomCommand>,
    pub session: AppSession,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: Timeouts,
    pub exit_on_done: bool,
    pub plugin_render_hints: Vec<(Arc<str>, RawRenderHints)>,
    pub buf_click: Option<BufClickHandler>,
    #[cfg(feature = "demo")]
    pub demo: bool,
}

pub(crate) struct EventLoop<'t> {
    terminal: &'t mut ratatui::DefaultTerminal,
    app: App,
    handles: AgentHandles,
    model_slot: Arc<ArcSwap<ModelSlot>>,
    skills: Arc<[Skill]>,
    config: AgentConfig,
    permissions: Arc<PermissionManager>,
    shell_tx: flume::Sender<ShellEvent>,
    shell_rx: flume::Receiver<ShellEvent>,
    warn_rx: flume::Receiver<String>,
    storage_writer: Arc<StorageWriter>,
    timeouts: Timeouts,
    _model_fetch_task: smol::Task<()>,
}

struct BackgroundModels {
    available: Arc<ArcSwapOption<Vec<String>>>,
    warn_rx: flume::Receiver<String>,
    task: smol::Task<()>,
}

fn spawn_model_fetch() -> BackgroundModels {
    let available: Arc<ArcSwapOption<Vec<String>>> = Arc::new(ArcSwapOption::empty());
    let bg = Arc::clone(&available);
    let (warn_tx, warn_rx) = flume::unbounded::<String>();
    let task = smol::spawn(async move {
        let warn_tx = warn_tx;
        fetch_all_models(|batch| {
            for w in batch.warnings {
                let _ = warn_tx.try_send(w);
            }
            if batch.models.is_empty() {
                return;
            }
            let mut merged = bg.load().as_deref().cloned().unwrap_or_default();
            merged.extend(batch.models);
            bg.store(Some(Arc::new(merged)));
        })
        .await;
    });
    BackgroundModels {
        available,
        warn_rx,
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

#[cfg(feature = "demo")]
fn apply_demo(app: &mut App) {
    app.status = components::Status::Streaming;
    app.run_id = 1;
    for event in mock::mock_events() {
        match event {
            mock::MockEvent::User(text) => app.main_chat().push_user_message(&text),
            mock::MockEvent::Error(text) => {
                app.main_chat().push(components::DisplayMessage::new(
                    components::DisplayRole::Error,
                    text,
                ));
            }
            mock::MockEvent::Flush => app.flush_all_chats(),
            mock::MockEvent::Agent(envelope) => {
                app.update(Msg::Agent(envelope));
            }
        }
    }
    app.flush_all_chats();
    if let Some(idx) = app.chat_index_for(mock::question_tool_id()) {
        app.set_demo_questions(idx, mock::mock_questions());
    }
    app.status = components::Status::Idle;
}

impl<'t> EventLoop<'t> {
    pub(crate) fn new(
        terminal: &'t mut ratatui::DefaultTerminal,
        params: EventLoopParams,
    ) -> Result<Self> {
        let EventLoopParams {
            model,
            skills,
            commands,
            session,
            storage,
            config,
            ui_config,
            input_history_size,
            permissions,
            timeouts,
            exit_on_done,
            plugin_render_hints,
            buf_click,
            #[cfg(feature = "demo")]
            demo,
        } = params;

        std::thread::spawn(crate::highlight::warmup);
        crate::update::spawn_check();

        let bg = spawn_model_fetch();
        let storage_writer = Arc::new(StorageWriter::new(storage.clone()));
        let (shell_tx, shell_rx) = flume::unbounded::<ShellEvent>();

        let resumed = !session.messages.is_empty();
        let initial_history = session.messages.clone();
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());

        let provider: Arc<dyn Provider> =
            Arc::from(from_model(&model, timeouts).context("create provider")?);
        let skills: Arc<[Skill]> = Arc::from(skills);
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: model.clone(),
            provider,
        }));
        let handles = AgentHandles::spawn(
            &model_slot,
            initial_history,
            &skills,
            config.clone(),
            ui_config.tool_output_lines,
            &permissions,
            cwd,
            Some(session.id.clone()),
            timeouts,
        );

        let custom_commands: Arc<[CustomCommand]> = Arc::from(commands);
        let mut app = App::new(
            &model,
            session,
            storage,
            bg.available,
            handles.mcp_reader(),
            Arc::clone(&storage_writer),
            ui_config,
            input_history_size,
            Arc::clone(&permissions),
            custom_commands,
        );
        app.exit_on_done = exit_on_done;
        app.buf_click = buf_click;

        #[cfg(feature = "demo")]
        if demo {
            apply_demo(&mut app);
        }

        handles.apply_to_app(&mut app);

        if !plugin_render_hints.is_empty() {
            app.main_chat().register_plugin_hints(plugin_render_hints);
        }

        if resumed {
            restore_session(&mut app, &handles);
        }

        Ok(Self {
            terminal,
            app,
            handles,
            model_slot,
            skills,
            config,
            permissions,
            shell_tx,
            shell_rx,
            warn_rx: bg.warn_rx,
            storage_writer,
            timeouts,
            _model_fetch_task: bg.task,
        })
    }

    pub(crate) fn run(mut self, initial_prompt: Option<String>) -> Result<(Option<String>, i32)> {
        if let Some(prompt) = initial_prompt {
            let sub = Submission {
                text: prompt,
                images: Vec::new(),
            };
            let actions = self.app.handle_submit(sub);
            self.dispatch(actions);
        }
        loop {
            self.tick();
            self.terminal.draw(|f| self.app.view(f))?;
            let had_agent_msg = self.drain_channels();

            if self.app.exit_request != ExitRequest::None {
                return Ok(self.shutdown());
            }

            self.poll_and_handle_input(had_agent_msg)?;
        }
    }

    fn tick(&mut self) {
        self.app.tick_edge_scroll();
        self.app.tick_error_expiry();
        self.app.poll_image_paste();
        self.app.btw_modal.poll();
        self.app.status_bar.poll_branch_update();
        self.app.mcp_picker.refresh();
    }

    fn drain_channels(&mut self) -> bool {
        while let Ok(event) = self.shell_rx.try_recv() {
            self.app.handle_shell_event(event);
        }

        let mut had_agent_msg = false;
        loop {
            match self.handles.agent_rx.try_recv() {
                Ok(envelope) => {
                    had_agent_msg = true;
                    let actions = self.app.update(Msg::Agent(Box::new(envelope)));
                    self.dispatch(actions);
                }
                Err(flume::TryRecvError::Disconnected) if self.app.status == Status::Streaming => {
                    self.app.status = Status::error("agent stopped unexpectedly".into());
                    break;
                }
                Err(_) => break,
            }
        }

        while let Ok(warning) = self.warn_rx.try_recv() {
            self.app.flash(warning);
        }

        had_agent_msg
    }

    fn poll_and_handle_input(&mut self, had_agent_msg: bool) -> Result<()> {
        let poll_duration = if had_agent_msg {
            Duration::ZERO
        } else if self.app.is_animating() {
            Duration::from_millis(ANIMATION_INTERVAL_MS)
        } else {
            Duration::from_millis(IDLE_POLL_INTERVAL_MS)
        };

        if !event::poll(poll_duration)? {
            return Ok(());
        }

        if let Some(msg) = self.translate_input()? {
            let actions = self.app.update(msg);
            self.dispatch(actions);
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
                    scroll_delta(mouse.kind, self.app.ui_config.mouse_scroll_lines),
                    self.app.ui_config.mouse_scroll_lines,
                );
                if let Some(extra) = extra {
                    let actions = self.app.update(scroll);
                    self.dispatch(actions);
                    Some(extra)
                } else {
                    Some(scroll)
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (drag, extra) = coalesce_drag(mouse);
                let actions = self.app.update(Msg::Mouse(drag));
                self.dispatch(actions);
                extra
            }
            _ => Some(Msg::Mouse(mouse)),
        }
    }

    fn dispatch(&mut self, actions: Vec<Action>) {
        for action in actions {
            self.handle_action(action);
        }
    }

    fn respawn_agent(&mut self, history: Vec<Message>) {
        self.handles.respawn(
            history,
            &self.model_slot,
            &self.skills,
            self.config.clone(),
            self.app.ui_config.tool_output_lines,
            &self.permissions,
            &mut self.app,
        );
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::SendMessage(input) => {
                let mut input = *input;
                input.preamble = self.app.shell.drain_results();
                let run_id = self.app.run_id;
                self.handles.queue.push(QueueItem::Message {
                    text: input.message.clone(),
                    image_count: input.images.len(),
                    input,
                    run_id,
                });
            }
            Action::CancelAgent { run_id } => {
                let _ = self
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::Cancel { run_id });
            }
            Action::NewSession => {
                self.respawn_agent(Vec::new());
            }
            Action::LoadSession(loaded) => {
                let loaded = *loaded;
                if loaded.model_spec != self.model_slot.load().model.spec()
                    && let Ok(new_model) = Model::from_spec(&loaded.model_spec)
                    && let Ok(new_provider) = from_model(&new_model, self.timeouts)
                {
                    self.model_slot.store(Arc::new(ModelSlot {
                        model: new_model,
                        provider: Arc::from(new_provider),
                    }));
                }
                self.respawn_agent(loaded.messages);
                *self
                    .handles
                    .tool_outputs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = loaded.tool_outputs;
            }
            Action::ChangeModel(spec) => self.change_model(spec),
            Action::AssignTier(spec, tier) => {
                maki_providers::tier_map::set_and_persist(spec, tier, &self.app.storage);
            }
            Action::Compact => {
                self.handles.queue.push(QueueItem::Compact {
                    run_id: self.app.run_id,
                });
            }
            Action::ToggleMcp(server_name, enabled) => {
                self.handles.send_mcp(McpCommand::Toggle {
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
                self.app.shell.add_trigger(trigger);
                spawn_shell(
                    command,
                    id,
                    visible,
                    self.shell_tx.clone(),
                    cancel,
                    self.config.clone(),
                );
            }
            Action::OpenEditor(path) => {
                if let Err(e) = terminal::open_in_editor(&path, self.terminal) {
                    self.app.flash(e);
                } else {
                    self.app.refresh_memory_entry(&path);
                }
            }
            Action::EditInputInEditor => {
                let current_text = self.app.input_box.buffer.value();
                match terminal::edit_temp_content(&current_text, self.terminal) {
                    Ok(edited) => self.app.input_box.set_input(edited),
                    Err(e) => self.app.flash(e),
                }
            }
            Action::Btw(question) => {
                let slot = self.model_slot.load();
                self.app
                    .start_btw(question, Arc::clone(&slot.provider), slot.model.clone());
            }
            Action::Suspend => terminal::suspend(self.terminal),
            Action::Quit => {}
        }
    }

    fn change_model(&mut self, spec: String) {
        match Model::from_spec(&spec) {
            Ok(new_model) => match from_model(&new_model, self.timeouts) {
                Ok(new_provider) => {
                    self.app.update_model(&new_model);
                    self.model_slot.store(Arc::new(ModelSlot {
                        model: new_model,
                        provider: Arc::from(new_provider),
                    }));
                }
                Err(e) => self.app.flash(format!("Failed to create provider: {e}")),
            },
            Err(e) => self.app.flash(format!("Invalid model: {e}")),
        }
    }

    fn shutdown(mut self) -> (Option<String>, i32) {
        let exit_code = self.app.exit_request.code();
        let session_id = self
            .app
            .has_content()
            .then(|| self.app.state.session.id.clone());
        maki_agent::mcp::kill_process_groups(&self.handles.mcp_reader().load().pids);
        self.app.cmd_tx = None;
        self.app.answer_tx = None;
        drop(self.app);
        self.handles.shutdown(Duration::from_secs(3));
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
