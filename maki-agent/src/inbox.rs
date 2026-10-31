//! Messages queued for a subagent while it runs, typed in its tab or sent by
//! a plugin. The host pushes, the subagent's loop drains them between turns,
//! each as its own message in one burst.
//! An interrupting push also cuts the current run short, so the next run
//! starts with the message instead of waiting for the turn to end.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use event_listener::Event;

use crate::cancel::{CancelToken, CancelTrigger};
use crate::{AgentInput, ExtractedCommand, InterruptSource};

#[derive(Default)]
pub struct SubagentInbox {
    state: Mutex<State>,
    pushed: Event,
}

#[derive(Default)]
struct State {
    items: VecDeque<AgentInput>,
    run: Option<(CancelTrigger, CancelToken)>,
}

impl SubagentInbox {
    pub fn push(&self, input: AgentInput) {
        self.lock().items.push_back(input);
        self.pushed.notify(usize::MAX);
    }

    /// Resolves once something is queued, for a session idle between runs.
    pub async fn wait_nonempty(&self) {
        loop {
            let listener = self.pushed.listen();
            if !self.is_empty() {
                return;
            }
            listener.await;
        }
    }

    /// Queues {input} and cancels the run going now, if any.
    pub fn interrupt(&self, input: AgentInput) {
        let mut state = self.lock();
        state.items.push_back(input);
        if let Some((trigger, _)) = &state.run {
            trigger.handle().fire();
        }
        drop(state);
        self.pushed.notify(usize::MAX);
    }

    /// The cancel token for a run, a child of {session}. Runs share one until
    /// an interrupt fires it, so what a tool handler leaves running past its
    /// reply is not cut off just because the next run started.
    pub fn run_cancel(&self, session: &CancelToken) -> CancelToken {
        let mut state = self.lock();
        if let Some((trigger, token)) = &state.run
            && !trigger.is_cancelled()
        {
            return token.clone();
        }
        let (trigger, token) = session.child();
        state.run = Some((trigger, token.clone()));
        token
    }

    pub fn remove(&self, index: usize) -> Option<AgentInput> {
        self.lock().items.remove(index)
    }

    pub fn len(&self) -> usize {
        self.lock().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().items.is_empty()
    }

    pub fn drain(&self) -> Vec<AgentInput> {
        self.lock().items.drain(..).collect()
    }

    pub fn texts(&self) -> Vec<String> {
        self.lock()
            .items
            .iter()
            .map(|input| input.message.clone())
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl InterruptSource for SubagentInbox {
    /// A cancelled run must leave the messages for the run after it. Drained
    /// here they would land ahead of the cancel marker, and the run that
    /// follows the interrupt would start with nothing to read.
    fn poll(&self) -> Option<ExtractedCommand> {
        let mut state = self.lock();
        if state
            .run
            .as_ref()
            .is_some_and(|(trigger, _)| trigger.is_cancelled())
        {
            return None;
        }
        let inputs: Vec<_> = state.items.drain(..).collect();
        (!inputs.is_empty()).then_some(ExtractedCommand::Interrupt(inputs))
    }
}

impl fmt::Debug for SubagentInbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubagentInbox")
            .field("len", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentMode, InputSource};
    use maki_providers::ThinkingConfig;

    fn input(message: &str) -> AgentInput {
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
            source: InputSource::Tui,
        }
    }

    #[test]
    fn poll_drains_the_burst_in_order_and_empties_the_queue() {
        let queue = SubagentInbox::default();
        assert!(queue.poll().is_none());
        queue.push(input("a"));
        queue.push(input("b"));
        assert_eq!(queue.texts(), ["a", "b"]);

        let Some(ExtractedCommand::Interrupt(inputs)) = queue.poll() else {
            panic!("a non-empty queue polls as one interrupt");
        };
        let texts: Vec<_> = inputs.into_iter().map(|i| i.message).collect();
        assert_eq!(texts, ["a", "b"]);
        assert!(queue.is_empty());
        assert!(queue.poll().is_none());
    }

    #[test]
    fn runs_share_a_token_until_an_interrupt_cancels_it_and_keeps_the_message() {
        let queue = SubagentInbox::default();
        let (_session_trigger, session) = CancelToken::new();
        let run = queue.run_cancel(&session);
        assert!(queue.run_cancel(&session).same_as(&run));
        queue.interrupt(input("a"));
        assert!(run.is_cancelled());
        assert!(!session.is_cancelled());
        assert!(queue.poll().is_none());
        assert_eq!(queue.texts(), ["a"]);

        assert!(!queue.run_cancel(&session).is_cancelled());
        assert!(queue.poll().is_some());
    }

    #[test]
    fn remove_takes_one_entry_by_position() {
        let queue = SubagentInbox::default();
        queue.push(input("a"));
        queue.push(input("b"));
        assert_eq!(queue.remove(0).map(|i| i.message).as_deref(), Some("a"));
        assert_eq!(queue.texts(), ["b"]);
        assert!(queue.remove(5).is_none());
    }
}
