use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::QueuedMessage;
use crate::theme;
use maki_agent::{AgentInput, AgentMode, DoneReason, InputSource};
use maki_lua::agent_autocmd::HostEvent;
use maki_providers::RequestOptions;
use maki_storage::StateDir;
use maki_storage::plans;
use ratatui::style::{Color, Modifier, Style};

use super::{
    App, FLASH_PLAN_ACTION_LOST, FLASH_PLAN_DRAFT_INTERRUPTED, FLASH_PLAN_UNREADABLE_PREFIX,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Build,
    Plan,
}

pub(crate) enum PlanTrigger {
    /// A successful write to the plan file landed. It starts a review cycle
    /// but is not itself approval: the run that wrote it may still change it.
    WriteDone,
    /// A focused window means the plan is being discussed again.
    InteractivePrompt,
    /// User input in plan mode started a new drafting cycle.
    RefinementStarted,
    /// The run and everything queued behind it ended normally.
    Settled,
    /// The cycle ended without a normal end: error, cancellation or a limit.
    Interrupted,
}

/// The part of the plan lifecycle that is not the plan file itself. `Ready`
/// keeps meaning "the agent wrote the plan", as `maki.plan.read` documents;
/// approval waits for this cycle to settle.
#[derive(Default)]
pub(crate) struct PlanCycle {
    /// A drafting cycle is active, so the plan is not approvable however
    /// written its file is.
    pub(super) refining: bool,
    /// A successful write landed during this cycle.
    pub(super) written: bool,
    /// The content the user is allowed to approve. Cleared by a write and set
    /// again when a settled revision is offered, so implementing a file that
    /// moved underneath the review has to be approved again.
    pub(super) reviewed: Option<String>,
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

    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
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
    /// The plan-review policy for plugin windows, kept here rather than at
    /// the UI action so it can be tested on its own. Only a window that says
    /// it needs input is a question; a picker or info popup is not.
    pub(crate) fn on_window_opened(&mut self, needs_input: bool) {
        if needs_input {
            self.transition_plan(PlanTrigger::InteractivePrompt);
        }
    }

    pub(crate) fn transition_plan(&mut self, trigger: PlanTrigger) {
        match trigger {
            PlanTrigger::WriteDone => self.plan_wrote(),
            PlanTrigger::InteractivePrompt => {
                if self.state.mode == Mode::Plan {
                    self.begin_plan_refinement();
                }
            }
            PlanTrigger::RefinementStarted => {
                self.plan_last_done = None;
                self.begin_plan_refinement();
            }
            PlanTrigger::Settled => self.settle_plan(true),
            PlanTrigger::Interrupted => self.settle_plan(false),
        }
    }

    fn plan_wrote(&mut self) {
        if (self.state.mode != Mode::Plan && !self.plan_cycle.refining)
            || self.state.plan.path().is_none()
        {
            return;
        }
        self.begin_plan_refinement();
        self.plan_cycle.written = true;
        self.plan_cycle.reviewed = None;
    }

    /// A new drafting cycle owns the plan until the run and everything queued
    /// behind it settles. Hiding the form is what keeps a mid-run write from
    /// looking like an approval opportunity.
    fn begin_plan_refinement(&mut self) {
        if self.state.plan.path().is_none() {
            return;
        }
        let lost_pick = self.plan_answers.action.is_some();
        self.plan_answers.abandon();
        if lost_pick {
            self.flash(FLASH_PLAN_ACTION_LOST.into());
        }
        self.plan_cycle.refining = true;
        self.plan_form.on_plan_drafting();
        self.plan_form.forget_menu();
    }

    fn settle_plan(&mut self, success: bool) {
        if !self.plan_cycle.refining {
            return;
        }
        let written = self.plan_cycle.written;
        self.plan_cycle.refining = false;
        self.plan_cycle.written = false;
        self.plan_last_done = None;
        if !success {
            self.plan_answers.abandon();
            if written {
                self.plan_cycle.reviewed = None;
                self.flash(FLASH_PLAN_DRAFT_INTERRUPTED.into());
            }
            return;
        }
        if !written {
            return;
        }
        let Some(path) = self.state.plan.path().map(|path| path.to_path_buf()) else {
            return;
        };
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                self.plan_cycle.reviewed = None;
                self.flash(format!(
                    "{FLASH_PLAN_UNREADABLE_PREFIX} {}: {error}",
                    path.display()
                ));
                return;
            }
        };
        self.state.plan.mark_ready();
        self.plan_cycle.reviewed = Some(content);
        let path = path.display().to_string();
        self.offer_plan_form(Some(&path));
        self.fire_session_event(HostEvent::PlanReady { path: &path });
    }

    /// A run that reaches the queue drain after a normal end, or an ending
    /// that is not approval material.
    pub(super) fn note_plan_turn_end(&mut self, reason: DoneReason) {
        match reason {
            DoneReason::EndTurn => self.plan_last_done = Some(reason),
            DoneReason::Compact => {}
            _ => self.plan_turn_interrupted(),
        }
    }

    pub(super) fn plan_turn_interrupted(&mut self) {
        self.plan_last_done = None;
        self.transition_plan(PlanTrigger::Interrupted);
    }

    pub(crate) fn on_queue_drained(&mut self) {
        if self.plan_last_done.take() == Some(DoneReason::EndTurn) {
            self.transition_plan(PlanTrigger::Settled);
        }
    }

    pub(super) fn enter_plan(&mut self) {
        self.state.plan.allocate_path(&self.storage);
        self.state.mode = Mode::Plan;
    }

    /// The mode switch behind `maki.session.set_mode`, i.e. what Tab does
    /// without the toggling. Build mode is what makes the next prompt an
    /// implementation of the plan instead of another draft.
    pub(crate) fn set_mode(&mut self, mode: Mode) {
        if self.state.mode == mode {
            return;
        }
        self.plan_answers.abandon();
        match mode {
            Mode::Plan => self.enter_plan(),
            Mode::Build => self.state.mode = Mode::Build,
        }
    }

    pub(super) fn toggle_mode(&mut self) -> Vec<super::Action> {
        self.plan_answers.abandon();
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
