use std::borrow::Cow;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};

use crate::agent::QueuedMessage;
use crate::components::DisplayMessage;
use crate::theme;
use maki_agent::{AgentInput, AgentMode, InputSource};
use maki_lua::agent_autocmd::HostEvent;
use maki_providers::RequestOptions;
use maki_storage::StateDir;
use maki_storage::plans;
use ratatui::style::{Color, Modifier, Style};
use tracing::debug;

use super::{App, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Build,
    Plan,
}

impl Mode {
    pub(crate) fn color(&self) -> Color {
        match self {
            Self::Build => theme::current().mode_build,
            Self::Plan => theme::current().mode_plan,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PlanState {
    #[default]
    None,
    Drafting(PathBuf),
    /// Offered for review. `revision` is the content this transcript was
    /// offered, `None` for a plan restored from disk until it is offered again.
    Ready {
        path: PathBuf,
        revision: Option<String>,
    },
}

impl PlanState {
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::None => Option::None,
            Self::Drafting(path) | Self::Ready { path, .. } => Some(path),
        }
    }

    /// Take {content} up for review, unless it is the revision already
    /// offered.
    pub(crate) fn offer(&mut self, content: &str) -> bool {
        let path = match self {
            Self::None => return false,
            Self::Ready {
                revision: Some(revision),
                ..
            } if revision == content => return false,
            Self::Drafting(path) | Self::Ready { path, .. } => mem::take(path),
        };
        *self = Self::Ready {
            path,
            revision: Some(content.to_owned()),
        };
        true
    }

    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    pub(crate) fn allocate_path(&mut self, storage: &StateDir) {
        if matches!(self, Self::None) {
            *self = Self::Drafting(
                plans::new_plan_path(storage).unwrap_or_else(|_| PathBuf::from("plans/plan.md")),
            );
        }
    }
}

impl App {
    /// The run in front ended and nothing is queued behind it, so the plan
    /// file holds what the agent hands the user. A failed run leaves the
    /// error to deal with first.
    pub(crate) fn on_queue_drained(&mut self) {
        if self.state.mode == Mode::Plan && self.status == Status::Idle {
            self.offer_plan_revision();
        }
    }

    /// Put the plan file up for review when its content is a revision this
    /// transcript has not been offered yet: shown in full, then the form and
    /// `PlanReady`. Reading the file rather than watching writes
    /// catches every way it changed, nested and delegated tool calls too.
    pub(super) fn offer_plan_revision(&mut self) -> bool {
        let Some(path) = self.state.plan.path() else {
            return false;
        };
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                debug!(path = %path.display(), %error, "no plan revision to offer");
                return false;
            }
        };
        let path = path.display().to_string();
        if !self.state.plan.offer(&content) {
            return false;
        }
        self.plan_form.reset();
        self.main_chat()
            .push(DisplayMessage::plan(content, path.clone()));
        self.offer_plan_form(Some(&path));
        self.fire_session_event(HostEvent::PlanReady { path: &path });
        true
    }

    pub(super) fn enter_plan(&mut self) {
        self.state.plan.allocate_path(&self.storage);
        self.state.mode = Mode::Plan;
    }

    /// The mode switch behind `maki.session.set_mode`, i.e. what Tab does
    /// without the toggling. Build mode is what makes the next prompt an
    /// implementation of the plan instead of another draft.
    pub(crate) fn set_mode(&mut self, mode: Mode) {
        match mode {
            Mode::Plan => self.enter_plan(),
            Mode::Build => self.state.mode = Mode::Build,
        }
    }

    pub(super) fn toggle_mode(&mut self) -> Vec<super::Action> {
        match self.state.mode {
            Mode::Build => self.enter_plan(),
            Mode::Plan => self.state.mode = Mode::Build,
        };
        vec![]
    }

    pub(crate) fn agent_mode(&self) -> AgentMode {
        match self.state.mode {
            Mode::Plan => match self.state.plan.path() {
                Some(p) => AgentMode::Plan(p.to_path_buf()),
                None => {
                    debug_assert!(false, "Plan mode without path - invariant violated");
                    AgentMode::Build
                }
            },
            Mode::Build => AgentMode::Build,
        }
    }

    /// Background subagents are capped by this same value, so they can never
    /// disagree with the chat's next prompt.
    pub(crate) fn request_options(&self) -> RequestOptions {
        RequestOptions {
            thinking: self.state.thinking,
            fast: self.state.fast,
        }
    }

    pub(crate) fn build_agent_input(&self, msg: &QueuedMessage) -> AgentInput {
        let RequestOptions { thinking, fast } = self.request_options();
        AgentInput {
            message: msg.text.clone(),
            mode: self.agent_mode(),
            images: msg.images.clone(),
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking,
            fast,
            workflow: self.state.workflow,
            prompt: None,
            source: InputSource::Tui,
        }
    }

    pub(super) fn mode_label(&self) -> (Cow<'static, str>, Style) {
        let label: Cow<'static, str> = if self.is_bash_input() {
            "[BASH]".into()
        } else {
            match self.state.mode {
                Mode::Build => "[BUILD]".into(),
                Mode::Plan => "[PLAN]".into(),
            }
        };
        let style = Style::new()
            .fg(self.effective_mode_color())
            .add_modifier(Modifier::BOLD);
        (label, style)
    }

    pub(crate) fn is_bash_input(&self) -> bool {
        self.input_box.buffer.value().starts_with('!')
    }

    pub(super) fn effective_mode_color(&self) -> Color {
        if self.is_bash_input() {
            theme::current().mode_bash
        } else {
            self.state.mode.color()
        }
    }

    /// Dim while the agent in front works, so a running task's tab reads
    /// busy like a streaming main chat.
    pub(super) fn separator_style(&self) -> Style {
        if self.active_chat_working() {
            theme::current().input_border
        } else {
            Style::new().fg(self.effective_mode_color())
        }
    }
}
