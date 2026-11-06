//! Everything outside a subagent's running prompt talks to it through its
//! `SessionControl`. The tab and the parent queue messages here, the registry
//! asks what it is doing, and a stop ends it. When this lived in several
//! places, a message could slip in between a stop and its cleanup and get lost.
//!
//! The session's cancel token is the only signal that it ended. [`end`] writes
//! down why before it fires the token. So a token that fired with nothing
//! written down was cancelled from above, by whatever opened the session, and
//! reads as [`EndReason::Cancelled`].
//!
//! [`end`]: SessionControl::end

use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use arc_swap::ArcSwap;
use event_listener::Event;
use thiserror::Error;

use crate::cancel::{CancelHandle, CancelToken, CancelTrigger};
use crate::{
    AgentEvent, AgentInput, Cutter, EarlierInput, EventSender, ExtractedCommand, HistorySnapshot,
    InterruptSource, SharedMessages,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Read at the next turn boundary. A prompt waiting on moved calls wakes
    /// for it right away.
    NextTurn,
    /// Also cuts a running turn short, so the next one starts with it.
    /// Otherwise the same as [`Delivery::NextTurn`].
    Interrupt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum EndReason {
    #[error("session closed")]
    Closed,
    #[error("stopped by {0}")]
    Stopped(Cutter),
    /// What opened the session ended.
    #[error("cancelled")]
    Cancelled,
}

impl EndReason {
    /// Only a recorded stop or a plugin's close names the parent. A cancel
    /// from above records nothing, and it is nearly always the user's: Esc on
    /// the subagent's tab or on the run that opened it. Naming the parent
    /// there tells the parent model it cut work the user stopped.
    fn cutter(self) -> Cutter {
        match self {
            Self::Stopped(by) => by,
            Self::Closed => Cutter::Parent,
            Self::Cancelled => Cutter::User,
        }
    }
}

/// What a prompt of the session is doing. Whether the session ended is
/// [`SessionControl::ended`], not this: a prompt cut by an end is still
/// unwinding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Activity {
    #[default]
    Idle,
    Running,
    /// Its answer waits on calls it moved to the background.
    Waiting,
}

/// Every run shares one turn token until an interrupt fires it. Work a tool
/// handler leaves running after its reply should not die just because the
/// next run started.
struct Turn {
    trigger: CancelTrigger,
    cut_by: Option<Cutter>,
}

impl Turn {
    fn under(session: &CancelTrigger) -> Self {
        let (trigger, _) = session.token().child();
        Self {
            trigger,
            cut_by: None,
        }
    }
}

struct ControlState {
    inbox: VecDeque<AgentInput>,
    activity: Activity,
    /// Only what [`SessionControl::end`] wrote down. Read through
    /// [`SessionControl::end_reason`], which also sees a cancel from above.
    ended: Option<EndReason>,
    turn: Turn,
}

pub struct SessionControl {
    state: Mutex<ControlState>,
    pushed: Event,
    cancel: CancelTrigger,
    transcript: SharedMessages,
}

impl Default for SessionControl {
    fn default() -> Self {
        Self::new(&CancelToken::none())
    }
}

impl SessionControl {
    /// A session whose token also fires with {parent}'s.
    pub fn new(parent: &CancelToken) -> Self {
        let (cancel, _) = parent.child();
        Self {
            state: Mutex::new(ControlState {
                inbox: VecDeque::new(),
                activity: Activity::Idle,
                ended: None,
                turn: Turn::under(&cancel),
            }),
            pushed: Event::new(),
            cancel,
            transcript: Arc::new(ArcSwap::from_pointee(HistorySnapshot::default())),
        }
    }

    /// Checked under the lock the push takes, so a message is either queued
    /// or refused, never lost to an end landing in between.
    pub fn send(&self, input: AgentInput, delivery: Delivery) -> Result<(), EndReason> {
        let mut state = self.lock();
        if let Some(reason) = self.end_reason(&state) {
            return Err(reason);
        }
        let cutter = Cutter::of(input.source);
        state.inbox.push_back(input);
        if delivery == Delivery::Interrupt && state.activity == Activity::Running {
            state.turn.cut_by.get_or_insert(cutter);
            state.turn.trigger.handle().fire();
        }
        drop(state);
        self.pushed.notify(usize::MAX);
        Ok(())
    }

    /// The first reason sticks, whoever ends the session after it.
    pub fn end(&self, reason: EndReason) {
        let mut state = self.lock();
        if self.end_reason(&state).is_none() {
            state.ended = Some(reason);
        }
        drop(state);
        self.cancel.handle().fire();
    }

    pub fn ended(&self) -> Option<EndReason> {
        self.end_reason(&self.lock())
    }

    pub fn activity(&self) -> Activity {
        self.lock().activity
    }

    pub fn token(&self) -> CancelToken {
        self.cancel.token()
    }

    /// Fires the session's token without owning it, which reads as
    /// [`EndReason::Cancelled`].
    pub fn handle(&self) -> CancelHandle {
        self.cancel.handle()
    }

    /// The session's history mirrors into it, so the parent can read the
    /// transcript while a run holds the session lock.
    pub fn transcript(&self) -> &SharedMessages {
        &self.transcript
    }

    /// What was sent while no run read the inbox, shown as consumed on
    /// {event_tx} and handed to the next run ahead of its own message.
    pub fn take_waiting(&self, event_tx: &EventSender) -> Vec<EarlierInput> {
        let inputs = mem::take(&mut self.lock().inbox);
        inputs
            .into_iter()
            .map(|input| {
                let _ = event_tx.send(AgentEvent::QueueItemConsumed {
                    text: input.message.clone(),
                    images: input.images.clone(),
                });
                EarlierInput::from(input)
            })
            .collect()
    }

