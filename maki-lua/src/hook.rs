//! The bridge between [`maki_agent::tools::hook`] and the slot chains plugins
//! register. Everything here is a hop: dispatch asks, the Lua thread answers,
//! and nothing decides anything on the way.

use std::sync::Arc;

use flume::Sender;
use maki_agent::tools::hook::{HookCall, HookStage, ToolHook, Verdict};
use maki_agent::tools::registry::BoxFuture;
use serde_json::{Value, json};

use crate::api::slot::{LayeredTools, host_slot_name};
use crate::runtime::{HookRun, Request};

pub(crate) struct SlotHook {
    pub(crate) tx: Sender<Request>,
    pub(crate) layered: Arc<LayeredTools>,
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
        let (reply, answer) = flume::bounded(1);
        let request = Request::RunHook {
            run: HookRun {
                slot: host_slot_name(call.tool, stage),
                authority: call.authority,
                cancel: call.cancel.clone(),
                deadline: call.deadline,
                value,
                // The `ctx` table a layer receives.
                call: json!({
                    "tool": call.tool,
                    "tool_id": call.tool_id,
                    "session_id": call.session_id,
                    "origin": call.origin.as_str(),
                }),
            },
            reply,
        };
        // A runtime that is gone, or one that drops the request on the way
        // down, leaves the call as it found it. A missing opinion about a
        // call is not a failure of that call.
        Box::pin(async move {
            if self.tx.send_async(request).await.is_err() {
                return Verdict::Unchanged;
            }
            answer.recv_async().await.unwrap_or(Verdict::Unchanged)
        })
    }
}
