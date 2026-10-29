//! Messages a user queues for a subagent while it runs. The host pushes,
//! the subagent's loop drains them between turns as one `<user-interrupt>`.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::{AgentInput, ExtractedCommand, InterruptSource};

#[derive(Default)]
pub struct SubagentInbox {
    items: Mutex<VecDeque<AgentInput>>,
}

impl SubagentInbox {
    pub fn push(&self, input: AgentInput) {
        self.lock().push_back(input);
    }

    pub fn remove(&self, index: usize) -> Option<AgentInput> {
        self.lock().remove(index)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    pub fn drain(&self) -> Vec<AgentInput> {
        self.lock().drain(..).collect()
    }

    pub fn texts(&self) -> Vec<String> {
        self.lock()
            .iter()
            .map(|input| input.message.clone())
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<AgentInput>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl InterruptSource for SubagentInbox {
    fn poll(&self) -> Option<ExtractedCommand> {
        let inputs = self.drain();
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
    fn remove_takes_one_entry_by_position() {
        let queue = SubagentInbox::default();
        queue.push(input("a"));
        queue.push(input("b"));
        assert_eq!(queue.remove(0).map(|i| i.message).as_deref(), Some("a"));
        assert_eq!(queue.texts(), ["b"]);
        assert!(queue.remove(5).is_none());
    }
}
