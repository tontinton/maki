//! Slots the agent loop fires itself, at moments only the loop can see: a user
//! message arriving, a run about to stop, a compaction about to start. They
//! keep the contract of [`crate::tools::hook`], so a plugin author only learns
//! one: a table replaces the value, nothing leaves it alone, and `nil, reason`
//! stops where stopping makes sense.
//!
//! The `agent.message.*` slots are monitors instead. The loop fires them, reads
//! nothing back, and hands over a frozen value, so a monitor watches a request
//! or a reply without any way to change it.

use std::time::{Duration, Instant};

use maki_providers::Model;
use maki_storage::id::SessionRef;
use serde_json::{Value, json};
use strum::EnumIter;

use crate::cancel::CancelToken;
use crate::tools::ToolRegistry;
use crate::tools::hook::{Verdict, bounded};
use crate::tools::registry::{BoxFuture, InstalledAgentHook};

/// A layer may shell out before it decides, but the user is waiting on every
/// one of these. Past this the loop moves on without it.
const AGENT_CHAIN_MAX: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, EnumIter)]
pub enum AgentSlot {
    UserMessage,
    /// The model ended its turn without calling a tool.
    Stop,
    CompactBefore,
    /// Picks which tool results the summarizer reads collapsed.
    CompactPrepare,
    /// The request about to reach the provider. A monitor reads the messages,
    /// system and tools. Fired once per logical request, before the retry loop,
    /// so a retry never re-asks it.
    MessageSend,
    /// The assistant message a stream produced, once assembled. A monitor reads
    /// what the model said.
    MessageReceive,
    /// Each text or thinking delta as it arrives. Hot: the loop only asks when a
    /// monitor is registered.
    MessageDelta,
}

impl AgentSlot {
    pub const fn name(self) -> &'static str {
        match self {
            Self::UserMessage => "agent.user_message",
            Self::Stop => "agent.stop",
            Self::CompactBefore => "agent.compact.before",
            Self::CompactPrepare => "agent.compact.prepare",
            Self::MessageSend => "agent.message.send",
            Self::MessageReceive => "agent.message.receive",
            Self::MessageDelta => "agent.message.delta",
        }
    }

    /// What the slot's layers may do with the value. The loop fires a
    /// `Steering` slot and honours its answer, or a `Monitor` slot and ignores
    /// it, handing over a frozen value.
    pub const fn role(self) -> AgentSlotRole {
        match self {
            Self::MessageSend | Self::MessageReceive | Self::MessageDelta => AgentSlotRole::Monitor,
            _ => AgentSlotRole::Steering,
        }
    }
}

/// Whether an [`AgentSlot`] steers the run or only watches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum AgentSlotRole {
    /// A rewrite is honoured, and where it makes sense a `nil, reason` stops
    /// the run.
    Steering,
    /// A monitor. The loop reads nothing back and treats a replacement as a bug
    /// rather than a request.
    Monitor,
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

    /// How long past a chain's deadline this host may still answer, see
    /// [`bounded`].
    fn answer_grace(&self) -> Duration;

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
    pub fn wraps(&self, slot: AgentSlot) -> bool {
        self.registry.agent_hook().is_some_and(|h| h.wraps(slot))
    }

    /// `value` is only built when a layer is there to read it. A cancelled,
    /// lapsed or absent chain answers [`Verdict::Unchanged`], because a layer
    /// is an opinion about the run and never a precondition for it.
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
        let chain = hook.run(slot, value(), &call);
        bounded(self.cancel, call.deadline, hook.answer_grace(), chain)
            .await
            .unwrap_or(Verdict::Unchanged)
    }

    /// An owned firer for a monitor slot, which outlives every borrow of the
    /// agent. `None` unless that monitor is registered, so an unwatched slot
    /// never pays for one.
    pub fn monitor_firer(&self, slot: AgentSlot) -> Option<MonitorFirer> {
        self.registry
            .agent_hook()
            .filter(|h| h.wraps(slot))
            .map(|hook| MonitorFirer {
                hook,
                slot,
                session_id: self.session_id.map(SessionRef::as_str).map(str::to_owned),
                task_id: self.task_id.map(str::to_owned),
                model: self.model.spec(),
                context_size: self.context_size,
                context_window: self.model.context_window,
                cancel: self.cancel.clone(),
            })
    }
}

