//! The bridge from the host's hooks ([`maki_agent::tools::hook`] and
//! [`maki_agent::agent::hook`]) to the slot chains plugins register.
//! Everything here is a hop: the agent asks, the Lua thread answers, and
//! nothing decides anything on the way.

use std::sync::Arc;
use std::time::Instant;

use flume::Sender;
use maki_agent::agent::{AgentCall, AgentHook, AgentSlot};
use maki_agent::tools::hook::{Authority, HookCall, HookStage, ToolHook, Verdict};
use maki_agent::tools::registry::BoxFuture;
use serde_json::{Value, json};

use crate::api::slot::{ANY_TOOL, LayeredTools, host_slot_name};
use crate::runtime::{HookRun, Request};

/// Every `agent.*` slot steers what the agent does next: the prompt it
/// answers, whether it stops, what it remembers after a compaction. No
/// narrower price would be honest.
const AGENT_SLOT_AUTHORITY: Authority = Authority::Unbounded;

pub(crate) struct SlotHook {
    pub(crate) tx: Sender<Request>,
    pub(crate) layered: Arc<LayeredTools>,
}

fn deadline_ms(deadline: Instant) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis() as u64
}

impl SlotHook {
    /// A runtime that is gone, or one that drops the request on the way
    /// down, leaves the value as it found it. A missing opinion is not a
    /// failure of whatever asked for it.
    fn send(&self, run: HookRun) -> BoxFuture<'_, Verdict> {
        let (reply, answer) = flume::bounded(1);
        let request = Request::RunHook { run, reply };
        Box::pin(async move {
            if self.tx.send_async(request).await.is_err() {
                return Verdict::Unchanged;
            }
            answer.recv_async().await.unwrap_or(Verdict::Unchanged)
        })
    }
}

impl ToolHook for SlotHook {
    fn wraps(&self, tool: &str, stage: HookStage) -> bool {
        self.layered.wraps(tool, stage)
    }

    fn run<'a>(
        &'a self,
        stage: HookStage,
        value: Value,
        call: &'a HookCall<'a>,
    ) -> BoxFuture<'a, Verdict> {
        self.send(HookRun {
            slots: vec![
                host_slot_name(call.tool, stage),
                host_slot_name(ANY_TOOL, stage),
            ],
            authority: call.authority,
            cancel: call.cancel.clone(),
            deadline: call.deadline,
            value,
            // The `ctx` table a layer receives.
            call: json!({
                "tool": call.tool,
                "tool_id": call.tool_id,
                "tool_kind": call.tool_kind,
                "input": call.input,
                "session_id": call.session_id,
                "origin": call.origin.as_str(),
                "deadline_ms": deadline_ms(call.deadline),
            }),
            may_ask: stage == HookStage::Input,
        })
    }
}

impl AgentHook for SlotHook {
    fn wraps(&self, slot: AgentSlot) -> bool {
        self.layered.layers_surface(slot.name())
    }

    fn run<'a>(
        &'a self,
        slot: AgentSlot,
        value: Value,
        call: &'a AgentCall<'a>,
    ) -> BoxFuture<'a, Verdict> {
        self.send(HookRun {
            slots: vec![slot.name().to_owned()],
            authority: AGENT_SLOT_AUTHORITY,
            cancel: call.cancel.clone(),
            deadline: call.deadline,
            value,
            call: json!({
                "session_id": call.session_id,
                "task_id": call.task_id,
                "model": call.model,
                "context_size": call.context_size,
                "context_window": call.context_window,
                "deadline_ms": deadline_ms(call.deadline),
            }),
            may_ask: false,
        })
    }
}
