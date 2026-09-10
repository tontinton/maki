//! The one shape `maki.session.read()` returns. The UI and the headless
//! drivers all serialize this struct, so a plugin sees the same keys wherever
//! it runs and a new field is one edit instead of three `json!` blobs.

use std::sync::{Arc, Mutex, MutexGuard};

use maki_agent::{AgentEvent, Envelope};
use maki_providers::{TokenUsage, add_cost};
use serde::Serialize;

use crate::EventHandle;

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";
pub const STATUS_IDLE: &str = "idle";
pub const STATUS_WORKING: &str = "working";
pub const STATUS_NEEDS_INPUT: &str = "needs_input";

#[derive(Serialize)]
pub struct SessionSnapshot {
    pub id: String,
    /// Working directory the session runs against.
    pub cwd: String,
    /// Left out when the host has no title to give. Never filled with the
    /// session id, so a plugin can tell "no title" from a real one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Model spec as shown in the status bar (`provider/id`).
    pub model: String,
    /// [`MODE_BUILD`] or [`MODE_PLAN`].
    pub mode: &'static str,
    /// [`STATUS_IDLE`], [`STATUS_WORKING`], or [`STATUS_NEEDS_INPUT`].
    pub status: &'static str,
    /// Whether this is the session the user is looking at. Always true
    /// for the single-session headless drivers.
    pub focused: bool,
    /// Unix seconds.
    pub updated_at: u64,
    /// Omitted by hosts that hold no queue of their own. An sdk client can
    /// pipeline messages that wait outside the driver's sight, so leaving
    /// this out beats reporting a zero the plugin would trust.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<SessionQueueSnapshot>,
    /// Subagent turns included, their tokens are real spend on this session
    /// too.
    pub usage: TokenUsage,
    /// Main session only. A subagent fills its own window, not ours.
    pub context_size: u32,
    pub context_window: u32,
    /// What the session bills, `None` on an unpriced model. No list price
    /// twin here, because `cost` is re-settled from stored usage when a
    /// session resumes and list price is not stored. Per turn list price
    /// lives on the `TurnEnd` autocmd instead.
    pub cost: Option<f64>,
}

#[derive(Serialize)]
pub struct SessionQueueSnapshot {
    /// Pending user text only. A queued `/compact` is housekeeping, not human
    /// input a supervisor should yield to.
    pub count: usize,
}

/// What a headless driver knows about its session before it runs.
pub struct HeadlessMeta {
    pub id: String,
    pub cwd: String,
    /// Model spec (`provider/id`).
    pub model: String,
}

#[derive(Default)]
struct Totals {
    usage: TokenUsage,
    cost: Option<f64>,
    context_size: u32,
    context_window: u32,
    working: bool,
}

/// A poisoned lock only means some other run panicked mid-update. The
/// numbers are still whole, so read them instead of losing the session's
/// whole accounting over it.
fn lock(totals: &Mutex<Totals>) -> MutexGuard<'_, Totals> {
    totals.lock().unwrap_or_else(|e| e.into_inner())
}

/// Backs `maki.session.read` for the single session headless drivers
/// (`maki -p`, sdk mode). The driver writes while the Lua thread reads, hence
/// the lock, though the two rarely meet.
#[derive(Clone, Default)]
pub struct HeadlessSnapshot(Arc<Mutex<Totals>>);

impl HeadlessSnapshot {
    /// Fold one envelope in. The drivers already loop over the stream, so
    /// they hand every envelope here and keep no bookkeeping of their own.
    pub fn observe(&self, envelope: &Envelope) {
        let event = &envelope.event;
        let mut totals = lock(&self.0);
        // Envelopes only flow while a run is alive, so anything that is not
        // an ending means the agent is busy. Sdk mode runs many turns and no
        // driver has to remember to clear a flag between them.
        totals.working = !matches!(event, AgentEvent::Done { .. } | AgentEvent::Error { .. });
        match event {
            AgentEvent::Done {
                usage,
                cost,
                context_size,
                context_window,
                ..
            } => {
                // Each turn's `Done` carries that turn's whole bill, subagents
                // and compaction included, so summing them lands on the exact
                // session total. Summing `TurnComplete` would miss compaction.
                totals.usage += *usage;
                add_cost(&mut totals.cost, *cost);
                totals.context_size = *context_size;
                totals.context_window = *context_window;
            }
            AgentEvent::TurnComplete(turn) if envelope.subagent.is_none() => {
                totals.context_size = turn.context_size.unwrap_or(totals.context_size);
                totals.context_window = turn.context_window;
            }
            _ => {}
        }
    }