/// Fires one `agent.message.*` monitor. It owns the context it needs because it
/// runs detached from the loop that built it: from the stream task for deltas,
/// or from a spawn of its own for send and receive. Only the value is borrowed
/// from the caller, and only long enough to build it.
#[derive(Clone)]
pub struct MonitorFirer {
    hook: InstalledAgentHook,
    slot: AgentSlot,
    session_id: Option<String>,
    task_id: Option<String>,
    model: String,
    context_size: u32,
    context_window: u32,
    cancel: CancelToken,
}

impl MonitorFirer {
    /// Fire and wait. The delta consumer awaits this so one stream's deltas
    /// stay in order; that task is already detached, so nothing user-facing
    /// waits here.
    pub async fn fire(&self, value: Value, deadline: Instant) {
        let call = AgentCall {
            session_id: self.session_id.as_deref(),
            task_id: self.task_id.as_deref(),
            model: &self.model,
            context_size: self.context_size,
            context_window: self.context_window,
            cancel: &self.cancel,
            deadline,
        };
        let chain = self.hook.run(self.slot, value, &call);
        // Observe only: whatever a monitor answers, the run keeps going.
        let _ = bounded(&self.cancel, call.deadline, self.hook.answer_grace(), chain).await;
    }

    /// One streamed delta, in the shape a delta monitor reads.
    pub async fn fire_delta(&self, kind: &'static str, text: &str, deadline: Instant) {
        self.fire(
            json!({
                "kind": kind,
                "text": text,
                "session_id": self.session_id.as_deref(),
                "task_id": self.task_id.as_deref(),
            }),
            deadline,
        )
        .await;
    }

    /// Fire and return at once, so a slow monitor never stalls the turn. The
    /// value is already frozen on the Lua side, and the answer is dropped
    /// there, so nothing here has to wait for it.
    pub fn spawn(&self, value: Value, deadline: Instant) {
        let firer = self.clone();
        tokio::spawn(async move { firer.fire(value, deadline).await });
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::ops::Deref;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::Value;
    use tokio::sync::Notify;

    use super::{AgentCall, AgentHook, AgentSlot};
    use crate::tools::ToolRegistry;
    use crate::tools::hook::Verdict;
    use crate::tools::registry::BoxFuture;

    /// What a scripted hook was asked, and a wake for each call. Steering slots
    /// fire on the run's own task, so a test that awaits the run has already
    /// seen them; a monitor fires detached, so a test awaits [`Seen::settle`]
    /// first. `seen.lock()` still reads the log directly.
    #[derive(Default)]
    pub(crate) struct Seen {
        log: Arc<Mutex<Vec<(AgentSlot, Value)>>>,
        landed: Arc<Notify>,
    }

    impl Deref for Seen {
        type Target = Mutex<Vec<(AgentSlot, Value)>>;

        fn deref(&self) -> &Self::Target {
            &self.log
        }
    }

    impl Seen {
        /// The log a clone shares, so the hook can push while the test reads.
        fn share(&self) -> Self {
            Self {
                log: Arc::clone(&self.log),
                landed: Arc::clone(&self.landed),
            }
        }

        /// Wait until at least `count` calls to `slot` have been logged. A
        /// detached firing registers with the hook here, so a test that asserts
        /// on it cannot race the spawn. The future is built before the check
        /// because `notify_waiters` wakes only the waiters that already exist.
        pub(crate) async fn settle(&self, slot: AgentSlot, count: usize) {
            let fired = || {
                self.log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(seen, _)| *seen == slot)
                    .count()
            };
            while fired() < count {
                let landed = self.landed.notified();
                if fired() >= count {
                    break;
                }
                landed.await;
            }
        }
    }

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

        fn answer_grace(&self) -> Duration {
            Duration::ZERO
        }

        fn run<'a>(
            &'a self,
            slot: AgentSlot,
            value: Value,
            _: &'a AgentCall<'a>,
        ) -> BoxFuture<'a, Verdict> {
            let verdict = (self.answer)(slot, &value);
            self.seen.log.lock().unwrap().push((slot, value));
            self.seen.landed.notify_waiters();
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
            seen: seen.share(),
        });
        seen
    }
}
