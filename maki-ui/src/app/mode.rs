use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::agent::QueuedMessage;
use crate::components::Status;
use crate::theme;
use maki_agent::{AgentInput, AgentMode};
use maki_storage::DataDir;
use maki_storage::plans;
use ratatui::style::{Color, Modifier, Style};

use super::App;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Build,
    Plan,
}

pub(crate) enum PlanTrigger {
    WriteDone,
    QuestionAsked,
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
    Ready(PathBuf),
}

impl PlanState {
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::None => Option::None,
            Self::Drafting(p) | Self::Ready(p) => Some(p),
        }
    }

    pub(crate) fn mark_ready(&mut self) {
        if let Self::Drafting(p) = self {
            *self = Self::Ready(std::mem::take(p));
        }
    }

    pub(crate) fn mark_drafting(&mut self) {
        if let Self::Ready(p) = self {
            *self = Self::Drafting(std::mem::take(p));
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    pub(crate) fn allocate_path(&mut self, storage: &DataDir) {
        if matches!(self, Self::None) {
            *self = Self::Drafting(
                plans::new_plan_path(storage).unwrap_or_else(|_| PathBuf::from("plans/plan.md")),
            );
        }
    }
}

impl App {
    pub(super) fn transition_plan(&mut self, trigger: PlanTrigger) {
        if self.state.mode != Mode::Plan {
            return;
        }
        match trigger {
            PlanTrigger::WriteDone => {
                if self.state.plan.is_ready() {
                    return;
                }
                self.state.plan.mark_ready();
                self.plan_form.open();
            }
            PlanTrigger::QuestionAsked => {
                if self.state.plan.is_ready() {
                    self.state.plan.mark_drafting();
                    self.plan_form.close();
                }
            }
        }
    }

    pub(super) fn enter_plan(&mut self) {
        self.state.plan.allocate_path(&self.storage);
        self.state.mode = Mode::Plan;
    }

    pub(super) fn toggle_mode(&mut self) -> Vec<super::Action> {
        match self.state.mode {
            Mode::Build => self.enter_plan(),
            Mode::Plan => self.state.mode = Mode::Build,
        };
        vec![]
    }

    pub(super) fn agent_mode(&self) -> AgentMode {
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

    pub(crate) fn build_agent_input(&self, msg: &QueuedMessage) -> AgentInput {
        AgentInput {
            message: msg.text.clone(),
            mode: self.agent_mode(),
            images: msg.images.clone(),
            thinking: self.state.thinking,
            ..Default::default()
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
        self.input_box
            .buffer
            .lines()
            .first()
            .is_some_and(|l| l.starts_with('!'))
    }

    pub(super) fn effective_mode_color(&self) -> Color {
        if self.is_bash_input() {
            theme::current().mode_bash
        } else {
            self.state.mode.color()
        }
    }

    pub(super) fn separator_style(&self) -> Style {
        if self.status == Status::Streaming {
            theme::current().input_border
        } else {
            Style::new().fg(self.effective_mode_color())
        }
    }
}