    /// Install as the provider `maki.session.read` answers from. `mode`
    /// is a callback because sdk mode can flip between build and plan
    /// mid-run.
    ///
    /// There is no stored session and no rename path under either driver,
    /// so `title` stays nil rather than echoing the id back as a fake one.
    pub fn install(
        &self,
        handle: &EventHandle,
        meta: HeadlessMeta,
        mode: impl Fn() -> &'static str + Send + Sync + 'static,
    ) {
        let totals = Arc::clone(&self.0);
        handle.install_session_snapshot(Box::new(move |id| {
            // One session here, so any other id names a tab that is not ours.
            if let Some(want) = id
                && want != meta.id
            {
                return Err(format!("session {want} not live"));
            }
            let mode = mode();
            let totals = lock(&totals);
            serde_json::to_value(SessionSnapshot {
                id: meta.id.clone(),
                cwd: meta.cwd.clone(),
                title: None,
                model: meta.model.clone(),
                mode,
                status: if totals.working {
                    STATUS_WORKING
                } else {
                    STATUS_IDLE
                },
                focused: true,
                updated_at: maki_storage::now_epoch(),
                queue: None,
                usage: totals.usage,
                context_size: totals.context_size,
                context_window: totals.context_window,
                cost: totals.cost,
            })
            .map_err(|e| e.to_string())
        }));
    }
}

#[cfg(test)]
mod tests {
    use maki_agent::{DoneReason, TurnCompleteEvent};
    use maki_providers::Message;

    use super::*;

    const FIRST_TURN_INPUT: u32 = 1_000;
    const SECOND_TURN_INPUT: u32 = 400;
    const TURN_COST: f64 = 0.25;
    const WINDOW: u32 = 200_000;
    const GROWS_MSG: &str = "session totals add up across turns, they never restart at one turn";
    const WORKING_MSG: &str = "a second turn has to read as working again";

    fn envelope(event: AgentEvent) -> Envelope {
        Envelope {
            event,
            subagent: None,
            run_id: 0,
        }
    }

    /// Carries no usage or cost of its own, so a fold that summed these instead
    /// of `Done` would leave the totals empty and fail below.
    fn turn_complete(context_size: u32) -> Envelope {
        envelope(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
            message: Message::user("go".into()),
            usage: TokenUsage::default(),
            model: String::new(),
            cost: None,
            list_cost: None,
            context_size: Some(context_size),
            context_window: WINDOW,
        })))
    }

    fn done(input: u32) -> Envelope {
        envelope(AgentEvent::Done {
            usage: TokenUsage {
                input,
                ..Default::default()
            },
            cost: Some(TURN_COST),
            list_cost: None,
            context_size: input,
            context_window: WINDOW,
            num_turns: 1,
            reason: DoneReason::EndTurn,
        })
    }

    /// Sdk mode serves many turns on one session, so `Done` is a turn boundary
    /// and not the end. Totals have to keep climbing and the status has to come
    /// back to life on turn two, with no driver clearing anything by hand.
    #[test]
    fn totals_grow_and_status_revives_across_turns() {
        let snapshot = HeadlessSnapshot::default();

        snapshot.observe(&turn_complete(FIRST_TURN_INPUT));
        assert!(lock(&snapshot.0).working);
        snapshot.observe(&done(FIRST_TURN_INPUT));
        assert!(!lock(&snapshot.0).working);

        snapshot.observe(&turn_complete(SECOND_TURN_INPUT));
        assert!(lock(&snapshot.0).working, "{WORKING_MSG}");
        snapshot.observe(&done(SECOND_TURN_INPUT));

        let totals = lock(&snapshot.0);
        assert!(!totals.working);
        assert_eq!(
            totals.usage.input,
            FIRST_TURN_INPUT + SECOND_TURN_INPUT,
            "{GROWS_MSG}"
        );
        assert_eq!(totals.cost, Some(TURN_COST * 2.0), "{GROWS_MSG}");
        assert_eq!(totals.context_size, SECOND_TURN_INPUT);
    }
}
