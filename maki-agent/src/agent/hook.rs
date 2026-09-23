//! Slots the agent loop fires itself, at moments only the loop can see: a user
//! message arriving, a run about to stop, a compaction about to start. They
//! keep the contract of [`crate::tools::hook`], so a plugin author only learns
//! one: a table replaces the value, nothing leaves it alone, and `nil, reason`
//! stops where stopping makes sense.

use std::time::{Duration, Instant};

use maki_providers::Model;
use maki_storage::id::SessionRef;
use serde_json::Value;

use crate::cancel::CancelToken;
use crate::tools::ToolRegistry;
use crate::tools::hook::Verdict;
use crate::tools::registry::BoxFuture;

/// A layer may shell out before it decides, but the user is waiting on every
/// one of these. Past this the loop moves on without it.
const AGENT_CHAIN_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum AgentSlot {
    UserMessage,
    /// The model ended its turn without calling a tool.
    Stop,
    CompactBefore,
    /// Picks which tool results the summarizer reads collapsed.
    CompactPrepare,
}

impl AgentSlot {
    pub const fn name(self) -> &'static str {
        match self {
            Self::UserMessage => "agent.user_message",
            Self::Stop => "agent.stop",
            Self::CompactBefore => "agent.compact.before",
            Self::CompactPrepare => "agent.compact.prepare",
        }
    }
}

pub struct AgentCall<'a> {
    pub session_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
    /// Spec of the session's model (`provider/id`), not the summarizer's.
    pub model: &'a str,
    pub context_size: u32,
    pub context_window: u32,
    pub cancel: &'a CancelToken,
    pub deadline: Instant,
}

pub trait AgentHook: Send + Sync + 'static {
    /// Sync and allocation free, because the loop asks before it builds the
    /// value. A slot nobody wrapped then costs a single lookup.
    fn wraps(&self, slot: AgentSlot) -> bool;

    fn run<'a>(
        &'a self,
        slot: AgentSlot,
        value: Value,
        call: &'a AgentCall<'a>,
    ) -> BoxFuture<'a, Verdict>;
}

/// Bundled so the standalone `/compact` path and a normal run fire slots
/// through the same door.
pub struct AgentHooks<'a> {
    pub registry: &'a ToolRegistry,
    pub session_id: Option<&'a SessionRef>,
    pub task_id: Option<&'a str>,
    pub model: &'a Model,
    pub cancel: &'a CancelToken,
    pub context_size: u32,
}

impl AgentHooks<'_> {
    /// `value` is only built when a layer is there to read it. A cancelled or
    /// absent chain answers [`Verdict::Unchanged`], because a layer is an
    /// opinion about the run and never a precondition for it.
    pub async fn fire(&self, slot: AgentSlot, value: impl FnOnce() -> Value) -> Verdict {
        let Some(hook) = self.registry.agent_hook().filter(|h| h.wraps(slot)) else {
            return Verdict::Unchanged;
        };
        let spec = self.model.spec();
        let call = AgentCall {
            session_id: self.session_id.map(SessionRef::as_str),
            task_id: self.task_id,
            model: &spec,
            context_size: self.context_size,
            context_window: self.model.context_window,
            cancel: self.cancel,
            deadline: Instant::now() + AGENT_CHAIN_MAX,
        };
        self.cancel
            .race(hook.run(slot, value(), &call))
            .await
            .unwrap_or(Verdict::Unchanged)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::{Arc, Mutex};

    use serde_json::Value;

    use super::{AgentCall, AgentHook, AgentSlot};
    use crate::tools::ToolRegistry;
    use crate::tools::hook::Verdict;
    use crate::tools::registry::BoxFuture;

    pub(crate) type Seen = Arc<Mutex<Vec<(AgentSlot, Value)>>>;
    type Answer = Box<dyn Fn(AgentSlot, &Value) -> Verdict + Send + Sync>;

    /// Plays the plugin host with one layer on every slot, and remembers what
    /// it was asked.
    struct ScriptedHook {
        answer: Answer,
        seen: Seen,
    }

    impl AgentHook for ScriptedHook {
        fn wraps(&self, _: AgentSlot) -> bool {
            true
        }

        fn run<'a>(
            &'a self,
            slot: AgentSlot,
            value: Value,
            _: &'a AgentCall<'a>,
        ) -> BoxFuture<'a, Verdict> {
            let verdict = (self.answer)(slot, &value);
            self.seen.lock().unwrap().push((slot, value));
            Box::pin(std::future::ready(verdict))
        }
    }

    pub(crate) fn script(
        registry: &ToolRegistry,
        answer: impl Fn(AgentSlot, &Value) -> Verdict + Send + Sync + 'static,
    ) -> Seen {
        let seen = Seen::default();
        registry.set_agent_hook(ScriptedHook {
            answer: Box::new(answer),
            seen: Arc::clone(&seen),
        });
        seen
    }
}
