use std::fs;
use std::mem;
use std::time::Instant;

use maki_lua::{PlanActionOutcome, PlanMenu, PlanRowAction};
use tokio::sync::oneshot::Receiver;
use tokio::sync::oneshot::error::TryRecvError;

use super::{
    App, FLASH_NO_PLAN, FLASH_PLAN_ACTION_FAILED, FLASH_PLAN_ACTION_LOST, FLASH_PLAN_FORM_SLOW,
    IMPLEMENT_MSG_PREFIX, IMPLEMENT_PARALLEL_HINT, Mode, PLAN_ACTION_ANSWER_WAIT,
    PLAN_FORM_ANSWER_WAIT, PlanState, QueuedMessage,
};
use crate::components::plan_form::{PlanFormAction, builtin_menu, builtin_rows};
use crate::components::{Action, DisplayMessage};
use crate::repaint::Dirty;

/// A plan form row picked by the user, waiting on the plugin handler behind
/// it. `pick` is the form's pick counter at the moment of the press, and an
/// answer that comes back under a different one belongs to a form the user
/// has already left.
pub(crate) struct PlanAction {
    pub(super) pick: u64,
    /// The built-in outcome the row kept, which runs after the handler.
    pub(super) then: Option<PlanRowAction>,
    pub(super) deadline: Instant,
    pub(super) answer: Receiver<PlanActionOutcome>,
}

/// Everything the plan form has in flight, under one owner so that no reset
/// path can retire half of it. A menu chain and a picked row's handler both
/// answer on the Lua thread long after the key press that started them, and
/// the session they were asked about may be gone by then.
#[derive(Default)]
pub(crate) struct PlanAnswers {
    /// In flight answer from the `ui.plan_form*` chains: whether the form
    /// opens for the draft that landed, and with which rows. Given up on
    /// after [`PLAN_FORM_ANSWER_WAIT`].
    pub(super) form: Option<(Instant, Receiver<Option<PlanMenu>>)>,
    /// In flight answer from a picked plugin row.
    pub(super) action: Option<PlanAction>,
    /// Bumped by every pick the plan form makes, and by every path that walks
    /// away from a draft. Without it an outcome from a pick nobody waits on
    /// any more fires a second implement prompt on top of the one already
    /// running, or one against a session the user never picked in.
    pick: u64,
}

impl PlanAnswers {
    /// Retires the answers in flight and the pick they belong to, returning
    /// the pick a new one starts at. The counter only ever moves forward, so
    /// anything stamped with an earlier pick can only miss from here on.
    ///
    /// Every reset path funnels through [`App::reset_ui_chrome`], which calls
    /// this: a new one gets the invariant for free.
    pub(super) fn abandon(&mut self) -> u64 {
        self.form = None;
        self.action = None;
        self.pick += 1;
        self.pick
    }
}

impl App {
    /// Both halves of the plan surface the Lua host answers asynchronously:
    /// the menu a draft opens with, and the built-in outcome a picked plugin
    /// row still owes. Drained for every session, since a plan that lands in
    /// a background tab has to reach its form too.
    pub(crate) fn tick_plan(&mut self) -> Dirty {
        self.tick_plan_form() | self.tick_plan_action()
    }