    pub fn remove(&self, index: usize) -> Option<AgentInput> {
        self.lock().inbox.remove(index)
    }

    pub fn len(&self) -> usize {
        self.lock().inbox.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().inbox.is_empty()
    }

    pub fn texts(&self) -> Vec<String> {
        self.lock()
            .inbox
            .iter()
            .map(|input| input.message.clone())
            .collect()
    }

    /// Marks the session running and hands out its turn's token, a fresh one
    /// once an interrupt fired the last.
    pub(crate) fn enter(&self) -> CancelToken {
        let mut state = self.lock();
        state.activity = Activity::Running;
        if state.turn.trigger.is_cancelled() {
            state.turn = Turn::under(&self.cancel);
        }
        state.turn.trigger.token()
    }

    pub(crate) fn set_activity(&self, activity: Activity) {
        self.lock().activity = activity;
    }

    pub(crate) async fn wait_nonempty(&self) {
        loop {
            let listener = self.pushed.listen();
            if !self.is_empty() {
                return;
            }
            listener.await;
        }
    }

    fn end_reason(&self, state: &ControlState) -> Option<EndReason> {
        state
            .ended
            .or_else(|| self.cancel.is_cancelled().then_some(EndReason::Cancelled))
    }

    fn lock(&self) -> MutexGuard<'_, ControlState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl InterruptSource for SessionControl {
    /// A cut turn leaves the messages for the run after it. Drained here they
    /// would land ahead of the cancel marker, and the run that picks up after
    /// the interrupt would start with nothing to read.
    fn poll(&self) -> Option<ExtractedCommand> {
        let mut state = self.lock();
        if state.turn.trigger.is_cancelled() {
            return None;
        }
        let inputs: Vec<_> = state.inbox.drain(..).collect();
        (!inputs.is_empty()).then_some(ExtractedCommand::Interrupt(inputs))
    }

    fn cut_by(&self) -> Cutter {
        let state = self.lock();
        state
            .turn
            .cut_by
            .or_else(|| self.end_reason(&state).map(EndReason::cutter))
            .unwrap_or_default()
    }
}

impl fmt::Debug for SessionControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.lock();
        f.debug_struct("SessionControl")
            .field("activity", &state.activity)
            .field("ended", &self.end_reason(&state))
            .field("queued", &state.inbox.len())
            .finish()
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use maki_providers::ThinkingConfig;

    use crate::{AgentInput, AgentMode, InputSource};

    pub(crate) fn input(message: &str, source: InputSource) -> AgentInput {
        AgentInput {
            message: message.into(),
            mode: AgentMode::Build,
            images: Vec::new(),
            preamble: Vec::new(),
            earlier: Vec::new(),
            thinking: ThinkingConfig::default(),
            fast: false,
            workflow: false,
            prompt: None,
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::testing::input;
    use super::*;
    use crate::InputSource;

    const FIRST: &str = "a";
    const SECOND: &str = "b";
    const KEPT_FOR_RESUME: &str = "a cut turn must leave the message for the run after it";

    fn queue(control: &SessionControl, message: &str) -> Result<(), EndReason> {
        control.send(input(message, InputSource::Tui), Delivery::NextTurn)
    }

    #[test]
    fn poll_drains_the_burst_in_order() {
        let control = SessionControl::default();
        queue(&control, FIRST).unwrap();
        queue(&control, SECOND).unwrap();

        let Some(ExtractedCommand::Interrupt(inputs)) = control.poll() else {
            panic!("a non-empty queue polls as one interrupt");
        };
        let texts: Vec<_> = inputs.into_iter().map(|i| i.message).collect();
        assert_eq!(texts, [FIRST, SECOND]);
        assert!(control.poll().is_none());
    }

    /// Firing a turn nobody runs would kill what tools left running past the
    /// last reply. A waiting prompt wakes for the message anyway.
    #[test_case(Activity::Running, true  ; "running")]
    #[test_case(Activity::Waiting, false ; "waiting_on_moved_calls")]
    #[test_case(Activity::Idle,    false ; "idle")]
    fn an_interrupt_cuts_only_a_running_turn(activity: Activity, cuts: bool) {
        let control = SessionControl::default();
        let turn = control.enter();
        control.set_activity(activity);

        control
            .send(input(FIRST, InputSource::Plugin), Delivery::Interrupt)
            .unwrap();

        assert_eq!(turn.is_cancelled(), cuts);
        assert!(!control.token().is_cancelled());
        assert_eq!(control.texts(), [FIRST]);
        if cuts {
            assert!(control.poll().is_none(), "{KEPT_FOR_RESUME}");
        }
        assert!(!control.enter().is_cancelled());
        assert!(control.poll().is_some());
    }

    #[test_case(EndReason::Closed, EndReason::Stopped(Cutter::User) ; "closed_then_stopped")]
    #[test_case(EndReason::Stopped(Cutter::Parent), EndReason::Closed ; "stopped_then_closed")]
    fn the_first_end_reason_sticks_and_refuses_every_send(first: EndReason, second: EndReason) {
        let control = SessionControl::default();
        control.end(first);
        control.end(second);

        assert!(control.token().is_cancelled());
        assert_eq!(control.ended(), Some(first));
        assert_eq!(queue(&control, FIRST), Err(first));
        assert!(control.is_empty());
    }

    #[test]
    fn a_parent_cancel_reads_as_cancelled() {
        let (parent_trigger, parent) = CancelToken::new();
        let control = SessionControl::new(&parent);
        parent_trigger.cancel();
        control.end(EndReason::Closed);

        assert_eq!(control.ended(), Some(EndReason::Cancelled));
        assert_eq!(queue(&control, FIRST), Err(EndReason::Cancelled));
    }
}