    /// Open the form with the menu the `ui.plan_form*` chains answered with.
    /// A chain that answers to keep it closed leaves it closed, and the
    /// plan-toggle key still reopens it. A chain that says nothing at all
    /// runs out of [`PLAN_FORM_ANSWER_WAIT`] and leaves the built-in form.
    pub(crate) fn tick_plan_form(&mut self) -> Dirty {
        let Some((deadline, rx)) = self.plan_answers.form.as_mut() else {
            return Dirty::NO;
        };
        let (answered, expired) = (rx.try_recv(), Instant::now() >= *deadline);
        let (menu, slow) = match answered {
            // A layer took the surface over, so the host draws nothing. The
            // menu goes with it, since its rows answer for the draft they
            // were built for.
            Ok(None) => {
                self.plan_answers.abandon();
                self.plan_form.forget_menu();
                return Dirty::from(self.plan_form.is_visible());
            }
            Ok(Some(menu)) => (menu, false),
            // The host went away mid-question, so nobody is drawing the plan.
            Err(TryRecvError::Closed) => (builtin_menu(), false),
            Err(TryRecvError::Empty) if expired => (builtin_menu(), true),
            Err(TryRecvError::Empty) => return Dirty::NO,
        };
        self.plan_answers.abandon();
        self.open_plan_form(menu);
        if slow {
            self.flash(FLASH_PLAN_FORM_SLOW.into());
        }
        Dirty::YES
    }

    /// The built-in outcome a picked plugin row kept, once its handler has
    /// answered.
    ///
    /// A handler that says `false` is gating the row on a check of its own
    /// and stays quiet. A handler that failed, or a pick that reached no
    /// handler, left the user watching the menu vanish for nothing, and says
    /// so.
    pub(super) fn tick_plan_action(&mut self) -> Dirty {
        let Some(pending) = self.plan_answers.action.as_mut() else {
            return Dirty::NO;
        };
        // "Lost" is the host never having taken the pick, which reads
        // differently to the user than a handler that ran and failed.
        let (answered, expired) = (
            pending.answer.try_recv(),
            Instant::now() >= pending.deadline,
        );
        let (outcome, lost) = match answered {
            Ok(outcome) => (outcome, false),
            Err(TryRecvError::Closed) => (PlanActionOutcome::Failed, true),
            Err(TryRecvError::Empty) if expired => (PlanActionOutcome::Failed, true),
            Err(TryRecvError::Empty) => return Dirty::NO,
        };
        let pending = self.plan_answers.action.take().expect("checked above");
        if pending.pick != self.plan_answers.pick {
            // Running the outcome of a pick the user has navigated away from
            // would be a second implement prompt behind the one they asked
            // for, or one against a session they never picked in.
            tracing::debug!(outcome = ?outcome, "dropping the answer of a stale plan form pick");
            return Dirty::NO;
        }
        match outcome {
            PlanActionOutcome::Proceed => {
                let actions = match pending.then {
                    Some(PlanRowAction::Implement) => self.implement_plan(false),
                    Some(PlanRowAction::ClearAndImplement) => self.implement_plan(true),
                    Some(PlanRowAction::Refine) | None => vec![],
                };
                self.pending_actions.extend(actions);
            }
            PlanActionOutcome::Vetoed => {}
            PlanActionOutcome::Failed => self.flash(
                if lost {
                    FLASH_PLAN_ACTION_LOST
                } else {
                    FLASH_PLAN_ACTION_FAILED
                }
                .into(),
            ),
        }
        Dirty::YES
    }

    /// Ask the `ui.plan_form*` chains what to draw for the draft that just
    /// landed, starting from the host's own rows.
    ///
    /// Only asked when a plugin is layering one of them. With no layer the
    /// chain answers with the host's own rows by construction, and the
    /// roundtrip through a request loop that may be busy would cost a stock
    /// install its form.
    pub(super) fn offer_plan_form(&mut self, path: Option<&str>) {
        // A new draft retires the pick the last one was waiting on, handler
        // and built-in outcome both. The handler may well still be running,
        // but its answer is stamped with a pick nobody waits on any more, so
        // the outcome the row promised is gone and the user is told.
        let lost_pick = self.plan_answers.action.is_some();
        let request = self.plan_answers.abandon();
        if lost_pick {
            self.flash(FLASH_PLAN_ACTION_LOST.into());
        }
        let Some(path) = path.filter(|_| self.lua_event_handle.plan_form_layered()) else {
            self.open_plan_form(builtin_menu());
            return;
        };
        // The last draft's menu is not this draft's, and the chain has not
        // answered with one yet.
        self.plan_form.forget_menu();
        let answer = self.lua_event_handle.open_plan_form(
            path.to_owned(),
            self.state.session.id.to_string(),
            request,
            builtin_rows(),
        );
        self.plan_answers.form = Some((Instant::now() + PLAN_FORM_ANSWER_WAIT, answer));
    }

    fn open_plan_form(&mut self, menu: PlanMenu) {
        self.open_prompt(|app| app.plan_form.open_with(menu));
    }

    pub(super) fn handle_plan_form_action(&mut self, action: PlanFormAction) -> Vec<Action> {
        match action {
            PlanFormAction::Consumed | PlanFormAction::Passthrough => vec![],
            PlanFormAction::Hide => {
                self.plan_answers.abandon();
                self.plan_form.hide();
                vec![]
            }
            PlanFormAction::OpenEditor => match self.state.plan.path() {
                Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
                None => {
                    self.flash(FLASH_NO_PLAN.into());
                    vec![]
                }
            },
            PlanFormAction::Implement => {
                self.plan_answers.abandon();
                self.implement_plan(false)
            }
            PlanFormAction::ClearAndImplement => {
                self.plan_answers.abandon();
                self.implement_plan(true)
            }
            PlanFormAction::Plugin {
                row,
                generation,
                then,
            } => {
                // Snapshot the parallel flag before reset() clears it, since
                // the handler is told what it was.
                let parallel = self.plan_form.parallel();
                let path = self
                    .state
                    .plan
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                // Plan state is per session, so the handler is told which one
                // fired instead of assuming the focused tab.
                let session = self.state.session.id.to_string();
                self.plan_form.reset();
                // The generation travels with the pick, so a chain that
                // resumed late and installed its handlers over this menu
                // cannot answer for the rows the user saw.
                let answer = self
                    .lua_event_handle
                    .run_plan_action(session, generation, row, path, parallel);
                let pick = self.plan_answers.abandon();
                self.plan_answers.action = Some(PlanAction {
                    pick,
                    then,
                    deadline: Instant::now() + PLAN_ACTION_ANSWER_WAIT,
                    answer,
                });
                vec![]
            }
        }
    }

    /// Snapshot of the current plan for `maki.plan.read()`. `content` stays
    /// `None` when the plan is not ready or the file cannot be read, which an
    /// empty plan is not.
    pub(crate) fn plan_snapshot(&self) -> serde_json::Value {
        let mode = if self.state.mode == Mode::Plan {
            "plan"
        } else {
            "build"
        };
        let path = self.state.plan.path().map(|p| p.display().to_string());
        let ready = self.state.plan.is_ready();
        let content = if ready {
            self.state
                .plan
                .path()
                .and_then(|p| fs::read_to_string(p).ok())
        } else {
            None
        };
        serde_json::json!({
            "mode": mode,
            "path": path,
            "ready": ready,
            "content": content,
        })
    }

    fn implement_plan(&mut self, clear_context: bool) -> Vec<Action> {
        let parallel = self.plan_form.parallel();
        self.plan_form.reset();
        let plan_snapshot = match mem::take(&mut self.state.plan) {
            PlanState::Ready(p) => Some((
                fs::read_to_string(&p).unwrap_or_default(),
                p.display().to_string(),
            )),
            _ => None,
        };

        self.state.mode = Mode::Build;

        let mut actions = if clear_context {
            self.reset_session()
        } else {
            vec![]
        };

        let text = if let Some((content, path_str)) = plan_snapshot {
            let text = if parallel {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`. {IMPLEMENT_PARALLEL_HINT}")
            } else {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`.")
            };
            self.main_chat()
                .push(DisplayMessage::plan(content, path_str));
            text
        } else {
            format!("{}.", IMPLEMENT_MSG_PREFIX)
        };
        let msg = QueuedMessage {
            text,
            images: vec![],
        };
        actions.extend(self.start_from_queue(&msg));
        actions
    }
}
